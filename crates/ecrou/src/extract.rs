//! Tar machinery for the image build: a decoder that infers
//! compression from the file name and then the magic bytes, and the
//! OCI layer-bundle merger that turns ecrou's `layers.manifest`
//! bundle into one plain tar. mke2fs consumes both.
//!
//! The extraction-to-disk path this module once carried is gone: runs
//! boot an image built straight from the tarball (see
//! [`crate::image`]), and a plain copy never preserved the archive's
//! owners the way mke2fs does.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::Context;

use crate::progress::{Progress, Unit};

/// A plain-tar reader for `path`: compression from the file name, then
/// from the magic bytes. Serves the image build, which consumes the
/// same formats the extraction path always did; bzip2 stays refused.
pub(crate) fn tar_decoder(path: &Path) -> anyhow::Result<tar::Archive<Box<dyn Read>>> {
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let inner: Box<dyn Read> = if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(reader))
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        Box::new(xz2::read::XzDecoder::new(reader))
    } else if filename.ends_with(".tar.zst") || filename.ends_with(".tar.zstd") {
        Box::new(zstd::stream::read::Decoder::new(reader)?)
    } else {
        let mut magic = [0u8; 6];
        let mut peek = BufReader::new(File::open(path)?);
        let _ = peek.read_exact(&mut magic); // short reads are fine for detection
        drop(peek);
        let file = File::open(path)?;
        match magic {
            [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(BufReader::new(file))),
            [0xfd, b'7', b'z', b'X', b'Z', 0x00] => {
                Box::new(xz2::read::XzDecoder::new(BufReader::new(file)))
            }
            [0x28, b'5', 0x2f, 0xfd, ..] => {
                Box::new(zstd::stream::read::Decoder::new(BufReader::new(file))?)
            }
            [b'B', b'Z', b'h', ..] => {
                return Err(anyhow::anyhow!(
                    "bzip2-compressed tarballs are not supported: {} (use a \
                     .tar.gz, .tar.xz or .tar.zst rootfs)",
                    path.display()
                ))
            }
            _ => Box::new(BufReader::new(file)),
        }
    };
    Ok(tar::Archive::new(inner))
}

/// Merge an OCI layer bundle into one plain tar for the image build.
/// Entry headers pass through verbatim, so the ownership and modes the
/// layers record reach the image instead of dying at a host chown the
/// way they did when layers unpacked into a tree.
///
/// Two passes. The first walks every layer collecting whiteouts; the
/// second walks them again and writes every entry no later layer
/// removes, a `.wh.` file by exact path and an `.wh..wh..opq` marker
/// by clearing its parent directory. Returns `false` when the bundle
/// carries no layers.manifest: the prefix matched but the archive is a
/// plain rootfs tarball, and the caller builds from it directly.
pub(crate) fn merge_oci_tarball(
    bundle: &Path,
    out: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<bool> {
    let temp = tempfile::tempdir_in(out.parent().unwrap_or(Path::new(".")))
        .context("Failed to create temp directory for OCI layers")?;
    tar_decoder(bundle)?.unpack(temp.path())?;

    let manifest_path = temp.path().join("layers.manifest");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).context("Failed to read layers.manifest"),
    };
    let layers: Vec<std::path::PathBuf> = manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| temp.path().join(l))
        .collect();

    // Pass one: every whiteout across every layer, keyed by layer
    // index. A later layer's whiteout removes an earlier layer's path;
    // knowing the full set up front is what lets pass two stay a
    // single forward walk.
    let mut deletes: Vec<std::collections::HashSet<String>> =
        vec![std::collections::HashSet::new(); layers.len()];
    let mut opaques: Vec<std::collections::HashSet<String>> =
        vec![std::collections::HashSet::new(); layers.len()];
    for (i, layer) in layers.iter().enumerate() {
        if !layer.exists() {
            continue;
        }
        for entry in tar_decoder(layer)?.entries().context("reading a layer")? {
            let entry = entry.context("reading a layer entry")?;
            let path = entry.path().context("Invalid tar entry path")?.into_owned();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == ".wh..wh..opq" {
                let parent = path.parent().unwrap_or(Path::new(""));
                opaques[i].insert(normalize_entry_path(parent));
            } else if let Some(real) = name.strip_prefix(".wh.") {
                let parent = path.parent().unwrap_or(Path::new(""));
                deletes[i].insert(normalize_entry_path(&parent.join(real)));
            }
        }
    }

    // Pass two: the merged archive, headers verbatim.
    let out_file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut builder = tar::Builder::new(out_file);
    progress.begin("Merging OCI layers", Unit::Files, None);
    for (i, layer) in layers.iter().enumerate() {
        if !layer.exists() {
            continue;
        }
        for entry in tar_decoder(layer)?.entries().context("reading a layer")? {
            let mut entry = entry.context("reading a layer entry")?;
            let path = entry.path().context("Invalid tar entry path")?.into_owned();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == ".wh..wh..opq" || name.starts_with(".wh.") {
                continue;
            }
            let key = normalize_entry_path(&path);
            if deleted_by_later(&key, i, &deletes, &opaques) {
                continue;
            }
            let mut header = entry.header().clone();
            if header.entry_type().is_file() {
                builder
                    .append_data(&mut header, &key, &mut entry)
                    .with_context(|| format!("writing {} into the merged archive", key))?;
            } else {
                // Directories, links, symlinks, devices: the header
                // carries everything, the data is always empty.
                builder
                    .append_data(&mut header, &key, std::io::empty())
                    .with_context(|| format!("writing {} into the merged archive", key))?;
            }
            progress.advance(1);
        }
    }
    builder.finish().context("finishing the merged archive")?;
    progress.finish();
    Ok(true)
}

/// Normalize an entry path for comparison: plain components joined by
/// slashes, `./` and leading-`/` noise gone, climbs impossible by
/// construction (ParentDir never survives the filter).
fn normalize_entry_path(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether any layer after `layer` removes `key`: a whiteout at the
/// exact path, or an opaque marker clearing a directory that holds it.
fn deleted_by_later(
    key: &str,
    layer: usize,
    deletes: &[std::collections::HashSet<String>],
    opaques: &[std::collections::HashSet<String>],
) -> bool {
    for j in layer + 1..deletes.len() {
        if deletes[j].contains(key) {
            return true;
        }
        for dir in &opaques[j] {
            if key == dir || key.starts_with(&format!("{dir}/")) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;

    /// One layer: entries appended as (type, path, mode, uid, data).
    fn layer(path: &Path, entries: &[(&str, &str, u32, u64, &[u8])]) {
        let mut builder = tar::Builder::new(std::fs::File::create(path).unwrap());
        for (kind, name, mode, uid, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_uid(*uid);
            header.set_gid(*uid);
            header.set_entry_type(match *kind {
                "dir" => tar::EntryType::Directory,
                _ => tar::EntryType::Regular,
            });
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.finish().unwrap();
    }

    #[test]
    fn the_merge_applies_whiteouts_and_keeps_headers() {
        let work = tempfile::tempdir().unwrap();
        let dir = work.path();
        layer(
            &dir.join("layer1.tar"),
            &[
                ("dir", "etc", 0o755, 0, b""),
                ("file", "etc/a", 0o644, 42, b"a"),
                ("file", "etc/keep", 0o644, 42, b"k"),
            ],
        );
        layer(
            &dir.join("layer2.tar"),
            &[
                ("file", "etc/.wh.a", 0o0, 0, b""),
                ("file", "etc/b", 0o600, 104, b"b"),
            ],
        );
        // The bundle is our own layout: layers.manifest plus layer files.
        std::fs::write(dir.join("layers.manifest"), "layer1.tar\nlayer2.tar\n").unwrap();
        let mut bundle = tar::Builder::new(std::fs::File::create(dir.join("bundle.tar")).unwrap());
        bundle
            .append_path_with_name(dir.join("layers.manifest"), "layers.manifest")
            .unwrap();
        bundle
            .append_path_with_name(dir.join("layer1.tar"), "layer1.tar")
            .unwrap();
        bundle
            .append_path_with_name(dir.join("layer2.tar"), "layer2.tar")
            .unwrap();
        bundle.finish().unwrap();

        let out = dir.join("merged.tar");
        let had = merge_oci_tarball(&dir.join("bundle.tar"), &out, &NoProgress).unwrap();
        assert!(had);

        let mut archive = tar::Archive::new(std::fs::File::open(&out).unwrap());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                format!(
                    "{} {} {:o}",
                    if e.header().entry_type().is_dir() {
                        "d"
                    } else {
                        "f"
                    },
                    e.path().unwrap().display(),
                    e.header().mode().unwrap()
                )
            })
            .collect();
        // a is whiteout-deleted by layer2; b and keep stay; modes and
        // ownership ride the headers verbatim.
        assert_eq!(
            names,
            vec![
                "d etc 755".to_string(),
                "f etc/keep 644".to_string(),
                "f etc/b 600".to_string(),
            ],
            "{names:?}"
        );
    }

    #[test]
    fn a_bundle_without_a_manifest_reports_itself() {
        // An "oci-" prefixed archive that is really a plain rootfs
        // tarball: the merge finds no layers.manifest and says so, so
        // the caller builds from the archive directly.
        let work = tempfile::tempdir().unwrap();
        let dir = work.path();
        layer(&dir.join("plain.tar"), &[("dir", "etc", 0o755, 0, b"")]);
        let out = dir.join("merged.tar");
        let had = merge_oci_tarball(&dir.join("plain.tar"), &out, &NoProgress).unwrap();
        assert!(!had);
        assert!(!out.exists());
    }

    #[test]
    fn a_bzip2_tarball_is_a_named_error() {
        // Magic-byte detection: the name says nothing, the stream says
        // bzip2, and the refusal names the formats that work.
        let work = tempfile::tempdir().unwrap();
        let tarball = work.path().join("mystery");
        std::fs::write(&tarball, b"BZh9").unwrap();
        let err = match tar_decoder(&tarball) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("a bzip2 stream must not decode"),
        };
        assert!(err.contains("bzip2"), "{err}");
    }

    #[test]
    fn the_decoder_reads_gz_xz_zst_and_plain() {
        use std::io::Write;

        let work = tempfile::tempdir().unwrap();
        let dir = work.path();
        layer(&dir.join("t.tar"), &[("dir", "etc", 0o755, 0, b"")]);
        let raw = std::fs::read(dir.join("t.tar")).unwrap();

        let mut enc = flate2::write::GzEncoder::new(
            std::fs::File::create(dir.join("t.tar.gz")).unwrap(),
            flate2::Compression::default(),
        );
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap();

        let mut enc =
            xz2::write::XzEncoder::new(std::fs::File::create(dir.join("t.tar.xz")).unwrap(), 6);
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap();

        let mut enc = zstd::stream::write::Encoder::new(
            std::fs::File::create(dir.join("t.tar.zst")).unwrap(),
            3,
        )
        .unwrap();
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap();

        // No extension at all: detection falls to the magic bytes.
        std::fs::copy(dir.join("t.tar.gz"), dir.join("blob")).unwrap();

        for name in ["t.tar", "t.tar.gz", "t.tar.xz", "t.tar.zst", "blob"] {
            let count = tar_decoder(&dir.join(name))
                .unwrap()
                .entries()
                .unwrap()
                .count();
            assert_eq!(count, 1, "{name}");
        }
    }
}
