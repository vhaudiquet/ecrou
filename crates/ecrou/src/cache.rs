//! The rootfs tarball cache: entries and freshness.
//!
//! One directory of content-addressed-ish entries: every downloaded
//! rootfs lives under a name derived from its source, an advisory lock
//! keeps concurrent runs from downloading the same image twice, and a
//! digest sidecar lets floating tags be re-checked cheaply. The
//! lifecycle that drives it — prepare, pull — lives in
//! [`crate::rootfs`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::distro::{map_arch, Distro, ImageSource};
use crate::download::digest_sidecar;
use crate::extract::extract_tarball;
use crate::progress::Progress;
use crate::registry::fetch_oci_digest;
use crate::rootfs::PreparedRootfs;
use crate::Result;

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
    /// Block until the entry is free, telling the sink what the run
    /// is waiting on (a silent block reads as a hang). `no_cache`
    /// callers skip the lock entirely: an explicit fresh download
    /// never waits.
    pub(crate) fn acquire(entry: &Path, progress: &dyn Progress) -> anyhow::Result<Self> {
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
            progress.message(&format!(
                "Waiting for another ecrou run to finish preparing {}",
                entry.display()
            ));
            let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if held != 0 {
                return Err(anyhow::Error::from(std::io::Error::last_os_error())
                    .context(format!("locking {}", lock_path.display())));
            }
        }
        Ok(EntryLock { _file: file })
    }
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
}

pub(crate) fn extract_prepared(
    source: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<PreparedRootfs> {
    let temp = tempfile::tempdir().context("Failed to create temporary directory")?;
    let rootfs = temp.path().to_path_buf();

    log::debug!("Extracting to: {}", rootfs.display());
    extract_tarball(source, &rootfs, progress)?;
    provision_resolver(&rootfs);

    Ok(PreparedRootfs { rootfs: temp })
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
            // Pure naming: no URL resolution, no network, ever — a
            // path derivation is a string shape, not an I/O decision.
            // The version here is whatever the caller carries
            // (resolve_source resolves before prepare, so internal
            // paths always name numbers). Dist artifacts are gzipped
            // tarballs; a new artifact format changes extraction and
            // this line together.
            format!(
                "{}-{}-{}.tar.gz",
                distro_name,
                version.as_deref().unwrap_or("latest"),
                distro_arch,
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
            // Size and mtime key the entry: an edited archive lands
            // in a fresh entry name, never a stale one.
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
}
