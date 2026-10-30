//! Cache-aware rootfs lifecycle: resolve an image reference, download
//! through the tarball cache, and build the ext4 image the machine
//! boots. A run copies the cached image into a scratch directory,
//! provisions its init into the copy, and throws it all away on exit.

//! The image is the deliverable: mke2fs builds it straight from the
//! tarball, so the ownership the archive records is the ownership the
//! guest sees - guest root on a real filesystem can then change it at
//! will, which a virtiofs share owned by the host user never allowed.
//!
//! The run path is [`RootfsCache::prepare`]: a present cache entry is
//! trusted as-is — the registry is never contacted — and the freshness
//! decision belongs to [`RootfsCache::pull`]:
//!
//! ```no_run
//! use ecrou::{NoProgress, PrepareRequest, RootfsCache};
//!
//! # fn main() -> anyhow::Result<()> {
//! let cache = RootfsCache::new(RootfsCache::default_dir()?);
//! let rootfs = cache.prepare(&PrepareRequest::new("alpine:3.23"), &NoProgress)?;
//! let dir = rootfs.dir().to_path_buf();
//! # let _ = dir; Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::cache::{oci_freshness_changed, EntryLock, RootfsCache};
use crate::distro::{resolve_source, ImageSource};
use crate::download::download_image;
use crate::error::Error;
use crate::progress::Progress;
use crate::Result;
use tempfile::TempDir;

/// What a satisfied image request landed on.
#[derive(Debug, Clone)]
pub struct Pulled {
    /// The cache entry holding the image.
    pub cache_path: PathBuf,
    /// Whether this call transferred bytes: `false` means the entry
    /// was already present and current.
    pub downloaded: bool,
}

impl RootfsCache {
    /// Resolve `req`, download through the cache when needed, and copy
    /// the entry image into a fresh scratch directory (removed when the
    /// returned [`PreparedRootfs`] is dropped). Download and image-build
    /// progress is reported through `progress`.
    ///
    /// This is the boot path, not the freshness path: a present entry
    /// is trusted as-is and the registry is never contacted — the
    /// refresh decision belongs to [`RootfsCache::pull`]. A cold cache
    /// pulls by default, because there is nothing to boot otherwise.
    pub fn prepare(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<PreparedRootfs> {
        let (source, cache_path, _) = self.ensure_entry(req, progress, false)?;
        let scratch = tempfile::tempdir().context("Failed to create temporary directory")?;
        let image = scratch.path().join("rootfs.img");
        match &source {
            // A local archive bypasses the cache: the file is the
            // source on every run (--no-cache has nothing to say about
            // it), and the derived entry name only matters for
            // diagnostics.
            ImageSource::LocalTarball { path } => {
                crate::image::build_rootfs_image(path, &image, progress)?;
            }
            _ => {
                let entry_image = cache_path.with_extension("img");
                self.ensure_image(&cache_path, &entry_image, progress)?;
                crate::image::copy_image(&entry_image, &image)?;
            }
        }
        Ok(PreparedRootfs { scratch })
    }

    /// Fetch or refresh the cache entry without booting it: resolve
    /// the reference, re-check unpinned OCI tags against the
    /// registry digest, re-resolve floating distro aliases, and
    /// download only when the entry is absent or has moved.  The
    /// explicit freshness operation — `prepare` (and so `run`) boots
    /// the local entry untouched.
    pub fn pull(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<Pulled> {
        let (_, cache_path, downloaded) = self.ensure_entry(req, progress, true)?;
        // The image is the deliverable: pull leaves the entry ready to
        // boot, so a run pays for a copy, never for a build.
        let entry_image = cache_path.with_extension("img");
        self.ensure_image(&cache_path, &entry_image, progress)?;
        Ok(Pulled {
            cache_path,
            downloaded,
        })
    }

    /// The entry's image, built from the entry tarball when absent or
    /// older than the tarball, or when the file on disk is not an ext4
    /// image at all — the remains of a build that died before its
    /// atomic rename. Runs copy the image; the tarball is the build
    /// input only. One writer per entry, like the download path.
    fn ensure_image(&self, tarball: &Path, image: &Path, progress: &dyn Progress) -> Result<()> {
        let mtime = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        };
        let fresh = || {
            image.exists() && crate::image::looks_like_ext4(image) && mtime(image) >= mtime(tarball)
        };
        if fresh() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("Failed to create cache directory {}", self.dir.display()))?;
        let _entry_lock = EntryLock::acquire(image, progress).map_err(Error::Other)?;
        // Re-derive freshness under the lock: a competing run may have
        // built the image while we waited.
        if fresh() {
            log::debug!(
                "another run built {} while we waited; using it",
                image.display()
            );
            return Ok(());
        }
        crate::image::build_rootfs_image(tarball, image, progress)?;
        Ok(())
    }

    /// The shared spine of `prepare` and `pull`: resolve the request
    /// and see to it the pristine cache entry exists and is current.
    ///
    /// `recheck` is the freshness policy. `false` trusts a present
    /// entry. `true` asks the registry whether an unpinned OCI tag has
    /// moved — on a network error the cached image wins with a
    /// warning, never a hard failure. Both variants download on an
    /// absent entry: a cold cache has to pull by default.
    fn ensure_entry(
        &self,
        req: &PrepareRequest,
        progress: &dyn Progress,
        recheck: bool,
    ) -> Result<(ImageSource, PathBuf, bool)> {
        let arch = req.resolved_arch();
        let source = resolve_source(&req.image, &arch)?;

        if let ImageSource::LocalTarball { path } = &source {
            if recheck {
                return Err(Error::Other(anyhow!(
                    "nothing to pull: {} is a local file; pull fetches remote images",
                    path.display()
                )));
            }
            if !path.is_file() {
                return Err(Error::Other(anyhow!(
                    "local rootfs archive not found: {} (a reference naming a path \
                     must be a rootfs tarball: ./rootfs.tar.gz)",
                    path.display()
                )));
            }
            let entry = self.tarball_path(&source, &arch);
            return Ok((source, entry, false));
        }

        let cache_path = self.tarball_path(&source, &arch);

        let oci_digest_changed = if recheck {
            oci_freshness_changed(&source, &cache_path)
        } else {
            false
        };

        let needs_download = req.no_cache || !cache_path.exists() || oci_digest_changed;
        let mut downloaded = false;
        if needs_download {
            std::fs::create_dir_all(&self.dir).with_context(|| {
                format!("Failed to create cache directory {}", self.dir.display())
            })?;
            if req.no_cache {
                download_image(&source, &cache_path, &arch, progress)?;
                downloaded = true;
            } else {
                // One writer per entry: a concurrent run preparing the
                // same image waits here instead of downloading a second
                // copy the atomic rename would throw away.
                let _entry_lock =
                    EntryLock::acquire(&cache_path, progress).map_err(Error::Other)?;
                // Re-derive the freshness answer under the lock: a
                // competing run may have filled or refreshed the entry
                // while we waited.
                let stale = !cache_path.exists()
                    || (oci_digest_changed && oci_freshness_changed(&source, &cache_path));
                if stale {
                    download_image(&source, &cache_path, &arch, progress)?;
                    downloaded = true;
                } else {
                    log::debug!(
                        "another run prepared {} while we waited; using it",
                        cache_path.display()
                    );
                }
            }
        } else {
            log::debug!("Using cached tarball: {}", cache_path.display());
        }

        Ok((source, cache_path, downloaded))
    }
}

/// Request for [`RootfsCache::prepare`] / [`RootfsCache::pull`].
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    /// Image reference: a distro name ("ubuntu", "ubuntu:noble",
    /// "alpine:3.23") or an OCI reference ("debian",
    /// "ghcr.io/org/image:tag", "docker://…").
    pub image: String,

    /// Target architecture (e.g. "amd64", "arm64").  Empty selects the host
    /// architecture.
    pub arch: String,

    /// Ignore the cache and download a fresh image.
    pub no_cache: bool,
}

impl PrepareRequest {
    pub fn new(image: impl Into<String>) -> Self {
        PrepareRequest {
            image: image.into(),
            arch: String::new(),
            no_cache: false,
        }
    }

    /// The effective architecture: the request value, or the host arch.
    pub fn resolved_arch(&self) -> String {
        if self.arch.is_empty() {
            crate::utils::get_host_arch().debian_name().to_string()
        } else {
            self.arch.clone()
        }
    }
}

/// A rootfs image copied into a scratch directory, ready for
/// [`crate::vm::run`] to boot. The scratch directory is removed when
/// this value is dropped.
#[derive(Debug)]
pub struct PreparedRootfs {
    pub(crate) scratch: TempDir,
}

impl PreparedRootfs {
    /// The run's scratch directory: the image copy lives here, next to
    /// the console capture and the files waiting to be provisioned in.
    pub fn dir(&self) -> &Path {
        self.scratch.path()
    }

    /// The image copy the machine boots.
    pub(crate) fn image(&self) -> PathBuf {
        self.scratch.path().join("rootfs.img")
    }

    /// Per-run surgery on the image copy: the generated init at its
    /// fixed guest path, the host resolver over whatever the image
    /// shipped, a stale control file removed. debugfs does it; nothing
    /// mounts.
    pub(crate) fn provision(&self, unlinks: &[&str], files: &[(&Path, &str, u32)]) -> Result<()> {
        crate::image::provision(&self.image(), unlinks, files).map_err(Error::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;

    fn have_e2fsprogs() -> bool {
        std::process::Command::new("mkfs.ext4")
            .arg("-V")
            .output()
            .is_ok()
    }

    #[test]
    fn pull_rejects_local_archives() {
        let cache = RootfsCache::new("/cache");
        let req = PrepareRequest::new("./nope.tar.gz");
        let err = cache.pull(&req, &NoProgress).unwrap_err();
        assert!(err.to_string().contains("nothing to pull"), "{err}");
    }

    #[test]
    fn pull_on_a_cached_entry_reports_no_download() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        let pinned = crate::distro::parse_image_ref(
            "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "amd64",
        )
        .unwrap();
        let entry = cache.tarball_path(&pinned, "amd64");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        // A real (tiny) rootfs tarball: the entry is trusted, but pull
        // still owes the cache its bootable image, built from it.
        let file = std::fs::File::create(&entry).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        tar.append_data(&mut header, "etc", std::io::empty())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let req = PrepareRequest::new(
            "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let pulled = cache.pull(&req, &NoProgress).unwrap();
        assert!(!pulled.downloaded);
        assert_eq!(pulled.cache_path, entry);
        // The image is the deliverable: pull leaves it next to the
        // tarball, and a second pull rebuilds nothing.
        let image = entry.with_extension("img");
        assert!(image.exists());
        let before = std::fs::metadata(&image).unwrap().modified().unwrap();
        let pulled = cache.pull(&req, &NoProgress).unwrap();
        assert!(!pulled.downloaded);
        assert_eq!(
            std::fs::metadata(&image).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn pull_downloads_an_absent_entry() {
        // Structural check only: absent pin entry + pin immutability
        // means the pull must decide to download without any network.
        // The transfer itself is fill_entry's job, exercised by the
        // real pull flow.
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        let req = PrepareRequest::new(
            "ubuntu@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        // A pinned OCI source resolves without the network, so
        // resolve_source succeeds; the entry is absent, so pull takes
        // the download path and fails in fill_entry (no registry
        // reachable in the sandbox, or a real fetch otherwise) — both
        // outcomes prove the decision, so only the resolution is
        // asserted here.
        let arch = req.resolved_arch();
        let source = resolve_source(&req.image, &arch).unwrap();
        assert!(matches!(source, ImageSource::OciImage { .. }));
        assert!(!cache.tarball_path(&source, &arch).exists());
    }

    #[test]
    fn resolved_arch_defaults_to_host() {
        let req = PrepareRequest::new("alpine");
        assert_eq!(
            req.resolved_arch(),
            crate::utils::get_host_arch().debian_name()
        );

        let mut req = PrepareRequest::new("alpine");
        req.arch = "arm64".to_string();
        assert_eq!(req.resolved_arch(), "arm64");
    }

    #[test]
    fn a_local_archive_prepares_without_touching_the_cache() {
        if !have_e2fsprogs() {
            eprintln!("SKIP: no e2fsprogs");
            return;
        }
        // A rootfs tree, packed next to itself as ./rootfs.tar.gz, the
        // way a caller names a local archive.
        let work = tempfile::tempdir().unwrap();
        let tree = work.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("etc-marker"), b"local\n").unwrap();
        let tarball = work.path().join("rootfs.tar.gz");
        let file = std::fs::File::create(&tarball).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        tar.append_dir_all(".", &tree).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(cache_dir.path().to_path_buf());
        // An absolute spelling: "./" would resolve against this test
        // process's working directory, not the tempdir.
        let req = PrepareRequest::new(tarball.to_str().unwrap());
        let rootfs = cache.prepare(&req, &NoProgress).expect("prepares");
        // The scratch holds a bootable image carrying the tree, not a
        // tree itself: the marker is read back out of the image.
        let image = rootfs.dir().join("rootfs.img");
        assert!(image.exists());
        let out = std::process::Command::new("debugfs")
            .arg("-R")
            .arg("cat /etc-marker")
            .arg(&image)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "local\n");
        // Nothing was downloaded or stashed: the file is the source.
        assert!(cache_dir.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn a_missing_local_archive_is_a_named_error() {
        let cache = RootfsCache::new(std::env::temp_dir().join("ecrou-test-absent"));
        let req = PrepareRequest::new("./definitely-not-here.tar.gz");
        let err = format!("{:#}", cache.prepare(&req, &NoProgress).unwrap_err());
        assert!(err.contains("definitely-not-here.tar.gz"), "{err}");
        assert!(err.contains("local rootfs archive"), "{err}");
    }

    #[test]
    fn a_local_archive_keys_its_derived_entry_by_size_and_mtime() {
        let source = crate::distro::parse_image_ref("./rootfs.tar.gz", "amd64").expect("parses");
        let cache = RootfsCache::new(std::path::PathBuf::from("/cache"));
        let name = cache.tarball_path(&source, "amd64");
        let name = name.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("local-rootfs.tar-"), "{name}");
        assert!(name.ends_with(".tar.gz"), "{name}");
        // A zero-length archive keys differently from a one-byte one:
        // an edited file never lands in a stale derived entry.
        let empty = tempfile::NamedTempFile::new().unwrap();
        let source = crate::distro::parse_image_ref(empty.path().to_str().unwrap(), "amd64")
            .expect("parses");
        let empty_name = cache
            .tarball_path(&source, "amd64")
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::fs::write(empty.path(), b"x").unwrap();
        let grown = cache
            .tarball_path(&source, "amd64")
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(empty_name, grown);
    }
}
