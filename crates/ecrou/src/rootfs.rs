//! Cache-aware rootfs lifecycle: resolve an image reference, download
//! through the tarball cache, extract into a scratch directory, and
//! optionally persist a provisioned rootfs back into the cache.
//!
//! The hot-cell flow is [`RootfsCache::prepare_provisioned`]: provision
//! once (package installs, user setup, …), and every subsequent call —
//! in this or another process sharing the cache — hits the persisted
//! entry and skips both the download and the provisioning step:
//!
//! ```no_run
//! use ecrou::{NoProgress, PrepareRequest, RootfsCache};
//!
//! # fn main() -> anyhow::Result<()> {
//! let cache = RootfsCache::new(RootfsCache::default_dir()?);
//! let req = PrepareRequest::new("alpine:3.23");
//!
//! let rootfs = cache.prepare_provisioned(&req, &NoProgress, |rootfs| {
//!     // Provision by editing the tree host-side. In-rootfs commands
//!     // arrive with vm::run, which boots the provisioned rootfs.
//!     std::fs::write(rootfs.dir().join("etc/motd"), "provisioned by ecrou\n")?;
//!     Ok(())
//! })?;
//!
//! // Amortized runs hit the provisioned entry directly
//! let dir = rootfs.dir().to_path_buf();
//! # let _ = dir; Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::cache::{
    extract_prepared, fill_entry, oci_freshness_changed, sidecar, EntryLock, RootfsCache,
    PROVISIONED_SUFFIX,
};
use crate::distro::{resolve_source, ImageSource};
use crate::error::Error;
use crate::progress::Progress;
use crate::Result;
use tempfile::TempDir;

impl RootfsCache {
    /// Resolve `req`, download through the cache when needed, and extract
    /// into a fresh scratch directory (removed when the returned
    /// [`PreparedRootfs`] is dropped).  Download and extraction progress is
    /// reported through `progress`.
    pub fn prepare(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<PreparedRootfs> {
        let arch = req.resolved_arch();
        let source = resolve_source(&req.image, &arch)?;

        // A local archive bypasses the cache: the file is the pristine
        // source on every run (--no-cache has nothing to say about it),
        // and the derived entry name only matters as the place a
        // provisioning over it would persist into.
        if let ImageSource::LocalTarball { path } = &source {
            if !path.is_file() {
                return Err(Error::Other(anyhow!(
                    "local rootfs archive not found: {} (a reference naming a path \
                     must be a rootfs tarball: ./rootfs.tar.gz)",
                    path.display()
                )));
            }
            let entry = self.tarball_path(&source, &arch);
            return extract_prepared(path, entry, progress).map_err(Error::Other);
        }

        let cache_path = self.tarball_path(&source, &arch);

        // OCI images with a floating tag (":latest") need a freshness check:
        // fetch the current manifest digest from the registry and compare it
        // against the digest stored from the last download.  Only re-pull when
        // the digest has actually changed.  On a network error we fall back to
        // the cached image with a warning rather than hard-failing.
        let oci_digest_changed = oci_freshness_changed(&source, &cache_path);

        // Download when not cached, explicitly requested, or the remote digest
        // has moved
        let needs_download = req.no_cache || !cache_path.exists() || oci_digest_changed;
        if needs_download {
            std::fs::create_dir_all(&self.dir).with_context(|| {
                format!("Failed to create cache directory {}", self.dir.display())
            })?;
            if req.no_cache {
                fill_entry(&source, &cache_path, &arch, progress)?;
            } else {
                // One writer per entry: a concurrent run preparing the
                // same image waits here instead of downloading a second
                // copy the atomic rename would throw away.
                let _entry_lock = EntryLock::acquire(&cache_path).map_err(Error::Other)?;
                // Re-derive the freshness answer under the lock: a
                // competing run may have filled or refreshed the entry
                // while we waited.
                let stale = !cache_path.exists()
                    || (oci_digest_changed && oci_freshness_changed(&source, &cache_path));
                if stale {
                    fill_entry(&source, &cache_path, &arch, progress)?;
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

        Ok(extract_prepared(&cache_path.clone(), cache_path, progress)?)
    }

    /// Prepare a provisioned rootfs with cache amortization: when the cache
    /// entry is already marked provisioned it is extracted as-is; otherwise
    /// the rootfs is prepared from the pristine image, `provision` runs
    /// once, and the result is persisted into the cache so subsequent calls
    /// (including from other processes sharing this cache) skip both the
    /// download and the provisioning step.
    ///
    /// A provisioned entry no longer tracks the upstream image: for
    /// `:latest` OCI images the freshness check is skipped on a marker hit.
    /// Re-provision by removing the entry and its `.provisioned` sidecar, or
    /// by passing `no_cache: true` (which re-downloads, re-provisions and
    /// re-persists).
    pub fn prepare_provisioned<F>(
        &self,
        req: &PrepareRequest,
        progress: &dyn Progress,
        provision: F,
    ) -> Result<PreparedRootfs>
    where
        F: FnOnce(&PreparedRootfs) -> anyhow::Result<()>,
    {
        let arch = req.resolved_arch();
        let source = resolve_source(&req.image, &arch)?;
        let cache_path = self.entry_path(&source, &arch, req.layer_key.as_deref())?;
        let marker = sidecar(&cache_path, PROVISIONED_SUFFIX);

        if !req.no_cache && cache_path.exists() && marker.exists() {
            log::debug!("Using provisioned cache entry: {}", cache_path.display());
            return Ok(extract_prepared(&cache_path.clone(), cache_path, progress)?);
        }

        // The miss path prepares the PRISTINE base — a keyed entry
        // has no tarball of its own to prepare from, and the base
        // must download if missing. The key is this call's business,
        // never prepare's.
        let mut base_req = req.clone();
        base_req.layer_key = None;
        let rootfs = self.prepare(&base_req, progress)?;
        provision(&rootfs)?;
        if req.layer_key.is_some() {
            // The prepared scratch is the BASE (a keyed entry has no
            // tarball to prepare from): the provisioned tree lands in
            // the keyed entry, not over the base.
            rootfs.persist_to(&cache_path).with_context(|| {
                format!(
                    "Failed to persist provisioned layer to {}",
                    cache_path.display()
                )
            })?;
        } else {
            rootfs.persist().with_context(|| {
                format!(
                    "Failed to persist provisioned rootfs to {}",
                    cache_path.display()
                )
            })?;
        }
        Ok(rootfs)
    }
}

/// Request for [`RootfsCache::prepare`] / [`RootfsCache::prepare_provisioned`].
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

    /// A layer key: when set, the cache entry is the keyed layer over
    /// the image rather than the image itself — the entry is never
    /// downloaded, only provisioned into existence
    /// ([`RootfsCache::prepare_provisioned`]).
    pub layer_key: Option<String>,
}

impl PrepareRequest {
    pub fn new(image: impl Into<String>) -> Self {
        PrepareRequest {
            image: image.into(),
            arch: String::new(),
            no_cache: false,
            layer_key: None,
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
/// [`crate::vm::run`] to boot. The directory is removed when this value is
/// dropped, unless [`PreparedRootfs::persist`] ran first.
#[derive(Debug)]
pub struct PreparedRootfs {
    pub(crate) rootfs: TempDir,
    pub(crate) cache_path: PathBuf,
}

impl PreparedRootfs {
    /// The extracted rootfs directory.
    pub fn dir(&self) -> &Path {
        self.rootfs.path()
    }

    /// Persist the current contents of the rootfs into the cache entry it
    /// was prepared from, replacing the pristine image tarball, and mark
    /// the entry provisioned.  Later preparations for the same image and
    /// architecture hit this entry directly; note that plain
    /// [`RootfsCache::prepare`] also returns the provisioned contents
    /// (indistinguishable from a pristine hit) — only
    /// [`RootfsCache::prepare_provisioned`] interprets the marker.
    pub fn persist(&self) -> Result<()> {
        self.persist_to(&self.cache_path)
    }

    /// Packs the tree into an arbitrary cache entry — how a provisioned
    /// layer lands under its own key while the prepared scratch still
    /// carries the base's own cache path.
    pub fn persist_to(&self, cache_path: &Path) -> Result<()> {
        crate::pack::pack_rootfs(self.dir(), cache_path)?;
        std::fs::write(sidecar(cache_path, PROVISIONED_SUFFIX), b"").with_context(|| {
            format!(
                "Failed to write provisioned marker for {}",
                cache_path.display()
            )
        })?;
        log::debug!("Persisted rootfs to cache: {}", cache_path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use std::os::unix::fs::PermissionsExt;

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

    fn roundtrip(dest_name: &str) {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("hello.txt"), b"hello").unwrap();
        std::fs::create_dir_all(src.path().join("usr/bin")).unwrap();
        std::fs::write(src.path().join("usr/bin/tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            src.path().join("usr/bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("hello.txt", src.path().join("link.txt")).unwrap();

        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join(dest_name);
        crate::pack::pack_rootfs(src.path(), &dest).unwrap();

        let out = tempfile::tempdir().unwrap();
        crate::extract::extract_tarball(&dest, out.path(), &NoProgress).unwrap();

        assert_eq!(
            std::fs::read(out.path().join("hello.txt")).unwrap(),
            b"hello"
        );
        let mode = std::fs::metadata(out.path().join("usr/bin/tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit must survive");
        assert_eq!(
            std::fs::read_link(out.path().join("link.txt")).unwrap(),
            Path::new("hello.txt")
        );
    }

    #[test]
    fn persist_roundtrip_gzip() {
        roundtrip("rootfs.tar.gz");
    }

    #[test]
    fn persist_roundtrip_xz() {
        roundtrip("rootfs.tar.xz");
    }

    #[test]
    fn persist_roundtrip_zstd() {
        roundtrip("rootfs.tar.zst");
    }

    #[test]
    fn persist_roundtrip_plain_tar() {
        roundtrip("rootfs.tar");
    }

    #[test]
    fn persist_writes_entry_and_provisioned_marker() {
        let rootfs_dir = tempfile::tempdir().unwrap();
        std::fs::write(rootfs_dir.path().join("file.txt"), b"data").unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().join("ubuntu-noble-amd64.tar.gz");

        let prepared = PreparedRootfs {
            rootfs: rootfs_dir,
            cache_path: cache_path.clone(),
        };
        prepared.persist().unwrap();

        assert!(cache_path.exists());
        assert!(sidecar(&cache_path, PROVISIONED_SUFFIX).exists());

        // The persisted entry extracts back to the original contents
        let out = tempfile::tempdir().unwrap();
        crate::extract::extract_tarball(&cache_path, out.path(), &NoProgress).unwrap();
        assert_eq!(std::fs::read(out.path().join("file.txt")).unwrap(), b"data");
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
        crate::pack::pack_rootfs(&tree, &tarball).unwrap();

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
