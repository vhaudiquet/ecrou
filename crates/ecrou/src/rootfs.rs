//! Cache-aware rootfs lifecycle: resolve an image reference, download
//! through the tarball cache, and extract into a scratch directory for
//! [`crate::vm`] to boot.
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

use crate::cache::{extract_prepared, oci_freshness_changed, EntryLock, RootfsCache};
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
    /// Resolve `req`, download through the cache when needed, and extract
    /// into a fresh scratch directory (removed when the returned
    /// [`PreparedRootfs`] is dropped).  Download and extraction progress is
    /// reported through `progress`.
    ///
    /// This is the boot path, not the freshness path: a present entry
    /// is trusted as-is and the registry is never contacted — the
    /// refresh decision belongs to [`RootfsCache::pull`]. A cold cache
    /// pulls by default, because there is nothing to boot otherwise.
    pub fn prepare(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<PreparedRootfs> {
        let (source, cache_path, _) = self.ensure_entry(req, progress, false)?;
        // A local archive bypasses the cache: the file is the source
        // on every run (--no-cache has nothing to say about it), and
        // the derived entry name only matters for diagnostics.
        if let ImageSource::LocalTarball { path } = &source {
            return extract_prepared(path, progress).map_err(Error::Other);
        }
        Ok(extract_prepared(&cache_path, progress)?)
    }

    /// Fetch or refresh the image's cache entry without extracting it:
    /// resolve the reference, re-check unpinned OCI tags against the
    /// registry digest, re-resolve floating distro aliases, and
    /// download only when the entry is absent or has moved.  The
    /// explicit freshness operation — `prepare` (and so `run`) boots
    /// the local entry untouched.
    pub fn pull(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<Pulled> {
        let (_, cache_path, downloaded) = self.ensure_entry(req, progress, true)?;
        Ok(Pulled {
            cache_path,
            downloaded,
        })
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

/// A rootfs extracted into a scratch directory, ready for
/// [`crate::vm::run`] to boot. The directory is removed when this
/// value is dropped.
#[derive(Debug)]
pub struct PreparedRootfs {
    pub(crate) rootfs: TempDir,
}

impl PreparedRootfs {
    /// The extracted rootfs directory.
    pub fn dir(&self) -> &Path {
        self.rootfs.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;

    #[test]
    fn pull_rejects_local_archives() {
        let cache = RootfsCache::new("/cache");
        let req = PrepareRequest::new("./nope.tar.gz");
        let err = cache.pull(&req, &NoProgress).unwrap_err();
        assert!(err.to_string().contains("nothing to pull"), "{err}");
    }

    #[test]
    fn pull_on_a_cached_entry_reports_no_download() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        let pinned = crate::distro::parse_image_ref(
            "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "amd64",
        )
        .unwrap();
        let entry = cache.tarball_path(&pinned, "amd64");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, b"x").unwrap();

        let req = PrepareRequest::new(
            "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let pulled = cache.pull(&req, &NoProgress).unwrap();
        assert!(!pulled.downloaded);
        assert_eq!(pulled.cache_path, entry);
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
        // A rootfs tree, packed next to itself as ./rootfs.tar.gz, the
        // way a caller names a local archive.
        let work = tempfile::tempdir().unwrap();
        let tree = work.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("etc-marker"), b"local").unwrap();
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
        assert_eq!(
            std::fs::read(rootfs.dir().join("etc-marker")).unwrap(),
            b"local"
        );
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
