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

use crate::error::Error;
use crate::Result;
use tempfile::TempDir;

use crate::distro::{
    map_arch, parse_image_ref, resolve_distro_url, resolve_distro_version, Distro, ImageSource,
};
use crate::download::{digest_sidecar, download_image};
use crate::extract::extract_tarball;
use crate::progress::Progress;
use crate::registry::fetch_oci_digest;

/// Sidecar suffix marking a cache entry that holds a provisioned rootfs.
const PROVISIONED_SUFFIX: &str = "provisioned";

/// Download `source` into the cache at `cache_path`, and drop any
/// provisioned marker: whatever was provisioned belonged to the entry
/// this download replaces, and a stale marker would present the
/// pristine image as provisioned and skip the next provisioning for
/// good.
fn fill_entry(
    source: &ImageSource,
    cache_path: &Path,
    arch: &str,
    progress: &dyn Progress,
) -> Result<()> {
    download_image(source, cache_path, arch, progress)?;
    let marker = sidecar(cache_path, PROVISIONED_SUFFIX);
    if marker.exists() {
        std::fs::remove_file(&marker).with_context(|| {
            format!(
                "Failed to drop the stale provisioned marker {}",
                marker.display()
            )
        })?;
    }
    Ok(())
}

/// Whether an OCI image with a floating tag (":latest") has moved since
/// it was cached: fetch the current manifest digest from the registry
/// and compare it against the digest stored from the last download.
/// Pinned tags are assumed immutable; a network error keeps the cache
/// rather than hard-failing; anything else is simply current.
fn oci_freshness_changed(source: &ImageSource, cache_path: &Path) -> bool {
    if !cache_path.exists() {
        return false; // cache absent — the missing-file check triggers the download
    }
    let ImageSource::OciImage {
        registry,
        repository,
        tag,
        ..
    } = source
    else {
        return false;
    };
    if tag != "latest" {
        return false;
    }
    match fetch_oci_digest(registry, repository, tag) {
        Ok(current) => {
            let stored = std::fs::read_to_string(digest_sidecar(cache_path)).ok();
            stored.as_deref() != Some(current.trim())
        }
        Err(e) => {
            log::warn!("could not check image freshness ({}); using cache", e);
            false
        }
    }
}

/// An exclusive advisory lock on one cache entry, held while a run
/// downloads it: a second run preparing the same image blocks here and
/// then reuses the winner's entry instead of downloading a duplicate
/// the atomic rename would throw away. The lock file is a permanent
/// zero-byte sidecar; flock releases when the holder exits, crashed or
/// not.
struct EntryLock {
    _file: std::fs::File,
}

impl EntryLock {
    /// Block until the entry is free. `no_cache` callers skip the
    /// lock entirely: an explicit fresh download never waits.
    fn acquire(entry: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd;

        let lock_path = entry.with_extension("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)
            .with_context(|| format!("Failed to open the entry lock {}", lock_path.display()))?;

        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if held != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(
                    anyhow::Error::from(err).context(format!("locking {}", lock_path.display()))
                );
            }
            log::warn!(
                "waiting for another ecrou run to finish preparing {}",
                entry.display()
            );
            let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if held != 0 {
                return Err(anyhow::Error::from(std::io::Error::last_os_error())
                    .context(format!("locking {}", lock_path.display())));
            }
        }
        Ok(EntryLock { _file: file })
    }
}

/// A layer name is a file name under the cache's `layers/` directory
/// and a component of entry file names: keep it to a safe charset so a
/// name like `../x` or `a/b` cannot climb out of the cache directory.
fn validate_layer_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !name.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(Error::Other(anyhow!(
            "invalid layer name {name:?}: use letters, digits, '_' or '-', \
             not starting with '.'"
        )))
    }
}

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

    /// The cache path for a prepared entry, keyed or not: a layer key
    /// is spliced into the filename, so a layer and its base — and two
    /// layers of one base — live in separate entries, never overwrite
    /// each other, and evict separately.
    pub fn entry_path(&self, source: &ImageSource, arch: &str, layer_key: Option<&str>) -> PathBuf {
        match layer_key {
            None => self.tarball_path(source, arch),
            Some(key) => {
                let filename = generate_cache_filename(source, arch);
                let stem = filename
                    .rsplit_once('.')
                    .map(|(stem, _)| stem)
                    .unwrap_or(&filename);
                self.dir.join(format!("{stem}.{key}.tar.gz"))
            }
        }
    }

    /// Extracts a named layer into a fresh scratch directory: the boot
    /// side of `run --layer`. A missing pointer or a pruned entry is a
    /// named error, not a download — layers are provisioned, never
    /// fetched.
    pub fn extract_layer(&self, name: &str, progress: &dyn Progress) -> Result<PreparedRootfs> {
        validate_layer_name(name)?;
        let path = self.layer_entry(name)?.ok_or_else(|| {
            Error::Other(anyhow::anyhow!(
                "layer {name} is not provisioned: run \
                     `ecrou provision --spec <spec> --layer {name}` first"
            ))
        })?;
        extract_prepared(&path.clone(), path, progress).map_err(Error::Other)
    }

    /// The cache path an image resolves to, keyed or not — the same
    /// resolution `prepare` does (floating aliases resolved), so a
    /// pointer written from this path names an entry that exists.
    pub fn resolved_entry_path(
        &self,
        image: &str,
        arch: &str,
        layer_key: Option<&str>,
    ) -> Result<PathBuf> {
        let source = resolve_source(image, arch)?;
        Ok(self.entry_path(&source, arch, layer_key))
    }

    /// Writes the layer pointer: `layers/<name>` holds the cache
    /// filename the name resolves to. Consumers boot layers by name —
    /// `run` should not have to re-derive the key from the spec, only
    /// agree with the provisioning call on what the layer is called.
    pub fn write_layer_pointer(&self, name: &str, cache_filename: &str) -> Result<()> {
        validate_layer_name(name)?;
        let dir = self.dir.join("layers");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create the layer directory {}", dir.display()))?;
        // Atomic write: a concurrent run reading the pointer must
        // never see a truncated filename mid-write.
        let pointer = dir.join(name);
        let partial = crate::utils::partial_path(&pointer);
        std::fs::write(&partial, cache_filename)
            .with_context(|| format!("Failed to write the layer pointer for {name}"))?;
        std::fs::rename(&partial, &pointer)
            .with_context(|| format!("Failed to move the layer pointer into place for {name}"))?;
        Ok(())
    }

    /// The cache path a layer pointer names, and whether the layer is
    /// really there: a pointer is a name, not a promise — the entry it
    /// names can be gone (a pruned cache, a base bump) while the
    /// pointer survives.
    pub fn layer_entry(&self, name: &str) -> Result<Option<PathBuf>> {
        validate_layer_name(name)?;
        let pointer = self.dir.join("layers").join(name);
        let filename = match std::fs::read_to_string(&pointer) {
            Ok(filename) => filename,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::Other(
                    anyhow::Error::from(err)
                        .context(format!("reading the layer pointer for {name}")),
                ))
            }
        };
        let path = self.dir.join(filename.trim());
        Ok((path.exists() && sidecar(&path, PROVISIONED_SUFFIX).exists()).then_some(path))
    }

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
        if let Some(key) = &req.layer_key {
            validate_layer_name(key)?;
        }
        let cache_path = self.entry_path(&source, &arch, req.layer_key.as_deref());
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
    rootfs: TempDir,
    cache_path: PathBuf,
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
    source: &Path,
    cache_path: PathBuf,
    progress: &dyn Progress,
) -> anyhow::Result<PreparedRootfs> {
    let temp = tempfile::tempdir().context("Failed to create temporary directory")?;
    let rootfs = temp.path().to_path_buf();

    log::debug!("Extracting to: {}", rootfs.display());
    extract_tarball(source, &rootfs, progress)?;
    provision_resolver(&rootfs);

    Ok(PreparedRootfs {
        rootfs: temp,
        cache_path,
    })
}

/// Give the guest the host's resolver: the engine proxies guest
/// sockets to the host's stack, so a nameserver line that works on the
/// host works in the machine, loopback stub included. Without it the
/// guest has whatever the image shipped, which is usually nothing.
/// Best effort: a rootfs with its own resolver config keeps it.
fn provision_resolver(rootfs: &Path) {
    // The engine proxies guest sockets to the host's stack, so any
    // host-reachable nameserver works in the machine - but loopback
    // addresses do not cross: 127.0.0.53 stays inside the guest, where
    // nothing listens. So the source of truth is systemd-resolved's
    // uplink config when present, and loopback nameservers are dropped
    // from whatever is copied. Best effort: failures leave the
    // image's file.
    let host = match std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
    {
        Ok(s) if !s.trim().is_empty() => drop_loopback_nameservers(&s),
        Ok(_) => {
            log::debug!("host resolver config is empty; leaving the image's in place");
            return;
        }
        Err(e) => {
            log::debug!(
                "no host resolver config ({}); leaving the image's in place",
                e
            );
            return;
        }
    };
    if !host.contains("nameserver ") {
        log::debug!("host resolver config has no usable nameserver; leaving the image's");
        return;
    }
    let target = rootfs.join("etc/resolv.conf");
    if let Err(e) = std::fs::remove_file(&target) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::debug!("could not clear {}: {}", target.display(), e);
            return;
        }
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&target, host) {
        Ok(()) => log::debug!("provisioned a resolver into {}", target.display()),
        Err(e) => log::debug!("could not write {}: {}", target.display(), e),
    }
}

/// Lines naming a loopback nameserver go: the guest's loopback is its
/// own, and the stub resolvers live on the host's.
fn drop_loopback_nameservers(config: &str) -> String {
    config
        .lines()
        .filter(|line| {
            let rest = match line.trim_start().strip_prefix("nameserver ") {
                Some(r) => r.trim(),
                None => return true,
            };
            !rest.starts_with("127.")
        })
        .fold(String::new(), |mut acc, line| {
            acc.push_str(line);
            acc.push('\n');
            acc
        })
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
        ImageSource::LocalTarball { path } => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rootfs")
                .replace(['/', '\\', ':'], "_");
            // Size and mtime key the entry: a provisioning over a
            // local archive persists into the cache under this name,
            // never over the caller's file, and an edited archive
            // lands in a fresh entry.
            let meta = std::fs::metadata(path);
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("local-{}-{:x}-{:x}.tar.gz", stem, len, mtime)
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
    use super::drop_loopback_nameservers;

    #[test]
    fn loopback_nameservers_dropped() {
        let config = "# comment\nnameserver 127.0.0.53\noptions edns0\nnameserver 192.168.1.1\nnameserver 127.0.0.1\n";
        assert_eq!(
            drop_loopback_nameservers(config),
            "# comment\noptions edns0\nnameserver 192.168.1.1\n"
        );
        // A search line whose address-ish token is untouched.
        let config = "search 127.example\nnameserver 10.0.0.1\n";
        assert_eq!(
            drop_loopback_nameservers(config),
            "search 127.example\nnameserver 10.0.0.1\n"
        );
    }

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

    #[test]
    fn a_layer_key_splices_into_the_entry_name() {
        let cache = RootfsCache::new(std::path::PathBuf::from("/cache"));
        let source = crate::distro::parse_image_ref("alpine:3.23", "amd64").expect("parses");
        let plain = cache.entry_path(&source, "amd64", None);
        let keyed = cache.entry_path(&source, "amd64", Some("abc123"));
        let other = cache.entry_path(&source, "amd64", Some("def456"));
        eprintln!("plain={plain:?} keyed={keyed:?} other={other:?}");
        assert_ne!(plain, keyed, "the key changes the entry");
        assert_ne!(keyed, other, "two keys are two entries");
    }

    #[test]
    fn layer_names_cannot_escape_the_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        for bad in ["../evil", "a/b", "", ".", "..", ".hidden", "a\0b"] {
            assert!(
                cache.write_layer_pointer(bad, "x.tar.gz").is_err(),
                "{bad:?} must be rejected"
            );
            assert!(cache.layer_entry(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["base", "dev-tools", "ci_2026", "Layer1"] {
            assert!(
                validate_layer_name(good).is_ok(),
                "{good:?} must be accepted"
            );
        }
    }

    #[test]
    fn a_layer_pointer_resolves_only_while_the_entry_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = RootfsCache::new(dir.path().to_path_buf());
        assert!(
            cache.layer_entry("nope").expect("reads").is_none(),
            "no pointer, no entry"
        );
        cache
            .write_layer_pointer("w", "alpine-3.23.7-x86_64.tar.abc.tar.gz")
            .expect("writes");
        assert!(
            cache.layer_entry("w").expect("reads").is_none(),
            "a pointer without its entry resolves to nothing"
        );
        std::fs::write(
            dir.path().join("alpine-3.23.7-x86_64.tar.abc.tar.gz"),
            b"tarball",
        )
        .expect("the entry appears");
        std::fs::write(
            dir.path()
                .join("alpine-3.23.7-x86_64.tar.abc.tar.gz.provisioned"),
            b"",
        )
        .expect("the marker appears");
        assert!(
            cache.layer_entry("w").expect("reads").is_some(),
            "pointer + entry + marker resolve"
        );
    }
}
