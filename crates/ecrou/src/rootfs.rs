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

use crate::Result;
use tempfile::TempDir;

use crate::distro::{
    map_arch, parse_image_ref, resolve_distro_url, resolve_distro_version, Distro, ImageSource,
};
use crate::download::{digest_sidecar, download_image, fetch_oci_digest};
use crate::extract::extract_tarball;
use crate::progress::Progress;

/// Sidecar suffix marking a cache entry that holds a provisioned rootfs.
const PROVISIONED_SUFFIX: &str = "provisioned";

/// Sidecar path for a cache entry: `foo.tar.gz` → `foo.tar.gz.<suffix>`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// The image tarball cache (default location: `~/.cache/ecrou`).
#[derive(Debug, Clone)]
pub struct RootfsCache {
    dir: PathBuf,
}

impl RootfsCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        RootfsCache { dir: dir.into() }
    }

    /// Default cache directory: `~/.cache/ecrou` (or the platform equivalent).
    pub fn default_dir() -> Result<PathBuf> {
        dirs::cache_dir()
            .map(|p| p.join("ecrou"))
            .ok_or_else(|| anyhow!("Could not determine cache directory").into())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cache entry (tarball path) for an image source and architecture.
    pub fn tarball_path(&self, source: &ImageSource, arch: &str) -> PathBuf {
        self.dir.join(generate_cache_filename(source, arch))
    }

    /// Resolve `req`, download through the cache when needed, and extract
    /// into a fresh scratch directory (removed when the returned
    /// [`PreparedRootfs`] is dropped).  Download and extraction progress is
    /// reported through `progress`.
    pub fn prepare(&self, req: &PrepareRequest, progress: &dyn Progress) -> Result<PreparedRootfs> {
        let arch = req.resolved_arch();
        let source = resolve_source(&req.image, &arch)?;
        let cache_path = self.tarball_path(&source, &arch);

        // OCI images with a floating tag (":latest") need a freshness check:
        // fetch the current manifest digest from the registry and compare it
        // against the digest stored from the last download.  Only re-pull when
        // the digest has actually changed.  On a network error we fall back to
        // the cached image with a warning rather than hard-failing.
        let oci_digest_changed = if cache_path.exists() {
            if let ImageSource::OciImage {
                registry,
                repository,
                tag,
                ..
            } = &source
            {
                if tag == "latest" {
                    match fetch_oci_digest(registry, repository, tag) {
                        Ok(current) => {
                            let stored = std::fs::read_to_string(digest_sidecar(&cache_path)).ok();
                            stored.as_deref() != Some(current.trim())
                        }
                        Err(e) => {
                            log::warn!("could not check image freshness ({}); using cache", e);
                            false
                        }
                    }
                } else {
                    false // pinned tags are assumed immutable
                }
            } else {
                false
            }
        } else {
            false // cache absent — download triggered by !cache_path.exists() below
        };

        // Download when not cached, explicitly requested, or the remote digest
        // has moved
        if req.no_cache || !cache_path.exists() || oci_digest_changed {
            std::fs::create_dir_all(&self.dir).with_context(|| {
                format!("Failed to create cache directory {}", self.dir.display())
            })?;
            download_image(&source, &cache_path, &arch, progress)?;
        } else {
            log::debug!("Using cached tarball: {}", cache_path.display());
        }

        Ok(extract_prepared(cache_path, arch, progress)?)
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
        let cache_path = self.tarball_path(&source, &arch);
        let marker = sidecar(&cache_path, PROVISIONED_SUFFIX);

        if !req.no_cache && cache_path.exists() && marker.exists() {
            log::debug!("Using provisioned cache entry: {}", cache_path.display());
            return Ok(extract_prepared(cache_path, arch, progress)?);
        }

        let rootfs = self.prepare(req, progress)?;
        provision(&rootfs)?;
        rootfs.persist().with_context(|| {
            format!(
                "Failed to persist provisioned rootfs to {}",
                cache_path.display()
            )
        })?;
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
/// [`crate::vm::run`] to boot. The directory is removed when this value is
/// dropped, unless [`PreparedRootfs::persist`] ran first.
#[derive(Debug)]
pub struct PreparedRootfs {
    rootfs: TempDir,
    cache_path: PathBuf,
    arch: String,
}

impl PreparedRootfs {
    /// The extracted rootfs directory.
    pub fn dir(&self) -> &Path {
        self.rootfs.path()
    }

    /// Architecture this rootfs was prepared for.
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// The cache entry this rootfs was prepared from.
    pub fn cache_entry(&self) -> &Path {
        &self.cache_path
    }

    /// Persist the current contents of the rootfs into the cache entry it
    /// was prepared from, replacing the pristine image tarball, and mark
    /// the entry provisioned.  Later preparations for the same image and
    /// architecture hit this entry directly; note that plain
    /// [`RootfsCache::prepare`] also returns the provisioned contents
    /// (indistinguishable from a pristine hit) — only
    /// [`RootfsCache::prepare_provisioned`] interprets the marker.
    pub fn persist(&self) -> Result<()> {
        pack_rootfs(self.dir(), &self.cache_path)?;
        std::fs::write(sidecar(&self.cache_path, PROVISIONED_SUFFIX), b"").with_context(|| {
            format!(
                "Failed to write provisioned marker for {}",
                self.cache_path.display()
            )
        })?;
        log::debug!("Persisted rootfs to cache: {}", self.cache_path.display());
        Ok(())
    }
}

/// Parse an image reference and resolve floating version aliases ("latest",
/// "lts", "edge", …) to a concrete version, so the cache key is stable
/// (e.g. "ubuntu-noble-amd64", not "ubuntu-latest-amd64") and a future
/// release automatically gets its own cache entry.
fn resolve_source(image: &str, arch: &str) -> anyhow::Result<ImageSource> {
    let source = parse_image_ref(image, arch)?;
    match source {
        ImageSource::DirectTarball { distro, version } => {
            let resolved = resolve_distro_version(&distro, version.as_deref(), arch)?;
            Ok(ImageSource::DirectTarball {
                distro,
                version: Some(resolved),
            })
        }
        other => Ok(other),
    }
}

fn extract_prepared(
    cache_path: PathBuf,
    arch: String,
    progress: &dyn Progress,
) -> anyhow::Result<PreparedRootfs> {
    let temp = tempfile::tempdir().context("Failed to create temporary directory")?;
    let rootfs = temp.path().to_path_buf();

    log::debug!("Extracting to: {}", rootfs.display());
    extract_tarball(&cache_path, &rootfs, progress)?;

    Ok(PreparedRootfs {
        rootfs: temp,
        cache_path,
        arch,
    })
}

/// Write a tarball of `rootfs` at `dest`, compressed according to dest's
/// extension (.tar.gz, .tar.xz, .tar.zst, plain .tar), atomically via a
/// `.partial` file.
fn pack_rootfs(rootfs: &Path, dest: &Path) -> anyhow::Result<()> {
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
    let partial = dest.with_extension("partial");

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

/// Generate a cache filename based on the image source
fn generate_cache_filename(source: &ImageSource, arch: &str) -> String {
    match source {
        ImageSource::DirectTarball { distro, version } => {
            let distro_name = match distro {
                Distro::Ubuntu => "ubuntu",
                Distro::Alpine => "alpine",
            };
            let distro_arch = map_arch(*distro, arch);
            // Get extension from URL
            let url = resolve_distro_url(distro, version.as_deref(), arch).unwrap_or_default();
            let ext = get_tarball_extension(&url);
            format!(
                "{}-{}-{}.{}",
                distro_name,
                version.as_deref().unwrap_or("latest"),
                distro_arch,
                ext
            )
        }
        ImageSource::OciImage {
            registry,
            repository,
            tag,
            architecture,
        } => {
            // Sanitize for filename
            let safe_registry = registry.replace(['.', ':'], "_");
            let safe_repo = repository.replace(['/', ':'], "_");
            format!(
                "oci-{}-{}-{}-{}.tar.gz",
                safe_registry, safe_repo, tag, architecture
            )
        }
    }
}

fn get_tarball_extension(url: &str) -> &str {
    // Extract extension from URL (e.g., .tar.gz, .tar.xz, .tar.zst)
    if url.ends_with(".tar.zst") {
        "tar.zst"
    } else if url.ends_with(".tar.xz") {
        "tar.xz"
    } else if url.ends_with(".tar.gz") {
        "tar.gz"
    } else if url.ends_with(".tar.bz2") {
        "tar.bz2"
    } else {
        "tar.gz" // default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use std::os::unix::fs::PermissionsExt;

    fn direct_tarball(distro: Distro, version: &str) -> ImageSource {
        ImageSource::DirectTarball {
            distro,
            version: Some(version.to_string()),
        }
    }

    #[test]
    fn cache_filename_direct_tarball() {
        // Explicit codename/version: no network lookups
        let cache = RootfsCache::new("/cache");
        assert_eq!(
            cache.tarball_path(&direct_tarball(Distro::Ubuntu, "noble"), "amd64"),
            PathBuf::from("/cache/ubuntu-noble-amd64.tar.gz")
        );
        assert_eq!(
            cache.tarball_path(&direct_tarball(Distro::Alpine, "3.23.1"), "amd64"),
            PathBuf::from("/cache/alpine-3.23.1-x86_64.tar.gz")
        );
    }

    #[test]
    fn cache_filename_oci_sanitized() {
        let cache = RootfsCache::new("/cache");
        let source = ImageSource::OciImage {
            registry: "docker.io".to_string(),
            repository: "library/ubuntu".to_string(),
            tag: "latest".to_string(),
            architecture: "amd64".to_string(),
        };
        assert_eq!(
            cache.tarball_path(&source, "amd64"),
            PathBuf::from("/cache/oci-docker_io-library_ubuntu-latest-amd64.tar.gz")
        );
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
    fn sidecar_appends_suffix() {
        assert_eq!(
            sidecar(Path::new("/cache/foo.tar.gz"), PROVISIONED_SUFFIX),
            PathBuf::from("/cache/foo.tar.gz.provisioned")
        );
    }

    /// Build a minimal rootfs tree with a nested executable and a symlink,
    /// pack it, and extract it back through the public extract_tarball path.
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
        pack_rootfs(src.path(), &dest).unwrap();

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
            arch: "amd64".to_string(),
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
    fn pack_failure_leaves_no_partial_file() {
        // A nonexistent rootfs must fail cleanly and clean up the .partial
        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join("x.tar.gz");
        assert!(pack_rootfs(Path::new("/nonexistent-rootfs"), &dest).is_err());
        assert!(!dest.exists());
        assert!(!dest.with_extension("partial").exists());
    }
}
