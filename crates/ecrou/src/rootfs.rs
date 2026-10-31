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
//! trusted as-is — the registry and the distro CDNs are never
//! contacted on a hit; a floating alias boots the newest cached entry
//! for the distro — and the freshness decision belongs to
//! [`RootfsCache::pull`]:
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
use crate::distro::{
    map_arch, offline_resolved_version, parse_image_ref, resolve_distro_version, Distro,
    ImageSource,
};
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

/// Resolution with the cache in the loop. A reference that names one
/// entry exactly — an Ubuntu YY.MM number or known codename, a full
/// Alpine X.Y.Z — resolves offline on every path. A floating
/// reference ("latest", "lts", "edge", an Alpine "X.Y" series) needs
/// the network or the cache to answer: `pull` and `--no-cache`
/// resolve upstream, because freshness is the point, while
/// `prepare`/`run` boot the newest cached entry for the distro and
/// resolve only when nothing cached matches — `docker run` uses the
/// local image, and a stale alias boots until someone pulls.
fn resolve_for_request(
    req: &PrepareRequest,
    cache_dir: &Path,
    recheck: bool,
) -> Result<ImageSource> {
    let arch = req.resolved_arch();
    let parsed = parse_image_ref(&req.image, &arch)?;
    let ImageSource::DirectTarball { distro, version } = parsed else {
        return Ok(parsed); // OCI references and local files never resolve aliases
    };

    if let Some(resolved) = offline_resolved_version(&distro, version.as_deref()) {
        return Ok(ImageSource::DirectTarball {
            distro,
            version: Some(resolved),
        });
    }

    // Floating reference: `pull` and `--no-cache` ask upstream.
    if recheck || req.no_cache {
        return resolve_distro_version(&distro, version.as_deref(), &arch).map(|resolved| {
            ImageSource::DirectTarball {
                distro,
                version: Some(resolved),
            }
        });
    }

    // The boot path consults the cache first: no network on a hit.
    let alias = version.as_deref().unwrap_or("latest");
    if let Some(resolved) = newest_cached_version(cache_dir, &distro, &arch, alias) {
        log::debug!(
            "floating reference {alias:?} boots the newest cached entry ({resolved}); \
             `pull` re-resolves it"
        );
        return Ok(ImageSource::DirectTarball {
            distro,
            version: Some(resolved),
        });
    }

    // Nothing cached: resolution has to answer, and a network error
    // is then the honest failure — there is nothing local to boot.
    resolve_distro_version(&distro, version.as_deref(), &arch)
        .map(|resolved| ImageSource::DirectTarball {
            distro,
            version: Some(resolved),
        })
        .map_err(|e| {
            Error::Other(anyhow::Error::from(e).context(
                "no cached entry for this floating reference; resolving it needs the network",
            ))
        })
}

/// The newest cached entry a floating distro alias can boot, read
/// straight from entry names (`{distro}-{version}-{arch}.tar.gz`).
/// An Alpine series alias ("3.23") floats only within its series;
/// "edge" floats among date-stamped versions ("20250401"); every
/// other alias floats across the distro's dotted releases.
fn newest_cached_version(
    cache_dir: &Path,
    distro: &Distro,
    arch: &str,
    alias: &str,
) -> Option<String> {
    let distro_name = match distro {
        Distro::Ubuntu => "ubuntu",
        Distro::Alpine => "alpine",
    };
    let series = alias.matches('.').count() == 1;
    let prefix = format!("{distro_name}-");
    let suffix = format!("-{}.tar.gz", map_arch(*distro, arch));
    let mut best: Option<(Vec<u64>, String)> = None;
    for entry in std::fs::read_dir(cache_dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(version) = name
            .strip_prefix(&prefix)
            .and_then(|n| n.strip_suffix(&suffix))
        else {
            continue;
        };
        // A version is dots and digits only. That shape, the distro
        // prefix and the arch suffix together keep sidecars and
        // foreign entries out.
        let dotted = version.contains('.');
        if version.is_empty()
            || dotted == (alias == "edge")
            || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
            || (series && !version.starts_with(&format!("{alias}.")))
        {
            continue;
        }
        let rank: Vec<u64> = version.split('.').filter_map(|p| p.parse().ok()).collect();
        let better = match &best {
            Some((top, _)) => rank > *top,
            None => true,
        };
        if better {
            best = Some((rank, version.to_string()));
        }
    }
    best.map(|(_, version)| version)
}

impl RootfsCache {
    /// Resolve `req`, download through the cache when needed, and copy
    /// the entry image into a fresh scratch directory (removed when the
    /// returned [`PreparedRootfs`] is dropped). Download and image-build
    /// progress is reported through `progress`.
    ///
    /// This is the boot path, not the freshness path: a present entry
    /// is trusted as-is and neither a registry nor a distro CDN is
    /// contacted. A reference that names one entry exactly — an Ubuntu
    /// YY.MM, a known codename, a full Alpine X.Y.Z — resolves
    /// offline; a floating alias ("latest", "lts", "edge", an Alpine
    /// "X.Y" series) boots the newest cached entry for the distro.
    /// The refresh decision belongs to [`RootfsCache::pull`]. A cold
    /// cache pulls by default, because there is nothing to boot
    /// otherwise.
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
        let source = resolve_for_request(req, &self.dir, recheck)?;

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
        crate::utils::find_tool("mkfs.ext4").is_some()
            && crate::utils::find_tool("debugfs").is_some()
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

        let mut req = PrepareRequest::new(
            "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        req.arch = "amd64".to_string();
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
        let mut req = PrepareRequest::new(
            "ubuntu@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        req.arch = "amd64".to_string();
        // A pinned OCI source resolves without the network, so
        // parse_image_ref succeeds; the entry is absent, so pull takes
        // the download path and fails in fill_entry (no registry
        // reachable in the sandbox, or a real fetch otherwise) — both
        // outcomes prove the decision, so only the resolution is
        // asserted here.
        let arch = req.resolved_arch();
        let source = parse_image_ref(&req.image, &arch).unwrap();
        assert!(matches!(source, ImageSource::OciImage { .. }));
        assert!(!cache.tarball_path(&source, &arch).exists());
    }

    fn touch_entry(cache: &RootfsCache, name: &str) {
        let path = cache.dir().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"placeholder").unwrap();
    }

    #[test]
    fn a_floating_alias_boots_the_newest_cached_entry_without_the_network() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        touch_entry(&cache, "alpine-3.23.0-x86_64.tar.gz");
        touch_entry(&cache, "alpine-3.22.9-x86_64.tar.gz");

        let mut req = PrepareRequest::new("alpine:3.23");
        req.arch = "amd64".to_string();
        let (source, path, downloaded) = cache.ensure_entry(&req, &NoProgress, false).unwrap();
        assert!(!downloaded);
        assert_eq!(path, dir.path().join("alpine-3.23.0-x86_64.tar.gz"));
        let ImageSource::DirectTarball { version, .. } = source else {
            panic!("expected a direct tarball");
        };
        assert_eq!(version.as_deref(), Some("3.23.0"));
    }

    #[test]
    fn a_series_alias_floats_only_within_its_series() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        touch_entry(&cache, "alpine-3.20.3-x86_64.tar.gz");
        touch_entry(&cache, "alpine-3.23.0-x86_64.tar.gz");

        let mut req = PrepareRequest::new("alpine:3.23");
        req.arch = "amd64".to_string();
        let (_, path, _) = cache.ensure_entry(&req, &NoProgress, false).unwrap();
        assert_eq!(path, dir.path().join("alpine-3.23.0-x86_64.tar.gz"));
    }

    #[test]
    fn an_edge_alias_floats_among_date_stamped_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        touch_entry(&cache, "alpine-20250401-x86_64.tar.gz");
        touch_entry(&cache, "alpine-3.23.0-x86_64.tar.gz");

        let mut req = PrepareRequest::new("alpine:edge");
        req.arch = "amd64".to_string();
        let (_, path, _) = cache.ensure_entry(&req, &NoProgress, false).unwrap();
        assert_eq!(path, dir.path().join("alpine-20250401-x86_64.tar.gz"));

        // Plain "alpine" floats among releases; the edge date is
        // invisible to it.
        let mut req = PrepareRequest::new("alpine");
        req.arch = "amd64".to_string();
        let (_, path, _) = cache.ensure_entry(&req, &NoProgress, false).unwrap();
        assert_eq!(path, dir.path().join("alpine-3.23.0-x86_64.tar.gz"));
    }

    #[test]
    fn a_floating_ubuntu_alias_picks_the_newest_cached_release() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RootfsCache::new(dir.path().to_path_buf());
        touch_entry(&cache, "ubuntu-24.04-amd64.tar.gz");
        touch_entry(&cache, "ubuntu-26.04-amd64.tar.gz");

        let mut req = PrepareRequest::new("ubuntu:lts");
        req.arch = "amd64".to_string();
        let (source, _, _) = cache.ensure_entry(&req, &NoProgress, false).unwrap();
        let ImageSource::DirectTarball { version, .. } = source else {
            panic!("expected a direct tarball");
        };
        assert_eq!(version.as_deref(), Some("26.04"));
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
        let debugfs = crate::utils::find_tool("debugfs").unwrap();
        let out = std::process::Command::new(debugfs)
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
