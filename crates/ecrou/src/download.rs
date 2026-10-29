//! Download plumbing shared by both image sources: the digest hasher,
//! the shared HTTP client, distro checksum files, and plain file
//! transfers into the cache.

use crate::progress::{Progress, Unit};
use anyhow::{anyhow, Context};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256, Sha512};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::distro::{Distro, ImageSource};
use crate::Result;

/// One HTTP client shape for the crate: a connect window, plus an
/// optional total request window. Metadata callers (checksum files,
/// token endpoints, version lookups) pass `Some` — small bodies, so a
/// total bound is safe. Transfer callers pass `None`: a slow transfer
/// of any size must finish, and the blocking client offers no per-read
/// window — a stalled stream is the operator's Ctrl-C.
pub(crate) fn http_client(request_window: Option<Duration>) -> anyhow::Result<Client> {
    let mut builder = Client::builder().connect_timeout(Duration::from_secs(30));
    if let Some(window) = request_window {
        builder = builder.timeout(window);
    }
    builder.build().context("Failed to create HTTP client")
}

/// Path of the digest sidecar file for a cached OCI image.
/// e.g. `foo.tar.gz` → `foo.tar.gz.digest`
pub fn digest_sidecar(cache_path: &Path) -> PathBuf {
    let mut s = cache_path.as_os_str().to_owned();
    s.push(".digest");
    PathBuf::from(s)
}

/// Incremental sha256+sha512 hasher used to verify downloaded content against
/// content-addressed digests.  Both algorithms run side by side so the caller
/// can check whatever digest the manifest or checksum file names.
pub(crate) struct DownloadHasher {
    sha256: Sha256,
    sha512: Sha512,
}

impl DownloadHasher {
    pub(crate) fn new() -> Self {
        DownloadHasher {
            sha256: Sha256::new(),
            sha512: Sha512::new(),
        }
    }

    pub(crate) fn update(&mut self, chunk: &[u8]) {
        self.sha256.update(chunk);
        self.sha512.update(chunk);
    }

    /// Verify the streamed content.  `expected` is an OCI `algo:hex` digest or
    /// a bare sha256 hex string (distro checksum files); None verifies nothing.
    /// Algorithms outside sha256/sha512 cannot be checked: warn and accept
    /// rather than reject images a registry may legitimately serve.
    pub(crate) fn verify(&self, expected: Option<&str>) -> anyhow::Result<()> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let (algo, hex) = match expected.split_once(':') {
            Some((algo, hex)) => (algo, hex),
            None => ("sha256", expected),
        };

        let actual = match algo {
            "sha256" => hex_encode(&self.sha256.clone().finalize()),
            "sha512" => hex_encode(&self.sha512.clone().finalize()),
            other => {
                log::warn!(
                    "digest algorithm '{}' not supported; skipping verification",
                    other
                );
                return Ok(());
            }
        };

        if actual.eq_ignore_ascii_case(hex) {
            Ok(())
        } else {
            Err(anyhow!(
                "content verification failed: expected {} {}, got {}",
                algo,
                hex,
                actual
            ))
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Download a container image (either direct tarball or OCI image),
/// reporting transfer progress through `progress`
pub fn download_image(
    source: &ImageSource,
    dest: &Path,
    arch: &str,
    progress: &dyn Progress,
) -> Result<()> {
    match source {
        ImageSource::DirectTarball { distro, version } => {
            let url = crate::distro::resolve_distro_url(distro, version.as_deref(), arch)?;
            log::debug!("Resolved URL: {}", url);
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(anyhow!("Unsupported URL scheme: {}", url).into());
            }
            // Verify the tarball against the checksum the distro publishes
            // next to it.  A fetch failure (missing artifact, network error)
            // degrades to an unverified download with a warning rather than
            // hard-failing, mirroring the digest freshness check in rootfs.
            let expected = match fetch_published_checksum(distro, &url) {
                Ok(expected) => Some(expected),
                Err(e) => {
                    log::warn!(
                        "could not fetch published checksum ({}); \
                         proceeding without verification",
                        e
                    );
                    None
                }
            };
            download_file(&url, dest, progress, expected.as_deref())?;
            Ok(())
        }
        ImageSource::OciImage {
            registry,
            repository,
            tag,
            architecture,
        } => {
            log::debug!("Pulling OCI image: {}/{}:{}", registry, repository, tag);
            crate::registry::download_oci_image(
                registry,
                repository,
                tag,
                architecture,
                dest,
                progress,
            )?;
            Ok(())
        }
        // A local archive is read in place by the prepare step; there
        // is nothing to fetch.
        ImageSource::LocalTarball { path } => Err(anyhow!(
            "a local rootfs archive is never downloaded: {}",
            path.display()
        )
        .into()),
    }
}

/// Fetch the sha256 the distro publishes for the tarball at `tarball_url`.
fn fetch_published_checksum(distro: &Distro, tarball_url: &str) -> anyhow::Result<String> {
    let Some(checksum_url) = crate::distro::checksum_artifact_url(distro, tarball_url) else {
        return Err(anyhow!("no checksum artifact for {}", tarball_url));
    };

    let client = http_client(Some(Duration::from_secs(30)))?;
    let response = client
        .get(&checksum_url)
        .send()
        .with_context(|| format!("Failed to fetch checksum artifact {}", checksum_url))?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "checksum artifact {} returned HTTP {}",
            checksum_url,
            response.status()
        ));
    }

    let text = response.text()?;
    let filename = tarball_url.rsplit('/').next().unwrap_or("");
    expected_digest_from_checksums(&text, filename).ok_or_else(|| {
        anyhow!(
            "checksum artifact {} does not list {}",
            checksum_url,
            filename
        )
    })
}

/// Extract the digest for `filename` from a sha256sum-style checksum file
/// (lines of `<hex>  <name>` or `<hex> *<name>`).
fn expected_digest_from_checksums(text: &str, filename: &str) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(digest), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name.trim_start_matches('*') == filename {
            return Some(digest.to_lowercase());
        }
    }
    None
}

fn download_file(
    url: &str,
    dest: &Path,
    progress: &dyn Progress,
    expected: Option<&str>,
) -> anyhow::Result<()> {
    log::debug!("Downloading: {}", url);

    let client = http_client(None)?;
    let mut response = client.get(url).send().context("Failed to start download")?;

    if !response.status().is_success() {
        return Err(anyhow!("Download failed: HTTP {}", response.status()));
    }

    let total_size = response.content_length().unwrap_or(0);

    // Create parent directories
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("Failed to create cache directory")?;
    }

    progress.begin(
        "Downloading",
        Unit::Bytes,
        (total_size > 0).then_some(total_size),
    );

    // Unique per call: concurrent runs writing the same entry each
    // complete their own file, and the last rename wins with whole
    // bytes either way.
    let temp_dest = crate::utils::partial_path(dest);

    // Download into the temp file, then atomically rename to the final path.
    // Verification runs before the rename so a corrupted download never
    // reaches the cache; on any failure after the temp file is created we
    // remove it so stale partial files don't accumulate.
    let mut hasher = DownloadHasher::new();
    let result: anyhow::Result<()> = (|| {
        let mut file =
            std::fs::File::create(&temp_dest).context("Failed to create temporary file")?;

        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = response.read(&mut buf).context("Download interrupted")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).context("Failed to write chunk")?;
            hasher.update(&buf[..n]);
            progress.advance(n as u64);
        }

        progress.finish();
        hasher.verify(expected)?;
        if expected.is_some() {
            log::debug!("Verified content digest: {}", dest.display());
        }

        std::fs::rename(&temp_dest, dest)
            .context("Failed to move partial download to final destination")?;
        log::debug!("Download complete: {}", dest.display());

        Ok(())
    })();

    if result.is_err() {
        // Best-effort cleanup; ignore errors (file may not exist if creation failed).
        let _ = std::fs::remove_file(&temp_dest);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::expected_digest_from_checksums;
    use super::DownloadHasher;

    // sha256("abc") and sha512("abc") — standard test vectors
    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                              2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    fn hasher_with(content: &[u8]) -> DownloadHasher {
        let mut hasher = DownloadHasher::new();
        hasher.update(content);
        hasher
    }

    #[test]
    fn verify_oci_digests() {
        let h = hasher_with(b"abc");
        assert!(h.verify(Some(&format!("sha256:{}", SHA256_ABC))).is_ok());
        assert!(h.verify(Some(&format!("sha512:{}", SHA512_ABC))).is_ok());
    }

    #[test]
    fn verify_bare_hex_is_sha256() {
        // distro checksum files carry a bare sha256 hex string
        let h = hasher_with(b"abc");
        assert!(h.verify(Some(SHA256_ABC)).is_ok());
    }

    #[test]
    fn verify_mismatch_errors() {
        let h = hasher_with(b"abc");
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(h.verify(Some(&wrong)).is_err());
        // right hash, wrong algorithm name pinned to it
        let algo_mismatch = format!("sha512:{}", SHA256_ABC);
        assert!(h.verify(Some(&algo_mismatch)).is_err());
    }

    #[test]
    fn verify_none_and_unknown_algo_accept() {
        let h = hasher_with(b"abc");
        assert!(h.verify(None).is_ok());
        assert!(h.verify(Some("md5:0123456789abcdef")).is_ok());
    }

    #[test]
    fn checksum_parsing_bsd_style() {
        // Alpine's per-file .sha256: "<hex>  <filename>"
        let text = format!("{}  alpine-minirootfs-3.23.0-x86_64.tar.gz\n", SHA256_ABC);
        assert_eq!(
            expected_digest_from_checksums(&text, "alpine-minirootfs-3.23.0-x86_64.tar.gz"),
            Some(SHA256_ABC.to_string())
        );
    }

    #[test]
    fn checksum_parsing_binary_marker() {
        // Ubuntu's SHA256SUMS: "<hex> *<filename>" (binary-mode marker)
        let text = format!("{} *noble-base-amd64.tar.gz\n", SHA256_ABC);
        assert_eq!(
            expected_digest_from_checksums(&text, "noble-base-amd64.tar.gz"),
            Some(SHA256_ABC.to_string())
        );
    }

    #[test]
    fn checksum_parsing_missing_and_malformed() {
        let text = format!("{} *other-file.tar.gz\nnot-a-checksum-line\n", SHA256_ABC);
        assert_eq!(
            expected_digest_from_checksums(&text, "noble-base-amd64.tar.gz"),
            None
        );
        assert_eq!(expected_digest_from_checksums("", "x"), None);
    }
}
