//! The rootfs tarball cache: entries, freshness, and provisioned layers.
//!
//! One directory of content-addressed-ish entries: every downloaded or
//! provisioned rootfs lives under a name derived from its source, an
//! advisory lock keeps concurrent runs from downloading the same image
//! twice, a digest sidecar lets floating tags be re-checked cheaply,
//! and layer pointers name provisioned entries so runs can boot them
//! without re-deriving keys. The lifecycle that drives it — prepare,
//! provision, persist — lives in [`crate::rootfs`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::distro::{map_arch, resolve_distro_url, resolve_source, Distro, ImageSource};
use crate::download::{digest_sidecar, download_image};
use crate::error::Error;
use crate::extract::extract_tarball;
use crate::progress::Progress;
use crate::registry::fetch_oci_digest;
use crate::rootfs::PreparedRootfs;
use crate::Result;

/// Sidecar suffix marking a cache entry that holds a provisioned rootfs.
pub(crate) const PROVISIONED_SUFFIX: &str = "provisioned";

/// Download `source` into the cache at `cache_path`, and drop any
/// provisioned marker: whatever was provisioned belonged to the entry
/// this download replaces, and a stale marker would present the
/// pristine image as provisioned and skip the next provisioning for
/// good.
pub(crate) fn fill_entry(
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
pub(crate) fn oci_freshness_changed(source: &ImageSource, cache_path: &Path) -> bool {
    if !cache_path.exists() {
        return false; // cache absent — the missing-file check triggers the download
    }
    let ImageSource::OciImage {
        registry,
        repository,
        tag,
        digest,
        ..
    } = source
    else {
        return false;
    };
    // A digest pin is immutable by definition; so is any non-latest tag.
    if digest.is_some() || tag != "latest" {
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
pub(crate) struct EntryLock {
    _file: std::fs::File,
}

impl EntryLock {
    /// Block until the entry is free. `no_cache` callers skip the
    /// lock entirely: an explicit fresh download never waits.
    pub(crate) fn acquire(entry: &Path) -> anyhow::Result<Self> {
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
pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// The image tarball cache (default location: `~/.cache/ecrou`).
#[derive(Debug, Clone)]
pub struct RootfsCache {
    pub(crate) dir: PathBuf,
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
    /// each other, and evict separately. The key is validated here, at
    /// the splice point, so no caller can derive a path outside the
    /// cache directory.
    pub fn entry_path(
        &self,
        source: &ImageSource,
        arch: &str,
        layer_key: Option<&str>,
    ) -> Result<PathBuf> {
        match layer_key {
            None => Ok(self.tarball_path(source, arch)),
            Some(key) => {
                validate_layer_name(key)?;
                let filename = generate_cache_filename(source, arch);
                let stem = filename
                    .rsplit_once('.')
                    .map(|(stem, _)| stem)
                    .unwrap_or(&filename);
                Ok(self.dir.join(format!("{stem}.{key}.tar.gz")))
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
        self.entry_path(&source, arch, layer_key)
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
}

pub(crate) fn extract_prepared(
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
            // The extension comes from the URL. `version` is the
            // resolved value (resolve_source ran first), so this
            // derivation is offline: numbers and patch-resolved
            // strings pass through without lookups. A resolution
            // failure here degrades to the plain .tar.gz name rather
            // than blocking every cache-path computation.
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
            digest,
            architecture,
        } => {
            // Sanitize for filename. A digest pin replaces the tag in
            // the name: two references differing only by pin must not
            // share an entry.
            let safe_registry = registry.replace(['.', ':'], "_");
            let safe_repo = repository.replace(['/', ':'], "_");
            let reference = digest.as_deref().unwrap_or(tag).replace(':', "_");
            format!(
                "oci-{}-{}-{}-{}.tar.gz",
                safe_registry, safe_repo, reference, architecture
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn direct_tarball(distro: Distro, version: &str) -> ImageSource {
        ImageSource::DirectTarball {
            distro,
            version: Some(version.to_string()),
        }
    }

    #[test]
    fn cache_filename_direct_tarball() {
        // Explicit version, already resolved to a number: no network lookups
        let cache = RootfsCache::new("/cache");
        assert_eq!(
            cache.tarball_path(&direct_tarball(Distro::Ubuntu, "24.04"), "amd64"),
            PathBuf::from("/cache/ubuntu-24.04-amd64.tar.gz")
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
            digest: None,
            architecture: "amd64".to_string(),
        };
        assert_eq!(
            cache.tarball_path(&source, "amd64"),
            PathBuf::from("/cache/oci-docker_io-library_ubuntu-latest-amd64.tar.gz")
        );
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

    #[test]
    fn a_digest_pin_names_its_own_cache_entry() {
        let cache = RootfsCache::new(std::path::PathBuf::from("/cache"));
        let tagged = crate::distro::parse_image_ref("ubuntu:24.04", "amd64").unwrap();
        let pinned = crate::distro::parse_image_ref(
            "ubuntu:24.04@sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "amd64",
        )
        .unwrap();
        assert_ne!(
            cache.tarball_path(&tagged, "amd64"),
            cache.tarball_path(&pinned, "amd64"),
            "a pin must not share the tag's entry"
        );
    }

    #[test]
    fn a_digest_pin_is_never_checked_for_freshness() {
        // Even with a :latest tag riding along, the pin wins: this
        // must return without any registry contact (it would hang the
        // test suite offline if it tried).
        let pinned = crate::distro::parse_image_ref(
            "ubuntu:latest@sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "amd64",
        )
        .unwrap();
        let existing = tempfile::NamedTempFile::new().unwrap();
        assert!(!oci_freshness_changed(&pinned, existing.path()));
    }

    #[test]
    fn a_layer_key_splices_into_the_entry_name() {
        let cache = RootfsCache::new(std::path::PathBuf::from("/cache"));
        let source = crate::distro::parse_image_ref("alpine:3.23", "amd64").expect("parses");
        let plain = cache.entry_path(&source, "amd64", None).unwrap();
        let keyed = cache.entry_path(&source, "amd64", Some("abc123")).unwrap();
        let other = cache.entry_path(&source, "amd64", Some("def456")).unwrap();
        eprintln!("plain={plain:?} keyed={keyed:?} other={other:?}");
        assert_ne!(plain, keyed, "the key changes the entry");
        assert_ne!(keyed, other, "two keys are two entries");
    }

    #[test]
    fn a_path_derivation_never_escapes_the_cache_directory() {
        // The splice point validates: even a consumer that only
        // derives paths (never writes) gets the named error instead
        // of a path outside the cache directory.
        let cache = RootfsCache::new("/cache");
        let err = cache
            .resolved_entry_path("alpine:3.23", "amd64", Some("../x"))
            .unwrap_err();
        assert!(err.to_string().contains("invalid layer name"), "{err}");
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
