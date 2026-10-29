//! Packing a rootfs tree into the compressed tarball a cache entry
//! stores, so a provisioned tree persists and later boots skip the
//! download. The write lands in a unique partial file and renames
//! atomically; compression follows the destination's extension.

use anyhow::{anyhow, Context};
use std::path::Path;

/// Write a tarball of `rootfs` at `dest`, compressed according to dest's
/// extension (.tar.gz, .tar.xz, .tar.zst, plain .tar), atomically via a
/// unique partial file ([`crate::utils::partial_path`]).
pub(crate) fn pack_rootfs(rootfs: &Path, dest: &Path) -> anyhow::Result<()> {
    // Packing the tree into a tarball placed inside it would recurse
    // forever: the walker keeps seeing the archive grow.
    let rootfs_canonical = rootfs
        .canonicalize()
        .with_context(|| format!("Failed to resolve rootfs directory {}", rootfs.display()))?;
    if let Some(parent) = dest.parent() {
        if let Ok(parent_canonical) = parent.canonicalize() {
            if parent_canonical.starts_with(&rootfs_canonical) {
                return Err(anyhow!(
                    "Cache entry {} must live outside the rootfs",
                    dest.display()
                ));
            }
        }
    }

    let filename = dest.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let partial = crate::utils::partial_path(dest);

    let file = std::fs::File::create(&partial)
        .with_context(|| format!("Failed to create {}", partial.display()))?;

    let result: anyhow::Result<()> = (|| {
        if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let encoder = write_rootfs_tar(rootfs, encoder)?;
            encoder.finish().context("Failed to finalise gzip stream")?;
        } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
            let encoder = xz2::write::XzEncoder::new(file, 6);
            let encoder = write_rootfs_tar(rootfs, encoder)?;
            encoder.finish().context("Failed to finalise xz stream")?;
        } else if filename.ends_with(".tar.zst") || filename.ends_with(".tar.zstd") {
            let encoder = zstd::stream::write::Encoder::new(file, 0)
                .context("Failed to create zstd encoder")?;
            let encoder = write_rootfs_tar(rootfs, encoder)?;
            encoder.finish().context("Failed to finalise zstd stream")?;
        } else {
            write_rootfs_tar(rootfs, file)?;
        }
        Ok(())
    })();

    if result.is_err() {
        // Best-effort cleanup; ignore errors (file may not exist if creation failed).
        let _ = std::fs::remove_file(&partial);
        return result;
    }

    std::fs::rename(&partial, dest)
        .with_context(|| format!("Failed to move packed rootfs to {}", dest.display()))
}

/// Write the rootfs tree into a tar archive streamed to `writer`, returning
/// the writer so the caller can finalise the compression stream.
fn write_rootfs_tar<W: std::io::Write>(rootfs: &Path, writer: W) -> anyhow::Result<W> {
    let mut builder = tar::Builder::new(writer);
    // Store symlinks as symlinks; following them would duplicate the target's
    // contents into the archive (and recurse into symlinked directories).
    builder.follow_symlinks(false);

    let entries = std::fs::read_dir(rootfs)
        .with_context(|| format!("Failed to read {}", rootfs.display()))?;

    for entry in entries {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", rootfs.display()))?;
        let path = entry.path();
        let name = entry.file_name();

        // symlink_metadata: never follow symlinks — a top-level symlink to a
        // directory must be stored as a link, not recursed into.
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to stat {}", path.display()))?;

        if meta.is_dir() {
            builder
                .append_dir_all(&name, &path)
                .with_context(|| format!("Failed to archive {}", path.display()))?;
        } else {
            builder
                .append_path_with_name(&path, &name)
                .with_context(|| format!("Failed to archive {}", path.display()))?;
        }
    }

    builder
        .into_inner()
        .context("Failed to finalise tar archive")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_failure_leaves_no_partial_file() {
        // A nonexistent rootfs must fail cleanly and clean up its
        // partial file, whatever unique name it carried.
        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join("x.tar.gz");
        assert!(pack_rootfs(Path::new("/nonexistent-rootfs"), &dest).is_err());
        assert!(!dest.exists());
        let leftovers: Vec<String> = std::fs::read_dir(cache.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("partial"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "partial files left behind: {leftovers:?}"
        );
    }
}
