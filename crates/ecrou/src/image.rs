//! The ext4 rootfs image: the rootfs crosses the machine boundary as a
//! raw ext4 image mke2fs builds straight from the image tarball, and
//! per-run provisioning rides debugfs. Both tools ship in e2fsprogs; a
//! missing tool is an environment problem, named as one.
//!
//! Building from the tarball instead of an extracted tree is what keeps
//! ownership honest: mke2fs bakes each entry's recorded uid, gid and
//! mode into the new filesystem, with no host-side chown in between to
//! lose them. An unprivileged extract cannot preserve a tar's owners
//! (chown to an arbitrary uid is root's alone); a tarball to image
//! pipeline never needs one.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context};

use crate::progress::Progress;

/// Build the rootfs image at `image` from `tarball`.
///
/// An OCI layer bundle merges into one plain tar first, so the whiteout
/// bookkeeping and the verbatim headers happen in one place; a bundle
/// that turns out to carry no manifest builds directly, like any other
/// archive. mkfs consumes gz, xz and zst tarballs natively.
pub(crate) fn build_rootfs_image(
    tarball: &Path,
    image: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    progress.message("Building the rootfs image");
    let name = tarball
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let mut merged: Option<PathBuf> = None;
    if name.starts_with("oci-") {
        let side = image.with_extension("oci.tar");
        if crate::extract::merge_oci_tarball(tarball, &side, progress)? {
            merged = Some(side);
        }
    }
    let source: &Path = merged.as_deref().unwrap_or(tarball);
    let result = mkfs_from_tar(source, image);
    if let Some(side) = merged {
        // The merged archive has value only until mke2fs consumes it;
        // a failure to clean up must not mask mkfs's own error.
        let _ = std::fs::remove_file(side);
    }
    result
}

/// Run mke2fs over the archive. The filesystem size and inode count
/// come from a scan of the archive: mke2fs derives neither from a tar.
fn mkfs_from_tar(tarball: &Path, image: &Path) -> anyhow::Result<()> {
    let (bytes, entries) = tar_scan(tarball)?;
    // Headroom for the journal, the inode tables and block rounding.
    // The file is created sparse, so generous headroom costs nothing
    // on disk and gives the guest room to install packages.
    let fs_bytes = bytes + bytes / 8 + 512 * 1024 * 1024;
    // The disk presents in 512-byte sectors: a size in between hides
    // its tail from the guest.
    let fs_bytes = fs_bytes.div_ceil(4096) * 4096;
    // -N pins the inode count: the ratio default does not know the
    // archive's file count and comes up short on trees of small files.
    let inodes = entries + entries / 4 + 2048;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(image)
        .with_context(|| format!("creating {}", image.display()))?;
    file.set_len(fs_bytes)
        .with_context(|| format!("sizing {}", image.display()))?;
    drop(file);

    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-N", &inodes.to_string(), "-d"])
        .arg(tarball)
        .arg(image)
        .output()
        .map_err(|e| anyhow!("mkfs.ext4 not found ({e}): install e2fsprogs; the snap stages it"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "mkfs.ext4 failed building the rootfs image: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The uncompressed byte total and entry count of an image tarball.
fn tar_scan(tarball: &Path) -> anyhow::Result<(u64, u64)> {
    let mut archive = crate::extract::tar_decoder(tarball)?;
    let mut bytes = 0u64;
    let mut entries = 0u64;
    for entry in archive.entries().context("reading the archive")? {
        let entry = entry.context("reading an archive entry")?;
        entries += 1;
        bytes += entry.header().entry_size().unwrap_or(0);
    }
    Ok((bytes, entries))
}

/// Copy the cache entry image into the scratch directory. cp reflinks
/// where the filesystem has them and keeps the copy sparse either way:
/// an image occupying 400 MiB must not materialize a GiB of holes.
pub(crate) fn copy_image(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let out = Command::new("cp")
        .args(["--reflink=auto", "--sparse=auto"])
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| anyhow!("cp not found ({e}): coreutils is required"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "copying the rootfs image failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Write files into an image: guest paths unlinked first (a stale
/// control file, a symlink standing where a regular file must), then
/// host files copied in and their modes pinned. One debugfs process
/// does all of it; no mount anywhere. An unlink of an absent path is
/// noise, not an error: the link set is defensive.
pub(crate) fn provision(
    image: &Path,
    unlinks: &[&str],
    files: &[(&Path, &str, u32)],
) -> anyhow::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let cmds = image.with_extension("provision");
    let mut script = String::new();
    for guest in unlinks {
        script.push_str(&format!("rm {guest}\n"));
    }
    for (host, guest, mode) in files {
        script.push_str(&format!("write {} {guest}\n", host.display()));
        // sif mode replaces the whole i_mode, type bits included: a
        // bare 0755 leaves an inode with no file type, and the
        // kernel's ext4_lookup refuses it as bogus. Every provisioned
        // file is regular, so the regular-file type goes in with the
        // permission bits.
        script.push_str(&format!("sif {guest} mode 0{:o}\n", 0o100000 | mode));
    }
    std::fs::write(&cmds, script).with_context(|| format!("writing {}", cmds.display()))?;
    let out = Command::new("debugfs")
        .args(["-w", "-f"])
        .arg(&cmds)
        .arg(image)
        .output()
        .map_err(|e| anyhow!("debugfs not found ({e}): install e2fsprogs; the snap stages it"))?;
    let _ = std::fs::remove_file(&cmds);
    if !out.status.success() {
        return Err(anyhow!(
            "debugfs failed provisioning {}: {}",
            image.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// A file's content out of the image. debugfs echoes commands and
/// prints a banner on stdout; both start with "debugfs" and the only
/// reader (the exit-code control file) holds an integer, so those
/// lines are dropped. An absent file reads back empty.
pub(crate) fn read_file(image: &Path, guest: &str) -> anyhow::Result<String> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("cat {guest}"))
        .arg(image)
        .output()
        .map_err(|e| anyhow!("debugfs not found ({e}): install e2fsprogs"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter(|l| !l.starts_with("debugfs"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Whether a guest path exists in the image: a debugfs stat that finds
/// an inode. Symlinks resolve; stat follows them like the guest will.
pub(crate) fn stat_exists(image: &Path, guest: &str) -> anyhow::Result<bool> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat {guest}"))
        .arg(image)
        .output()
        .map_err(|e| anyhow!("debugfs not found ({e}): install e2fsprogs"))?;
    Ok(String::from_utf8_lossy(&out.stdout).contains("Inode:"))
}

/// Which shell the image ships, probed in the image. The order mirrors
/// `utils::detect_shell` over an extracted tree, which the image build
/// replaced; the fallback is the same, and fails in the guest with a
/// clear exec error if the image ships no shell at all.
pub(crate) fn detect_shell(image: &Path) -> anyhow::Result<&'static str> {
    for shell in ["/bin/bash", "/bin/sh", "/usr/bin/bash", "/usr/bin/sh"] {
        if stat_exists(image, shell)? {
            return Ok(shell);
        }
    }
    Ok("/bin/sh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn have_e2fsprogs() -> bool {
        Command::new("mkfs.ext4").arg("-V").output().is_ok()
            && Command::new("debugfs").arg("-V").output().is_ok()
    }

    /// A tarball with one root-owned directory and a file owned by uid
    /// 104, the way distro archives record ownership. Gzip when the
    /// name says so, so both dispatch paths get exercised.
    fn fixture(tarball: &Path) {
        let file = std::fs::File::create(tarball).unwrap();
        let name = tarball.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // The erased writer keeps one build path; the compression
        // comes from the name, exactly like the reader dispatches.
        let mut builder = tar::Builder::new(Box::new(if name.ends_with(".tar.gz") {
            Box::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::default(),
            )) as Box<dyn std::io::Write>
        } else {
            Box::new(file) as Box<dyn std::io::Write>
        }));

        let mut dir = tar::Header::new_gnu();
        dir.set_size(0);
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_mode(0o755);
        dir.set_uid(0);
        dir.set_gid(0);
        builder
            .append_data(&mut dir, "etc", std::io::empty())
            .unwrap();

        let mut file = tar::Header::new_gnu();
        file.set_size(2);
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o644);
        file.set_uid(104);
        file.set_gid(104);
        builder
            .append_data(&mut file, "etc/marker", b"hi" as &[u8])
            .unwrap();

        builder.finish().unwrap();
    }

    fn build(tarball: &Path, image: &Path) {
        build_rootfs_image(tarball, image, &crate::progress::NoProgress).unwrap();
    }

    #[test]
    fn the_image_carries_the_tarball_ownership() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar.gz");
        fixture(&tarball);
        let image = work.path().join("rootfs.img");
        build(&tarball, &image);

        let out = Command::new("debugfs")
            .arg("-R")
            .arg("stat /etc/marker")
            .arg(&image)
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        assert!(
            stat.lines()
                .any(|l| l.contains("User:") && l.contains("104")),
            "{stat}"
        );
        assert!(
            stat.lines()
                .any(|l| l.contains("Group:") && l.contains("104")),
            "{stat}"
        );
    }

    #[test]
    fn provisioning_writes_files_and_modes() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar");
        fixture(&tarball);
        let image = work.path().join("rootfs.img");
        build(&tarball, &image);

        let script = work.path().join("ecrou-init");
        std::fs::write(&script, "#!/bin/sh\necho one\n").unwrap();

        // The unlink of an absent path is noise: resolv.conf does not
        // exist in this fixture, /.ecrou-exit does not either.
        provision(
            &image,
            &["/.ecrou-exit", "/etc/resolv.conf"],
            &[(&script, "/ecrou-init", 0o755)],
        )
        .unwrap();

        assert_eq!(
            read_file(&image, "/ecrou-init").unwrap(),
            "#!/bin/sh\necho one"
        );
        assert!(stat_exists(&image, "/ecrou-init").unwrap());
        assert!(!stat_exists(&image, "/etc/marker-does-not-exist").unwrap());

        let out = Command::new("debugfs")
            .arg("-R")
            .arg("stat /ecrou-init")
            .arg(&image)
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        assert!(stat.contains("Mode:  0755"), "{stat}");
    }

    #[test]
    fn a_plain_tarball_builds_the_same_way() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar");
        fixture(&tarball);
        let image = work.path().join("rootfs.img");
        build(&tarball, &image);
        assert_eq!(read_file(&image, "/etc/marker").unwrap(), "hi");
    }

    #[test]
    fn a_missing_file_reads_back_empty() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar");
        fixture(&tarball);
        let image = work.path().join("rootfs.img");
        build(&tarball, &image);
        assert_eq!(read_file(&image, "/.ecrou-exit").unwrap(), "");
    }

    #[test]
    fn the_shell_probe_finds_what_the_image_ships() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar");
        fixture(&tarball);
        let image = work.path().join("rootfs.img");
        build(&tarball, &image);
        // The fixture ships no shell: the fallback, not a probe hit.
        assert_eq!(detect_shell(&image).unwrap(), "/bin/sh");
    }

    #[test]
    fn the_scratch_copy_stays_sparse() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("rootfs.tar");
        fixture(&tarball);
        let entry = work.path().join("entry.img");
        build(&tarball, &entry);
        // The headroom truncation is sparse: real blocks stay small.
        let real = entry.metadata().unwrap().blocks() * 512;
        let apparent = entry.metadata().unwrap().len();
        assert!(real < apparent / 2, "real {real}, apparent {apparent}");

        let scratch = work.path().join("scratch.img");
        copy_image(&entry, &scratch).unwrap();
        let copied = scratch.metadata().unwrap();
        assert_eq!(copied.len(), apparent);
        assert!(copied.blocks() * 512 < apparent / 2);
    }
}
