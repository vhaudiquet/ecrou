use crate::progress::{Progress, Unit};
use anyhow::{anyhow, Context};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::distro::{Distro, ImageSource};
use crate::{Error, Result};

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
struct DownloadHasher {
    sha256: Sha256,
    sha512: Sha512,
}

impl DownloadHasher {
    fn new() -> Self {
        DownloadHasher {
            sha256: Sha256::new(),
            sha512: Sha512::new(),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        self.sha256.update(chunk);
        self.sha512.update(chunk);
    }

    /// Verify the streamed content.  `expected` is an OCI `algo:hex` digest or
    /// a bare sha256 hex string (distro checksum files); None verifies nothing.
    /// Algorithms outside sha256/sha512 cannot be checked: warn and accept
    /// rather than reject images a registry may legitimately serve.
    fn verify(&self, expected: Option<&str>) -> anyhow::Result<()> {
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

/// Fetch the current manifest digest for an OCI tag without downloading any
/// layers.  The registry returns the content-addressable digest in the
/// `Docker-Content-Digest` response header of any manifest GET.
pub fn fetch_oci_digest(registry: &str, repository: &str, tag: &str) -> Result<String> {
    let digest = tokio::runtime::Runtime::new()
        .context("Failed to create Tokio runtime")?
        .block_on(fetch_oci_digest_async(registry, repository, tag))?;
    Ok(digest)
}

async fn fetch_oci_digest_async(
    registry: &str,
    repository: &str,
    tag: &str,
) -> anyhow::Result<String> {
    // Metadata call: a small body, so the read window alone bounds it.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")?;

    let token = get_auth_token(&client, registry, repository).await?;

    let manifest_url = if registry == "docker.io" {
        format!(
            "https://registry-1.docker.io/v2/{}/manifests/{}",
            repository, tag
        )
    } else {
        format!("https://{}/v2/{}/manifests/{}", registry, repository, tag)
    };

    let response = client
        .get(&manifest_url)
        .header(
            "Accept",
            "application/vnd.docker.distribution.manifest.list.v2+json, \
             application/vnd.docker.distribution.manifest.v2+json, \
             application/vnd.oci.image.index.v1+json, \
             application/vnd.oci.image.manifest.v1+json",
        )
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch manifest for digest check")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Manifest fetch returned HTTP {}",
            response.status()
        ));
    }

    response
        .headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Registry did not return a Docker-Content-Digest header"))
}

/// Download a container image (either direct tarball or OCI image),
/// reporting transfer progress through `progress`
pub fn download_image(
    source: &ImageSource,
    dest: &Path,
    arch: &str,
    progress: &dyn Progress,
) -> Result<()> {
    // Create a single Tokio runtime for all async I/O in this download.
    // Previously each of the two sync wrappers (download_file_sync and
    // download_oci_image) created their own runtime; this consolidates them.
    let rt = tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;

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
            let expected = match rt.block_on(fetch_published_checksum(distro, &url)) {
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
            Ok(rt.block_on(download_file_async(
                &url,
                dest,
                progress,
                expected.as_deref(),
            ))?)
        }
        ImageSource::OciImage {
            registry,
            repository,
            tag,
            architecture,
        } => {
            log::debug!("Pulling OCI image: {}/{}:{}", registry, repository, tag);
            Ok(rt.block_on(download_oci_image_async(
                registry,
                repository,
                tag,
                architecture,
                dest,
                progress,
            ))?)
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
async fn fetch_published_checksum(distro: &Distro, tarball_url: &str) -> anyhow::Result<String> {
    let Some(checksum_url) = crate::distro::checksum_artifact_url(distro, tarball_url) else {
        return Err(anyhow!("no checksum artifact for {}", tarball_url));
    };

    // Metadata call: a small body, so the read window alone bounds it.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(&checksum_url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch checksum artifact {}", checksum_url))?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "checksum artifact {} returned HTTP {}",
            checksum_url,
            response.status()
        ));
    }

    let text = response.text().await?;
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

async fn download_file_async(
    url: &str,
    dest: &Path,
    progress: &dyn Progress,
    expected: Option<&str>,
) -> anyhow::Result<()> {
    log::debug!("Downloading: {}", url);

    // No total timeout: a slow transfer of any size must finish. The
    // connect window and the per-read window catch dead peers and
    // stalled streams instead.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(url)
        .send()
        .await
        .context("Failed to start download")?;

    if !response.status().is_success() {
        return Err(anyhow!("Download failed: HTTP {}", response.status()));
    }

    let total_size = response.content_length().unwrap_or(0);

    // Create parent directories
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("Failed to create cache directory")?;
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
    let result: anyhow::Result<()> = async {
        let mut file = tokio::fs::File::create(&temp_dest)
            .await
            .context("Failed to create temporary file")?;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Download interrupted")?;
            tokio::io::copy(&mut &chunk[..], &mut file).await?;
            hasher.update(&chunk);
            progress.advance(chunk.len() as u64);
        }

        progress.finish();
        hasher.verify(expected)?;
        if expected.is_some() {
            log::debug!("Verified content digest: {}", dest.display());
        }

        tokio::fs::rename(&temp_dest, dest)
            .await
            .context("Failed to move partial download to final destination")?;
        log::debug!("Download complete: {}", dest.display());

        Ok(())
    }
    .await;

    if result.is_err() {
        // Best-effort cleanup; ignore errors (file may not exist if creation failed).
        let _ = tokio::fs::remove_file(&temp_dest).await;
    }

    result
}

async fn download_oci_image_async(
    registry: &str,
    repository: &str,
    tag: &str,
    arch: &str,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    // No total timeout: a slow transfer of any size must finish. The
    // connect window and the per-read window catch dead peers and
    // stalled streams instead.
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("Failed to create HTTP client")?;

    let digest =
        try_download_oci_image(&client, registry, repository, tag, arch, dest, progress).await?;

    // Persist the manifest digest so the next run can skip the download when
    // the tag hasn't changed (see fetch_oci_digest / digest_sidecar).
    if !digest.is_empty() {
        let _ = std::fs::write(digest_sidecar(dest), &digest);
    }

    Ok(())
}

async fn try_download_oci_image(
    client: &Client,
    registry: &str,
    repository: &str,
    tag: &str,
    arch: &str,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<String> {
    // Get authentication token for the registry
    let token = get_auth_token(client, registry, repository).await?;

    // Construct manifest URL based on registry
    let manifest_url = if registry == "docker.io" {
        // Docker Hub uses registry-1.docker.io for API
        format!(
            "https://registry-1.docker.io/v2/{}/manifests/{}",
            repository, tag
        )
    } else {
        format!("https://{}/v2/{}/manifests/{}", registry, repository, tag)
    };

    log::debug!("Fetching manifest from: {}", manifest_url);

    // Request both manifest list and single manifest types (including OCI formats)
    let response = client
        .get(&manifest_url)
        .header("Accept", "application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to get image manifest")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to get manifest: HTTP {}",
            response.status()
        ));
    }

    // Capture the tag-level digest before consuming the response body.
    // This is the content-addressable identity of the manifest (or manifest
    // list) at this tag — used to detect whether the tag has moved since the
    // last download.
    let tag_digest = response
        .headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let body = response.text().await?;
    let manifest: Value = serde_json::from_str(&body).context("Failed to parse manifest JSON")?;

    // Check if this is a manifest list (multi-arch)
    let layers = if let Some(manifests) = manifest["manifests"].as_array() {
        // This is a manifest list - find the right architecture
        log::debug!("Got manifest list with {} manifests", manifests.len());

        let manifest_entry = manifests
            .iter()
            .find(|m| {
                let m_arch = m["platform"]["architecture"].as_str().unwrap_or("");
                let m_os = m["platform"]["os"].as_str().unwrap_or("");
                m_arch == arch && m_os == "linux"
            })
            .ok_or_else(|| {
                // The typed variant rides at the top of the chain, so it
                // survives into `crate::Error` for consumers to match on.
                let available: Vec<&str> = manifests
                    .iter()
                    .filter_map(|m| m["platform"]["architecture"].as_str())
                    .collect();
                anyhow::Error::new(Error::ArchNotAvailable {
                    arch: arch.to_string(),
                    available: available.join(", "),
                })
            })?;

        let manifest_digest = manifest_entry["digest"]
            .as_str()
            .ok_or_else(|| anyhow!("No digest in manifest entry"))?;

        log::debug!("Found manifest digest: {}", manifest_digest);

        // Now get the actual manifest for this architecture
        let arch_manifest_url = if registry == "docker.io" {
            format!(
                "https://registry-1.docker.io/v2/{}/manifests/{}",
                repository, manifest_digest
            )
        } else {
            format!(
                "https://{}/v2/{}/manifests/{}",
                registry, repository, manifest_digest
            )
        };

        let arch_response = client
            .get(&arch_manifest_url)
            .header("Accept", "application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json")
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .context("Failed to get architecture manifest")?;

        if !arch_response.status().is_success() {
            return Err(anyhow!(
                "Failed to get arch manifest: HTTP {}",
                arch_response.status()
            ));
        }

        let arch_body = arch_response.text().await?;

        // The arch manifest was requested by digest: verify the response
        // matches, since the layer digests read from it are trusted next.
        let mut manifest_hasher = DownloadHasher::new();
        manifest_hasher.update(arch_body.as_bytes());
        manifest_hasher
            .verify(Some(manifest_digest))
            .context("architecture manifest failed digest verification")?;

        let arch_manifest: Value = serde_json::from_str(&arch_body)
            .context("Failed to parse architecture manifest JSON")?;

        // Get all layers from architecture manifest (in order)
        arch_manifest["layers"]
            .as_array()
            .ok_or_else(|| anyhow!("No layers in architecture manifest"))?
            .clone()
    } else {
        // Single architecture manifest - get all layers in order
        manifest["layers"]
            .as_array()
            .ok_or_else(|| anyhow!("No layers in manifest"))?
            .clone()
    };

    log::debug!("Found {} layers to download", layers.len());

    // Download all layer blobs into a temp directory
    let temp_dir = tempfile::tempdir()?;

    // layer_names[i] is the filename used both for the downloaded blob and in
    // layers.manifest.  The extension is derived from the layer's mediaType so
    // extract_oci_layer can dispatch on the filename without relying on
    // magic-byte detection (which would misfire on a zstd blob named .tar.gz).
    let mut layer_names: Vec<String> = Vec::with_capacity(layers.len());

    for (i, layer) in layers.iter().enumerate() {
        let layer_digest = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow!("No digest in layer"))?;

        let media_type = layer["mediaType"].as_str().unwrap_or("");
        let ext = media_type_to_extension(media_type);
        let layer_name = format!("layer_{}.{}", i, ext);

        log::debug!(
            "Fetching layer {}/{}: {}",
            i + 1,
            layers.len(),
            layer_digest
        );

        let blob_url = if registry == "docker.io" {
            format!(
                "https://registry-1.docker.io/v2/{}/blobs/{}",
                repository, layer_digest
            )
        } else {
            format!(
                "https://{}/v2/{}/blobs/{}",
                registry, repository, layer_digest
            )
        };

        let layer_path = temp_dir.path().join(&layer_name);
        let label = format!("Layer {}/{} ({})", i + 1, layers.len(), layer_name);
        download_blob_with_auth(
            client,
            &blob_url,
            &layer_path,
            &token,
            &label,
            layer_digest,
            progress,
        )
        .await?;
        layer_names.push(layer_name);
    }

    // Write the layer index so extract.rs knows the order
    {
        use std::io::Write;
        let mut manifest_file = std::fs::File::create(temp_dir.path().join("layers.manifest"))?;
        for name in &layer_names {
            writeln!(manifest_file, "{}", name)?;
        }
    }

    // Bundle layers.manifest + all layer blobs into a single .tar.gz cache file
    // using the tar + flate2 crates — no external `tar` binary required.
    let temp_dest = crate::utils::partial_path(dest);

    let bundle_result: anyhow::Result<()> = (|| {
        use flate2::{write::GzEncoder, Compression};

        let out_file =
            std::fs::File::create(&temp_dest).context("Failed to create temporary bundle file")?;
        let mut builder = tar::Builder::new(GzEncoder::new(out_file, Compression::default()));

        builder
            .append_path_with_name(temp_dir.path().join("layers.manifest"), "layers.manifest")
            .context("Failed to add layers.manifest to bundle")?;

        for name in &layer_names {
            builder
                .append_path_with_name(temp_dir.path().join(name), name)
                .with_context(|| format!("Failed to add {} to bundle", name))?;
        }

        // into_inner finalises the tar end-of-archive marker and returns the
        // GzEncoder; finish() flushes and closes the gzip stream.
        builder
            .into_inner()
            .context("Failed to finalise tar archive")?
            .finish()
            .context("Failed to finalise gzip stream")?;

        std::fs::rename(&temp_dest, dest).context("Failed to move bundle to cache destination")
    })();

    if bundle_result.is_err() {
        // Best-effort cleanup; ignore errors (file may not exist if creation failed).
        let _ = std::fs::remove_file(&temp_dest);
    }
    bundle_result?;

    Ok(tag_digest)
}

async fn get_auth_token(
    client: &Client,
    registry: &str,
    repository: &str,
) -> anyhow::Result<String> {
    // Docker Hub has a well-known, stable auth endpoint.
    if registry == "docker.io" {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            repository
        );
        log::debug!("Getting auth token from: {}", url);
        return fetch_bearer_token(client, &url).await;
    }

    // For every other registry follow the OCI Distribution Spec §4.2:
    //   1. Probe GET /v2/ unauthenticated.
    //   2. Read the WWW-Authenticate challenge from the 401 response.
    //   3. Build the token URL from the advertised realm + service.
    let probe_url = format!("https://{}/v2/", registry);
    let probe = client
        .get(&probe_url)
        .send()
        .await
        .with_context(|| format!("Failed to probe registry at {}", probe_url))?;

    match probe.status().as_u16() {
        200 => {
            // Registry requires no authentication (e.g. local insecure registry).
            return Ok(String::new());
        }
        401 => {}
        other => {
            // Unexpected status; proceed without a token and let the manifest
            // request fail with a more descriptive error.
            log::warn!(
                "registry probe returned HTTP {}; trying without auth",
                other
            );
            return Ok(String::new());
        }
    }

    // Parse the WWW-Authenticate challenge.
    let www_auth = probe
        .headers()
        .get("WWW-Authenticate")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            anyhow!(
                "Registry {} returned 401 but no WWW-Authenticate header",
                registry
            )
        })?;

    let (realm, service) = parse_www_authenticate(www_auth)
        .ok_or_else(|| anyhow!("Could not parse WWW-Authenticate header: {}", www_auth))?;

    // The probe scope is generic; override it with the per-repository pull scope.
    let token_url = format!(
        "{}?service={}&scope=repository:{}:pull",
        realm, service, repository
    );
    log::debug!("Getting auth token from: {}", token_url);
    fetch_bearer_token(client, &token_url).await
}

/// Parse a `Bearer realm="...",service="..."[,scope="..."]` header value.
/// Returns `(realm, service)` on success.
fn parse_www_authenticate(header: &str) -> Option<(String, String)> {
    let params = header.strip_prefix("Bearer ")?;

    let mut realm: Option<String> = None;
    let mut service: Option<String> = None;

    // Split on commas that lie outside quoted strings.
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = params.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                parts.push(params[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(params[start..].trim());

    for part in parts {
        if let Some(val) = part.strip_prefix("realm=") {
            realm = Some(val.trim_matches('"').to_string());
        } else if let Some(val) = part.strip_prefix("service=") {
            service = Some(val.trim_matches('"').to_string());
        }
    }

    Some((realm?, service?))
}

/// Fetch a bearer token from a fully-constructed token URL.
async fn fetch_bearer_token(client: &Client, token_url: &str) -> anyhow::Result<String> {
    let response = client
        .get(token_url)
        .send()
        .await
        .context("Failed to request auth token")?;

    if !response.status().is_success() {
        // Registry may allow anonymous pulls; proceed with an empty token.
        return Ok(String::new());
    }

    let body = response.text().await?;
    let json: Value = serde_json::from_str(&body).context("Failed to parse auth token response")?;

    // OCI spec uses "token"; Docker also emits "access_token".
    json["token"]
        .as_str()
        .or_else(|| json["access_token"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("No token field in auth response: {}", body))
}

async fn download_blob_with_auth(
    client: &Client,
    url: &str,
    dest: &Path,
    token: &str,
    label: &str,
    expected_digest: &str,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let mut request = client.get(url);
    if !token.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let response = request.send().await.context("Failed to start download")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to download blob: HTTP {}",
            response.status()
        ));
    }

    let total_size = response.content_length().unwrap_or(0);

    progress.begin(label, Unit::Bytes, (total_size > 0).then_some(total_size));

    let mut file = tokio::fs::File::create(dest)
        .await
        .context("Failed to create destination file")?;

    let mut hasher = DownloadHasher::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read chunk")?;
        file.write_all(&chunk)
            .await
            .context("Failed to write chunk")?;
        hasher.update(&chunk);
        progress.advance(chunk.len() as u64);
    }

    file.flush().await?;
    progress.finish();

    // The blob was addressed by this digest in the manifest; a mismatch means
    // truncated or tampered content.  The blob lives in a temp directory that
    // is discarded on error, so a bad layer never reaches the cache.
    hasher.verify(Some(expected_digest)).with_context(|| {
        format!(
            "layer blob {} failed verification against manifest digest",
            url
        )
    })?;

    Ok(())
}

/// Map an OCI layer mediaType to the appropriate file extension.
/// The extension is stored in layers.manifest and used by extract_oci_layer
/// for compression dispatch, so it must accurately reflect the blob encoding.
fn media_type_to_extension(media_type: &str) -> &'static str {
    match media_type {
        // Docker V2 schema 2
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => "tar.gz",
        // OCI image spec
        "application/vnd.oci.image.layer.v1.tar+gzip" => "tar.gz",
        "application/vnd.oci.image.layer.v1.tar+zstd" => "tar.zst",
        "application/vnd.oci.image.layer.v1.tar" => "tar",
        // Non-distributable variants (same encoding, different semantics)
        "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => "tar.gz",
        "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => "tar.zst",
        "application/vnd.oci.image.layer.nondistributable.v1.tar" => "tar",
        // Unknown: fall back to gzip (historically the most common format)
        // and let magic-byte detection in extract_oci_layer handle it.
        _ => "tar.gz",
    }
}

#[cfg(test)]
mod tests {
    use super::expected_digest_from_checksums;
    use super::parse_www_authenticate;
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

    #[test]
    fn test_docker_hub_challenge() {
        let hdr = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/ubuntu:pull""#;
        let (realm, service) = parse_www_authenticate(hdr).unwrap();
        assert_eq!(realm, "https://auth.docker.io/token");
        assert_eq!(service, "registry.docker.io");
    }

    #[test]
    fn test_quay_challenge() {
        let hdr = r#"Bearer realm="https://quay.io/v2/auth",service="quay.io",scope="repository:centos/centos:pull""#;
        let (realm, service) = parse_www_authenticate(hdr).unwrap();
        assert_eq!(realm, "https://quay.io/v2/auth");
        assert_eq!(service, "quay.io");
    }

    #[test]
    fn test_ghcr_challenge() {
        let hdr = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:owner/repo:pull""#;
        let (realm, service) = parse_www_authenticate(hdr).unwrap();
        assert_eq!(realm, "https://ghcr.io/token");
        assert_eq!(service, "ghcr.io");
    }

    #[test]
    fn test_gcr_challenge() {
        let hdr = r#"Bearer realm="https://gcr.io/v2/token",service="gcr.io""#;
        let (realm, service) = parse_www_authenticate(hdr).unwrap();
        assert_eq!(realm, "https://gcr.io/v2/token");
        assert_eq!(service, "gcr.io");
    }

    #[test]
    fn test_value_with_comma_in_scope() {
        // scope field itself contains no comma but realm/service values may
        // contain other special chars — ensure the quoted-comma splitter works
        let hdr = r#"Bearer realm="https://example.com/auth",service="example.com",scope="repository:a/b:pull""#;
        let (realm, service) = parse_www_authenticate(hdr).unwrap();
        assert_eq!(realm, "https://example.com/auth");
        assert_eq!(service, "example.com");
    }

    #[test]
    fn test_not_bearer_returns_none() {
        assert!(parse_www_authenticate("Basic realm=\"registry\"").is_none());
    }

    #[test]
    fn test_media_type_to_extension_known_types() {
        use super::media_type_to_extension;
        assert_eq!(
            media_type_to_extension("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            "tar.gz"
        );
        assert_eq!(
            media_type_to_extension("application/vnd.oci.image.layer.v1.tar+gzip"),
            "tar.gz"
        );
        assert_eq!(
            media_type_to_extension("application/vnd.oci.image.layer.v1.tar+zstd"),
            "tar.zst"
        );
        assert_eq!(
            media_type_to_extension("application/vnd.oci.image.layer.v1.tar"),
            "tar"
        );
    }

    #[test]
    fn test_media_type_to_extension_unknown_falls_back_to_gz() {
        use super::media_type_to_extension;
        assert_eq!(media_type_to_extension(""), "tar.gz");
        assert_eq!(media_type_to_extension("text/plain"), "tar.gz");
    }
}
