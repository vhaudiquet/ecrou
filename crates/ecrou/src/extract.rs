use crate::progress::{Progress, Unit};
use anyhow::Context;

use crate::Result;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// Extract a tarball to the specified directory
pub fn extract_tarball(tarball: &Path, dest: &Path, progress: &dyn Progress) -> Result<()> {
    let filename = tarball.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // OCI bundle files are always named "oci-…" by generate_cache_filename.
    // Use the filename prefix as a zero-I/O dispatch signal instead of
    // scanning the archive for a layers.manifest entry (which required
    // reading the entire compressed archive twice for large OCI images).
    if filename.starts_with("oci-") {
        log::debug!("Detected multi-layer OCI image, extracting layers...");
        return Ok(extract_multi_layer_oci(tarball, dest, progress)?);
    }

    Ok(extract_single_archive(tarball, dest, progress)?)
}

/// Extract a plain rootfs tarball, detecting the compression format from
/// the filename with a magic-byte fallback.
fn extract_single_archive(
    tarball: &Path,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let filename = tarball.file_name().and_then(|n| n.to_str()).unwrap_or("");

    let file = File::open(tarball)
        .with_context(|| format!("Failed to open tarball: {}", tarball.display()))?;

    let reader = BufReader::new(file);
    let mut fixups = DeferredFixups::default();

    // Detect compression format from filename
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        extract_gz(reader, dest, &mut fixups, progress)?;
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        extract_xz(reader, dest, &mut fixups, progress)?;
    } else if filename.ends_with(".tar.zst") || filename.ends_with(".tar.zstd") {
        extract_zst(reader, dest, &mut fixups, progress)?;
    } else if filename.ends_with(".tar.bz2") || filename.ends_with(".tbz2") {
        // Named before the magic-byte fallback misreads a bzip2
        // stream as an uncompressed tar.
        return Err(anyhow::anyhow!(
            "bzip2-compressed tarballs are not supported: {} (use a \
             .tar.gz, .tar.xz or .tar.zst rootfs)",
            tarball.display()
        ));
    } else if filename.ends_with(".tar") {
        extract_tar(reader, dest, &mut fixups, progress)?;
    } else {
        // Try to detect from magic bytes
        let mut magic = [0u8; 6];
        let mut peek_reader = BufReader::new(File::open(tarball)?);
        peek_reader.read_exact(&mut magic)?;

        match magic {
            [0x1f, 0x8b, ..] => {
                // gzip magic
                drop(peek_reader);
                let file = File::open(tarball)?;
                extract_gz(BufReader::new(file), dest, &mut fixups, progress)?;
            }
            [0xfd, b'7', b'z', b'X', b'Z', 0x00] => {
                // xz magic
                drop(peek_reader);
                let file = File::open(tarball)?;
                extract_xz(BufReader::new(file), dest, &mut fixups, progress)?;
            }
            [0x28, 0xb5, 0x2f, 0xfd, ..] => {
                // zstd magic
                drop(peek_reader);
                let file = File::open(tarball)?;
                extract_zst(BufReader::new(file), dest, &mut fixups, progress)?;
            }
            _ => {
                // Assume uncompressed tar
                drop(peek_reader);
                let file = File::open(tarball)?;
                extract_tar(BufReader::new(file), dest, &mut fixups, progress)?;
            }
        }
    }

    apply_fixups(dest, &mut fixups)
}

/// Extract a multi-layer OCI image
fn extract_multi_layer_oci(
    tarball: &Path,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let filename = tarball.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Extract the outer OCI bundle (layers.manifest + layer_N.tar.gz files)
    // into a temp directory.  This is our own format, not an OCI layer, so
    // plain unpack is fine here.
    let temp_dir = tempfile::tempdir().context("Failed to create temp directory for OCI layers")?;

    let file = File::open(tarball)
        .with_context(|| format!("Failed to open OCI tarball: {}", tarball.display()))?;
    let reader = BufReader::new(file);

    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        tar::Archive::new(flate2::read::GzDecoder::new(reader))
            .unpack(temp_dir.path())
            .context("Failed to unpack OCI bundle")?;
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        tar::Archive::new(xz2::read::XzDecoder::new(reader))
            .unpack(temp_dir.path())
            .context("Failed to unpack OCI bundle")?;
    } else if filename.ends_with(".tar.zst") || filename.ends_with(".tar.zstd") {
        tar::Archive::new(zstd::stream::read::Decoder::new(reader)?)
            .unpack(temp_dir.path())
            .context("Failed to unpack OCI bundle")?;
    } else {
        tar::Archive::new(reader)
            .unpack(temp_dir.path())
            .context("Failed to unpack OCI bundle")?;
    }

    let manifest_path = temp_dir.path().join("layers.manifest");
    if !manifest_path.exists() {
        // Not our layer-bundle layout: an "oci-" entry whose outer
        // archive is a plain rootfs tarball (the dispatch keys on the
        // filename prefix, so a non-bundle entry still lands here).
        log::debug!("No layers.manifest in bundle; extracting as plain rootfs tarball");
        return extract_single_archive(tarball, dest, progress);
    }

    // Read the layers manifest
    let manifest =
        std::fs::read_to_string(&manifest_path).context("Failed to read layers.manifest")?;

    // One fixup set across all layers: later layers routinely write into
    // directories the base layer ships read-only, and hard-link targets
    // routinely live in an earlier layer.
    let mut fixups = DeferredFixups::default();

    // Apply each layer in order with full whiteout handling.
    for layer_name in manifest.lines() {
        let layer_path = temp_dir.path().join(layer_name);
        if !layer_path.exists() {
            continue;
        }
        log::debug!("Extracting layer: {}", layer_name);
        extract_oci_layer(&layer_path, dest, &mut fixups, progress)?;
    }

    apply_fixups(dest, &mut fixups)
}

/// Decompress and extract one OCI layer tarball into `dest`, honouring
/// whiteout markers.  Compression is inferred from the filename then from
/// magic bytes.
fn extract_oci_layer(
    layer_path: &Path,
    dest: &Path,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    use std::io::Read;
    let layer_name = layer_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let file = File::open(layer_path)
        .with_context(|| format!("Failed to open layer: {}", layer_path.display()))?;
    let reader = BufReader::new(file);

    // The decompressor type differs per format; erase it behind a trait
    // object so a single archive-extraction call site serves every branch.
    let archive: tar::Archive<Box<dyn Read>> =
        if layer_name.ends_with(".tar.gz") || layer_name.ends_with(".tgz") {
            tar::Archive::new(Box::new(flate2::read::GzDecoder::new(reader)))
        } else if layer_name.ends_with(".tar.xz") || layer_name.ends_with(".txz") {
            tar::Archive::new(Box::new(xz2::read::XzDecoder::new(reader)))
        } else if layer_name.ends_with(".tar.zst") || layer_name.ends_with(".tar.zstd") {
            tar::Archive::new(Box::new(zstd::stream::read::Decoder::new(reader)?))
        } else {
            // Fall back to magic-byte detection
            let mut magic = [0u8; 6];
            let mut peek = BufReader::new(File::open(layer_path)?);
            let _ = peek.read_exact(&mut magic); // short reads are fine for detection
            drop(peek);
            let file = File::open(layer_path)?;
            match magic {
                [0x1f, 0x8b, ..] => {
                    tar::Archive::new(Box::new(flate2::read::GzDecoder::new(BufReader::new(file))))
                }
                [0xfd, b'7', b'z', b'X', b'Z', 0x00] => {
                    tar::Archive::new(Box::new(xz2::read::XzDecoder::new(BufReader::new(file))))
                }
                [0x28, 0xb5, 0x2f, 0xfd, ..] => tar::Archive::new(Box::new(
                    zstd::stream::read::Decoder::new(BufReader::new(file))?,
                )),
                _ => tar::Archive::new(Box::new(BufReader::new(file))),
            }
        };

    extract_archive(archive, dest, "Extracting OCI layer", fixups, progress)
}

fn extract_gz<R: std::io::Read>(
    reader: R,
    dest: &Path,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let gz_decoder = flate2::read::GzDecoder::new(reader);
    let archive = tar::Archive::new(gz_decoder);

    extract_archive(archive, dest, "Extracting gzip archive", fixups, progress)?;

    Ok(())
}

fn extract_xz<R: std::io::Read>(
    reader: R,
    dest: &Path,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let xz_decoder = xz2::read::XzDecoder::new(reader);
    let archive = tar::Archive::new(xz_decoder);

    extract_archive(archive, dest, "Extracting xz archive", fixups, progress)?;

    Ok(())
}

fn extract_zst<R: std::io::Read>(
    reader: R,
    dest: &Path,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let zst_decoder = zstd::Decoder::new(reader)?;
    let archive = tar::Archive::new(zst_decoder);

    extract_archive(archive, dest, "Extracting zstd archive", fixups, progress)?;

    Ok(())
}

fn extract_tar<R: std::io::Read>(
    reader: R,
    dest: &Path,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let archive = tar::Archive::new(reader);

    extract_archive(archive, dest, "Extracting tar archive", fixups, progress)?;

    Ok(())
}

/// Archive entries that must not be materialized inline because the tar
/// order does not guarantee their prerequisites exist yet.
#[derive(Default)]
struct DeferredFixups {
    /// Regular-file entries with the mode they ship; applied after the
    /// walk.  The crate's own chmod runs with the special bits masked
    /// off (see `extract_archive`), so this is where a setgid binary
    /// gets its bit back — degraded, not fatal, when the filesystem
    /// refuses it.
    file_modes: Vec<(PathBuf, u32)>,
    /// Directory entries with the mode they ship; applied last.  Images
    /// routinely ship read-only directories (Fedora's ca-trust tree is
    /// dr-xr-xr-x) whose contents would be rejected by an eagerly-applied
    /// mode.
    dir_modes: Vec<(PathBuf, u32)>,
    /// Hard link entries as (destination, raw link target); applied after
    /// the file pass so targets appearing later in the tar (or in an
    /// earlier layer) resolve.
    hard_links: Vec<(PathBuf, PathBuf)>,
}

/// Join an archive-relative entry path onto `dest`, refusing paths that
/// traverse above the root (mirrors tar's own unpack component walk).
fn safe_join(dest: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    for part in rel.components() {
        match part {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(p) => out.push(p),
        }
    }
    Some(out)
}

/// Resolve a raw link target against `dest`, normalizing `..` lexically.
/// None when the target climbs out of the rootfs.
fn normalize_within(dest: &Path, target: &Path) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    for part in target.components() {
        match part {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if out == dest {
                    return None;
                }
                out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    Some(out)
}

/// Apply hard links and the collected modes after extraction.
fn apply_fixups(dest: &Path, fixups: &mut DeferredFixups) -> anyhow::Result<()> {
    for (dst, target) in fixups.hard_links.drain(..) {
        apply_hard_link(dest, &dst, &target)?;
    }
    for (path, mode) in fixups.file_modes.drain(..) {
        set_shipped_mode(&path, mode);
    }
    for (path, mode) in fixups.dir_modes.drain(..) {
        set_shipped_mode(&path, mode);
    }
    Ok(())
}

/// Apply one shipped mode, degrading the special bits when the
/// filesystem refuses them.  Images ship setgid binaries (alpine's
/// `chage`, group `shadow`) and setgid directories, and an extracting
/// context that cannot set the bit — an overlayfs tmp, a confined
/// sandbox — fails the chmod with EPERM outright.  Unprivileged tar
/// degrades the same way: keep the rwx bits, drop setuid/setgid/
/// sticky, and let the guest re-apply what it needs.  The extract
/// never dies for a mode again; a refused plain chmod is only a
/// warning (a later layer's whiteout may have removed the path, and
/// the mode is cosmetic for a machine that lives for one command).
fn set_shipped_mode(path: &Path, mode: u32) {
    let applied = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    if applied.is_err() && mode & 0o7000 != 0 {
        let plain = mode & !0o7000;
        log::debug!(
            "chmod {:o} refused for {}; extracting {:o} instead",
            mode,
            path.display(),
            plain
        );
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(plain));
    }
}

/// Materialize one hard link.  A real hard link is used when the target
/// exists on disk; otherwise the entry is preserved verbatim as a symlink —
/// some images (Fedora's ca-trust directory-hash) ship links whose targets
/// are generated after linking and never exist anywhere in the archive.
fn apply_hard_link(dest: &Path, dst: &Path, target: &Path) -> anyhow::Result<()> {
    if dst.symlink_metadata().is_ok() {
        std::fs::remove_file(dst)
            .with_context(|| format!("Failed to replace {}", dst.display()))?;
    }
    if let Some(src) = normalize_within(dest, target) {
        if std::fs::hard_link(&src, dst).is_ok() {
            return Ok(());
        }
    }
    std::os::unix::fs::symlink(target, dst)
        .with_context(|| format!("Failed to link {} -> {}", dst.display(), target.display()))?;
    log::debug!(
        "Warning: hard link target {} absent; extracted {} as a symlink",
        target.display(),
        dst.display()
    );
    Ok(())
}

/// Extract tar archive entries, reporting a per-file count through
/// `progress` and handling whiteout files for OCI layers
fn extract_archive<R: std::io::Read>(
    mut archive: tar::Archive<R>,
    dest: &Path,
    label: &str,
    fixups: &mut DeferredFixups,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    progress.begin(label, Unit::Files, None);

    // Permissions are applied by hand through the fixups, not here:
    // with preservation on, the crate chmods each file to the mode it
    // ships, and a setgid entry meeting a filesystem that refuses the
    // bit kills the whole extract with EPERM.  With preservation off
    // the crate's own chmod masks to plain rwx, which no filesystem
    // refuses, and the shipped modes come back through the fixups —
    // degraded where they must be, exact everywhere else.
    archive.set_preserve_permissions(false);
    archive.set_preserve_ownerships(false);
    archive.set_unpack_xattrs(false);

    let entries = archive
        .entries()
        .context("Failed to read archive entries")?;

    let mut count: u64 = 0;
    for entry in entries {
        let mut entry = entry.context("Failed to read archive entry")?;

        // Clone the path before any mutable borrow of entry
        let path = entry.path().context("Invalid tar entry path")?.into_owned();

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Handle whiteout files (OCI layer markers for deletions).
        // Whiteout paths come from the archive, so both branches resolve
        // them through `safe_join` like every other entry: a crafted
        // marker whose path or target climbs above the rootfs is
        // skipped, never followed host-side.
        if filename == ".wh..wh..opq" {
            // Opaque whiteout: clear all previously-extracted content in the parent directory
            let parent = path.parent().unwrap_or(Path::new(""));
            match safe_join(dest, parent) {
                Some(dest_dir) => {
                    if dest_dir.symlink_metadata().is_ok() {
                        for child in std::fs::read_dir(&dest_dir)
                            .with_context(|| format!("Failed to read {}", dest_dir.display()))?
                        {
                            let child = child?;
                            let child_path = child.path();
                            remove_path(&child_path).with_context(|| {
                                format!(
                                    "Opaque whiteout: failed to remove {}",
                                    child_path.display()
                                )
                            })?;
                        }
                    }
                }
                None => log::debug!("skipping whiteout above the rootfs: {}", path.display()),
            }
            // Do not extract the .wh..wh..opq marker itself
        } else if let Some(real_name) = filename.strip_prefix(".wh.") {
            // Regular whiteout: delete the named path from lower layers.
            // The name joins the parent before the check, so a `.wh..`
            // marker, whose target would be the parent directory
            // itself, is refused like any path that climbs.
            let parent = path.parent().unwrap_or(Path::new(""));
            match safe_join(dest, &parent.join(real_name)) {
                Some(target) => {
                    if target.symlink_metadata().is_ok() {
                        remove_path(&target).with_context(|| {
                            format!("Whiteout: failed to remove {}", target.display())
                        })?;
                    }
                }
                None => log::debug!("skipping whiteout above the rootfs: {}", path.display()),
            }
            // Do not extract the .wh.* marker itself
        } else if entry.header().entry_type().is_dir() {
            // Create the directory but defer its mode: the mode in the tar
            // may be read-only, and this directory's own contents (and other
            // layers') still need to be written into it.
            if let Some(dest_dir) = safe_join(dest, &path) {
                std::fs::create_dir_all(&dest_dir)
                    .with_context(|| format!("Failed to create {}", dest_dir.display()))?;
                if let Ok(mode) = entry.header().mode() {
                    fixups.dir_modes.push((dest_dir, mode));
                }
            }
        } else if entry.header().entry_type().is_hard_link() {
            // Defer: the target may be a later entry in this tar, live in an
            // earlier layer, or not exist at all (see apply_hard_link).
            if let Some(dest_path) = safe_join(dest, &path) {
                if let Some(link) = entry.link_name().context("Invalid hard link target")? {
                    if !link.as_os_str().is_empty() {
                        fixups.hard_links.push((dest_path, link.into_owned()));
                    }
                }
            }
        } else {
            entry
                .unpack_in(dest)
                .with_context(|| format!("Failed to extract {}", path.display()))?;
            // The crate left the file at plain rwx; the shipped mode —
            // special bits included — goes through the fixups.  A
            // symlink is skipped: set_permissions follows links, and a
            // link's own mode is nothing anyone can apply.
            if !entry.header().entry_type().is_symlink() {
                if let Some(dest_path) = safe_join(dest, &path) {
                    if let Ok(mode) = entry.header().mode() {
                        fixups.file_modes.push((dest_path, mode));
                    }
                }
            }
        }

        count += 1;
        progress.advance(1);
    }

    progress.finish();
    log::debug!("Extracted {} files", count);
    Ok(())
}

/// Remove a path: uses remove_dir_all for real directories, remove_file for
/// everything else (regular files, symlinks — including symlinks-to-dirs).
fn remove_path(path: &Path) -> std::io::Result<()> {
    // symlink_metadata does not follow symlinks, so a symlink-to-dir correctly
    // reports file_type().is_symlink() rather than is_dir().
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tar::{Builder, EntryType, Header};

    /// Build a plain tar archive from (path, entry-type, mode, payload)
    /// tuples in the given order; link payloads are the raw link targets.
    fn build_tar(path: &Path, entries: &[(&str, EntryType, u32, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut ar = Builder::new(file);
        build_tar_into(&mut ar, entries);
        ar.finish().unwrap();
    }

    /// Append (path, entry-type, mode, payload) tuples to an open
    /// archive; entry paths go through the builder's own validation.
    fn build_tar_into(ar: &mut Builder<File>, entries: &[(&str, EntryType, u32, &[u8])]) {
        for (name, kind, mode, payload) in entries {
            let mut header = Header::new_gnu();
            header.set_size(if *kind == EntryType::Regular {
                payload.len() as u64
            } else {
                0
            });
            header.set_entry_type(*kind);
            header.set_mode(*mode);
            if *kind == EntryType::Symlink || *kind == EntryType::Link {
                header
                    .set_link_name(std::path::PathBuf::from(
                        std::str::from_utf8(payload).unwrap(),
                    ))
                    .unwrap();
            }
            header.set_cksum();
            if *kind == EntryType::Regular {
                ar.append_data(&mut header, name, *payload).unwrap();
            } else {
                ar.append_data(&mut header, name, std::io::empty()).unwrap();
            }
        }
    }

    /// Fedora's ca-certificates ships etc/pki/ca-trust/extracted/pem/
    /// directory-hash/ as a read-only (0555) directory full of per-cert hash
    /// symlinks (002c0b4f.0 -> GlobalSign_Root_R46.pem) whose targets come
    /// later in the tar or not at all, plus hard links elsewhere in the
    /// image.  Extracting must never open or follow a link payload, and must
    /// not choke on the read-only directory.
    #[test]
    fn fedora_directory_hash_links_extract_without_following() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let tarball = src.path().join("rootfs.tar");
        build_tar(
            &tarball,
            &[
                (
                    "etc/pki/ca-trust/extracted/pem/directory-hash/",
                    EntryType::Directory,
                    0o555,
                    b"",
                ),
                // symlink whose target appears later in the archive
                (
                    "etc/pki/ca-trust/extracted/pem/directory-hash/002c0b4f.0",
                    EntryType::Symlink,
                    0o777,
                    b"GlobalSign_Root_R46.pem",
                ),
                // symlink whose target never appears (dangling)
                (
                    "etc/pki/ca-trust/extracted/pem/directory-hash/01419da9.0",
                    EntryType::Symlink,
                    0o777,
                    b"Missing_Root.pem",
                ),
                // the real cert file, written after the read-only dir exists
                (
                    "etc/pki/ca-trust/extracted/pem/directory-hash/GlobalSign_Root_R46.pem",
                    EntryType::Regular,
                    0o644,
                    b"-----BEGIN CERTIFICATE-----",
                ),
            ],
        );

        extract_tarball(&tarball, out.path(), &NoProgress).unwrap();

        let base = out
            .path()
            .join("etc/pki/ca-trust/extracted/pem/directory-hash");
        assert_eq!(
            std::fs::read_link(base.join("002c0b4f.0")).unwrap(),
            Path::new("GlobalSign_Root_R46.pem")
        );
        assert_eq!(
            std::fs::read_link(base.join("01419da9.0")).unwrap(),
            Path::new("Missing_Root.pem"),
            "dangling symlinks must be created verbatim, not followed"
        );
        assert_eq!(
            std::fs::read(base.join("GlobalSign_Root_R46.pem")).unwrap(),
            b"-----BEGIN CERTIFICATE-----"
        );
        let dir_mode = base.symlink_metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(dir_mode, 0o555, "shipped directory mode must be restored");
    }

    /// Alpine ships setgid-shadow binaries (usr/bin/chage at 02755,
    /// group `shadow`) and setgid directories.  Their modes no longer
    /// ride the crate's own chmod — one refused setgid bit there killed
    /// the whole extract with EPERM — so they must come back through the
    /// fixups exactly where the filesystem allows the bits.
    #[test]
    fn setgid_entries_keep_their_shipped_modes() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let tarball = src.path().join("rootfs.tar");
        build_tar(
            &tarball,
            &[
                ("usr/", EntryType::Directory, 0o755, b""),
                ("usr/bin/", EntryType::Directory, 0o755, b""),
                ("usr/bin/chage", EntryType::Regular, 0o2755, b"#!/bin/sh\n"),
                // setuid and setgid together, the way su ships
                ("usr/bin/su", EntryType::Regular, 0o6755, b"#!/bin/sh\n"),
                ("usr/bin/plain", EntryType::Regular, 0o644, b"plain"),
                ("var/", EntryType::Directory, 0o755, b""),
                ("var/spool/", EntryType::Directory, 0o2755, b""),
            ],
        );

        extract_tarball(&tarball, out.path(), &NoProgress).unwrap();

        let mode = |p: &str| {
            std::fs::symlink_metadata(out.path().join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("usr/bin/chage"), 0o2755, "setgid restored");
        assert_eq!(mode("usr/bin/su"), 0o6755, "setuid and setgid restored");
        assert_eq!(mode("usr/bin/plain"), 0o644, "plain modes untouched");
        assert_eq!(mode("var/spool"), 0o2755, "setgid directories restored");
    }

    #[test]
    fn hard_link_to_later_target_extracts() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let tarball = src.path().join("rootfs.tar");
        build_tar(
            &tarball,
            &[
                ("etc/dir/", EntryType::Directory, 0o755, b""),
                // hard link recorded before its target
                (
                    "etc/dir/link.0",
                    EntryType::Link,
                    0o644,
                    b"etc/dir/real.pem",
                ),
                ("etc/dir/real.pem", EntryType::Regular, 0o644, b"payload"),
            ],
        );

        extract_tarball(&tarball, out.path(), &NoProgress).unwrap();
        assert_eq!(
            std::fs::read(out.path().join("etc/dir/link.0")).unwrap(),
            b"payload"
        );
        let link = out
            .path()
            .join("etc/dir/link.0")
            .symlink_metadata()
            .unwrap();
        assert!(
            !link.file_type().is_symlink(),
            "target exists later in the tar, so a real hard link is expected"
        );
        let link_ino = std::fs::metadata(out.path().join("etc/dir/link.0"))
            .unwrap()
            .ino();
        let real_ino = std::fs::metadata(out.path().join("etc/dir/real.pem"))
            .unwrap()
            .ino();
        assert_eq!(link_ino, real_ino);
    }

    /// Whiteout markers delete inside the rootfs and never above it:
    /// both a `..`-carrying marker path and a `.wh..` marker, whose
    /// target is the parent directory itself, must be refused while the
    /// ordinary markers still do their job.
    #[test]
    fn whiteouts_delete_inside_and_never_above_the_rootfs() {
        let src = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, b"keep").unwrap();

        let tarball = src.path().join("rootfs.tar");
        let file = File::create(&tarball).unwrap();
        let mut ar = Builder::new(file);
        build_tar_into(
            &mut ar,
            &[
                ("etc/", EntryType::Directory, 0o755, b""),
                ("etc/passwd", EntryType::Regular, 0o644, b"old"),
                // the ordinary marker: etc/passwd goes away
                ("etc/.wh.passwd", EntryType::Regular, 0o0, b""),
                // a marker whose target would be the parent directory
                ("etc/.wh..", EntryType::Regular, 0o0, b""),
                ("etc/motd", EntryType::Regular, 0o644, b"kept"),
            ],
        );
        // A marker path climbing above the rootfs, written as a raw
        // header: the builder refuses `..` names, an attacker's tar
        // does not have to.
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(0);
        header.set_mode(0o0);
        let mut name = [0u8; 100];
        name[..b"../.wh.victim.txt".len()].copy_from_slice(b"../.wh.victim.txt");
        header.as_gnu_mut().unwrap().name = name;
        header.set_cksum();
        ar.append(&header, std::io::empty()).unwrap();
        ar.finish().unwrap();

        extract_tarball(&tarball, out.path(), &NoProgress).unwrap();

        assert!(
            !out.path().join("etc/passwd").exists(),
            "the ordinary whiteout must remove its entry"
        );
        assert!(
            out.path().join("etc/motd").exists(),
            "other entries survive"
        );
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"keep",
            "a whiteout above the rootfs must not touch the host"
        );
        assert!(
            out.path().exists(),
            "the `.wh..` marker must not delete the rootfs itself"
        );
    }

    #[test]
    fn dangling_hard_link_does_not_abort_extraction() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let tarball = src.path().join("rootfs.tar");
        build_tar(
            &tarball,
            &[
                ("etc/dir/", EntryType::Directory, 0o755, b""),
                // target is not in the archive at all
                (
                    "etc/dir/dangling.0",
                    EntryType::Link,
                    0o644,
                    b"etc/dir/absent.pem",
                ),
                ("etc/dir/keep.txt", EntryType::Regular, 0o644, b"kept"),
            ],
        );

        extract_tarball(&tarball, out.path(), &NoProgress).unwrap();
        assert_eq!(
            std::fs::read(out.path().join("etc/dir/keep.txt")).unwrap(),
            b"kept",
            "extraction must not abort on an unresolvable hard link"
        );
        let meta = out.path().join("etc/dir/dangling.0").symlink_metadata();
        assert!(meta.map(|m| m.file_type().is_symlink()).unwrap_or(false));
        assert_eq!(
            std::fs::read_link(out.path().join("etc/dir/dangling.0")).unwrap(),
            Path::new("etc/dir/absent.pem")
        );
    }
    #[test]
    fn a_bzip2_tarball_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("rootfs.tar.bz2");
        std::fs::write(&tarball, b"BZh9-not-really").unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = format!(
            "{:#}",
            extract_tarball(&tarball, out.path(), &NoProgress).unwrap_err()
        );
        assert!(
            err.contains("bzip2-compressed tarballs are not supported"),
            "{err}"
        );
    }
}
