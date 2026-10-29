//! The OCI registry side of image download: bearer-token auth per
//! the Distribution Spec, a manifest list reduced to one architecture,
//! and content-addressed layer blobs bundled into a cache entry.

use anyhow::{anyhow, Context};
use reqwest::blocking::Client;
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::download::http_client;
use crate::download::{digest_sidecar, DownloadHasher};
use crate::progress::{Progress, Unit};
use crate::{Error, Result};

/// Fetch the current manifest digest for an OCI tag without downloading any
/// layers.  The registry returns the content-addressable digest in the
/// `Docker-Content-Digest` response header of any manifest GET.
pub(crate) fn fetch_oci_digest(registry: &str, repository: &str, tag: &str) -> Result<String> {
    fetch_oci_digest_inner(registry, repository, tag).map_err(Error::Other)
}

fn fetch_oci_digest_inner(registry: &str, repository: &str, tag: &str) -> anyhow::Result<String> {
    // Metadata: a small body, so a total request bound is safe.
    let client = http_client(Some(Duration::from_secs(30)))?;
    let token = get_auth_token(&client, registry, repository)?;

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

pub(crate) fn download_oci_image(
    registry: &str,
    repository: &str,
    tag: &str,
    arch: &str,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<()> {
    let client = http_client(None)?;
    let digest = try_download_oci_image(&client, registry, repository, tag, arch, dest, progress)?;

    // Persist the manifest digest so the next run can skip the download when
    // the tag hasn't changed (see fetch_oci_digest / digest_sidecar).
    if !digest.is_empty() {
        let _ = std::fs::write(digest_sidecar(dest), &digest);
    }

    Ok(())
}

fn try_download_oci_image(
    client: &Client,
    registry: &str,
    repository: &str,
    tag: &str,
    arch: &str,
    dest: &Path,
    progress: &dyn Progress,
) -> anyhow::Result<String> {
    // Get authentication token for the registry
    let token = get_auth_token(client, registry, repository)?;

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

    let body = response.text()?;
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
            .context("Failed to get architecture manifest")?;

        if !arch_response.status().is_success() {
            return Err(anyhow!(
                "Failed to get arch manifest: HTTP {}",
                arch_response.status()
            ));
        }

        let arch_body = arch_response.text()?;

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
        )?;
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

fn get_auth_token(client: &Client, registry: &str, repository: &str) -> anyhow::Result<String> {
    // Docker Hub has a well-known, stable auth endpoint.
    if registry == "docker.io" {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            repository
        );
        log::debug!("Getting auth token from: {}", url);
        return fetch_bearer_token(client, &url);
    }

    // For every other registry follow the OCI Distribution Spec §4.2:
    //   1. Probe GET /v2/ unauthenticated.
    //   2. Read the WWW-Authenticate challenge from the 401 response.
    //   3. Build the token URL from the advertised realm + service.
    let probe_url = format!("https://{}/v2/", registry);
    let probe = client
        .get(&probe_url)
        .send()
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
    fetch_bearer_token(client, &token_url)
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
fn fetch_bearer_token(client: &Client, token_url: &str) -> anyhow::Result<String> {
    let response = client
        .get(token_url)
        .send()
        .context("Failed to request auth token")?;

    if !response.status().is_success() {
        // Registry may allow anonymous pulls; proceed with an empty token.
        return Ok(String::new());
    }

    let body = response.text()?;
    let json: Value = serde_json::from_str(&body).context("Failed to parse auth token response")?;

    // OCI spec uses "token"; Docker also emits "access_token".
    json["token"]
        .as_str()
        .or_else(|| json["access_token"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("No token field in auth response: {}", body))
}

fn download_blob_with_auth(
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

    let mut response = request.send().context("Failed to start download")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to download blob: HTTP {}",
            response.status()
        ));
    }

    let total_size = response.content_length().unwrap_or(0);

    progress.begin(label, Unit::Bytes, (total_size > 0).then_some(total_size));

    let mut file = std::fs::File::create(dest).context("Failed to create destination file")?;

    let mut hasher = DownloadHasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response.read(&mut buf).context("Failed to read chunk")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("Failed to write chunk")?;
        hasher.update(&buf[..n]);
        progress.advance(n as u64);
    }

    file.flush().context("Failed to flush the layer file")?;
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
    use super::{media_type_to_extension, parse_www_authenticate};

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
        assert_eq!(media_type_to_extension(""), "tar.gz");
        assert_eq!(media_type_to_extension("text/plain"), "tar.gz");
    }
}
