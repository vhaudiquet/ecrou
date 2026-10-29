use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context};

use crate::Result;

/// Known distributions with optimized direct tarball downloads
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Distro {
    Ubuntu,
    Alpine,
}

/// Represents the source of the container image
#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    /// Direct tarball download from known distro
    DirectTarball {
        distro: Distro,
        version: Option<String>,
    },
    /// OCI/Docker registry image
    OciImage {
        registry: String,
        repository: String,
        tag: String,
        /// A digest pin (`sha256:…`), when the reference carries one
        /// (`repo:tag@sha256:…` or `repo@sha256:…`): the manifest is
        /// fetched by digest and the image is immutable by definition.
        digest: Option<String>,
        architecture: String,
    },
    /// A rootfs archive on the host filesystem, named with a path
    /// ("./rootfs.tar.gz", "/tmp/rootfs.tar"): read directly at prepare
    /// time, never downloaded and never cached as a pristine entry.
    LocalTarball { path: PathBuf },
}

impl Distro {
    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            "ubuntu" => Ok(Distro::Ubuntu),
            "alpine" => Ok(Distro::Alpine),
            _ => Err(anyhow!("Unknown distribution: {}", name).into()),
        }
    }
}

/// Parse image reference and return the appropriate source
/// Supports:
/// - Simple distro names: ubuntu, debian, alpine
/// - Distro with version: ubuntu:noble, alpine:3.19
/// - OCI image references: docker://ubuntu:latest, quay.io/centos/centos:stream9
/// - Docker Hub shorthand: ubuntu:latest (without registry prefix)
/// - Digest pins, as docker accepts them: ubuntu@sha256:…, repo:tag@sha256:…
pub fn parse_image_ref(input: &str, arch: &str) -> Result<ImageSource> {
    let input = input.trim();

    // Check for explicit docker:// or oci:// prefix
    if let Some(rest) = input
        .strip_prefix("docker://")
        .or_else(|| input.strip_prefix("oci://"))
    {
        return Ok(parse_oci_ref(rest, arch)?);
    }

    // A reference that names a path stays a path: "./x", "../x" and
    // "/x" can never be a distro name or an OCI reference, and the
    // shell hands unexpanded "~" over only when quoted, which is
    // deliberate. Everything else with a '/' is a registry ref.
    if input.starts_with("./") || input.starts_with("../") || input.starts_with('/') {
        return Ok(ImageSource::LocalTarball {
            path: PathBuf::from(input),
        });
    }

    // Check for registry prefix (contains /)
    if input.contains('/') {
        return Ok(parse_oci_ref(input, arch)?);
    }

    // Try to parse as known distro
    let (name, version) = match input.split_once(':') {
        Some((n, v)) => (n, Some(v.to_string())),
        None => (input, None),
    };

    // Check if it's a known distro with optimized path
    match name.to_lowercase().as_str() {
        "ubuntu" | "alpine" => {
            let distro = Distro::from_name(name)?;
            Ok(ImageSource::DirectTarball { distro, version })
        }
        // Arch: use Docker image (has mirrors configured, unlike bootstrap tarball)
        "arch" => {
            let oci_arch = map_oci_arch(arch);
            Ok(ImageSource::OciImage {
                registry: "docker.io".to_string(),
                repository: "library/archlinux".to_string(),
                tag: version.unwrap_or_else(|| "latest".to_string()),
                digest: None,
                architecture: oci_arch,
            })
        }
        // Special case: gentoo maps to gentoo/stage3 on Docker Hub (full rootfs)
        "gentoo" => {
            let oci_arch = map_oci_arch(arch);
            Ok(ImageSource::OciImage {
                registry: "docker.io".to_string(),
                repository: "gentoo/stage3".to_string(),
                tag: version.unwrap_or_else(|| "latest".to_string()),
                digest: None,
                architecture: oci_arch,
            })
        }
        // Default to Docker Hub for unknown distros (debian, fedora, etc.)
        _ => Ok(parse_oci_ref(input, arch)?),
    }
}

/// Parse OCI image reference (registry/repo:tag[@digest] or repo:tag[@digest])
fn parse_oci_ref(input: &str, arch: &str) -> anyhow::Result<ImageSource> {
    // Split off a digest pin first: '@' after the last '/' introduces
    // `algo:hex`, which itself contains a colon, so it must be peeled
    // before tag splitting.  Docker accepts `repo:tag@digest` (digest
    // wins) and bare `repo@digest`; so do we.
    let (without_digest, digest) = match digest_separator(input) {
        Some(at) => (
            &input[..at],
            Some(validate_digest(&input[at + 1..]).map_err(anyhow::Error::msg)?),
        ),
        None => (input, None),
    };
    // Split off the tag.  A ':' is a tag separator only when it appears after
    // the last '/'; any ':' before a '/' is a port number
    // (e.g. localhost:5000/image).  With no '/' the single ':' is the tag.
    let (without_tag, tag) = if let Some(slash_pos) = without_digest.rfind('/') {
        let after_slash = &without_digest[slash_pos + 1..];
        if let Some(colon_pos) = after_slash.find(':') {
            (
                &without_digest[..slash_pos + 1 + colon_pos],
                without_digest[slash_pos + 1 + colon_pos + 1..].to_string(),
            )
        } else {
            (without_digest, "latest".to_string())
        }
    } else {
        // No slash: bare "ubuntu" or "ubuntu:latest"
        match without_digest.split_once(':') {
            Some((name, t)) => (name, t.to_string()),
            None => (without_digest, "latest".to_string()),
        }
    };

    // Split without_tag into registry + repository.
    // The first path component is the registry when it contains '.' or ':'
    // (hostname / host:port) or is the literal "localhost".
    let (registry, repository) = if let Some((first, rest)) = without_tag.split_once('/') {
        if first.contains('.') || first.contains(':') || first == "localhost" {
            (first.to_string(), rest.to_string())
        } else {
            // Org-qualified Docker Hub shorthand: "myorg/myimage"
            ("docker.io".to_string(), without_tag.to_string())
        }
    } else {
        // Bare image name → Docker Hub library image
        ("docker.io".to_string(), format!("library/{}", without_tag))
    };

    // Map architecture to OCI standard
    let oci_arch = map_oci_arch(arch);

    Ok(ImageSource::OciImage {
        registry,
        repository,
        tag,
        digest,
        architecture: oci_arch,
    })
}

/// Byte offset of the digest separator ('@' after the last '/'), if the
/// reference carries a digest pin. '@' is not legal in registries,
/// repositories, or tags, so the first one after the last '/' is
/// unambiguous.
fn digest_separator(input: &str) -> Option<usize> {
    let after_slash = match input.rfind('/') {
        Some(slash) => &input[slash + 1..],
        None => input,
    };
    after_slash
        .find('@')
        .map(|at| input.len() - after_slash.len() + at)
}

/// Validate a digest pin: `algo:hex`, hex at least 32 digits — sha256
/// and sha512 are the digests registries actually issue. Returns the
/// lowercased digest.
fn validate_digest(digest: &str) -> anyhow::Result<String> {
    let (algo, hex) = digest
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid image digest {digest:?}: expected algo:hex"))?;
    let ok = !algo.is_empty()
        && algo
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && hex.len() >= 32
        && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !ok {
        return Err(anyhow::anyhow!(
            "invalid image digest {digest:?}: expected e.g. sha256:<64 hex digits>"
        ));
    }
    Ok(format!("{}:{}", algo, hex.to_lowercase()))
}

/// Map architecture to OCI standard names
pub fn map_oci_arch(arch: &str) -> String {
    crate::utils::map_oci_arch(arch)
}

/// Map ecrou architecture names to distro-specific names
pub fn map_arch(distro: Distro, arch: &str) -> String {
    let distro_name = match distro {
        Distro::Ubuntu => "ubuntu",
        Distro::Alpine => "alpine",
    };
    crate::utils::map_arch_for_distro(distro_name, arch)
}

/// Resolve the download URL for a known distro (optimized path)
pub fn resolve_distro_url(distro: &Distro, version: Option<&str>, arch: &str) -> Result<String> {
    Ok(match distro {
        Distro::Ubuntu => resolve_ubuntu_url(version, arch),
        Distro::Alpine => resolve_alpine_url(version, arch),
    }?)
}

/// URL of the checksum artifact covering the tarball at `tarball_url`, when
/// the distro publishes one.  Alpine ships a per-file `.sha256`; Ubuntu
/// publishes a `SHA256SUMS` manifest per release directory.
pub fn checksum_artifact_url(distro: &Distro, tarball_url: &str) -> Option<String> {
    match distro {
        Distro::Alpine => Some(format!("{}.sha256", tarball_url)),
        Distro::Ubuntu => {
            let dir = tarball_url.rsplit_once('/')?.0;
            Some(format!("{}/SHA256SUMS", dir))
        }
    }
}

/// Ubuntu releases with base images: `(codename, YY.MM)`, oldest
/// first. Static on purpose — `ubuntu:26.04` and `ubuntu:resolute`
/// must resolve identically and offline, and a codename never changes
/// meaning. A newer release than this table needs an entry only for
/// its codename; its number always resolves without one.
const UBUNTU_CODENAMES: &[(&str, &str)] = &[
    ("xenial", "16.04"),
    ("bionic", "18.04"),
    ("cosmic", "18.10"),
    ("disco", "19.04"),
    ("eoan", "19.10"),
    ("focal", "20.04"),
    ("groovy", "20.10"),
    ("hirsute", "21.04"),
    ("impish", "21.10"),
    ("jammy", "22.04"),
    ("kinetic", "22.10"),
    ("lunar", "23.04"),
    ("mantic", "23.10"),
    ("noble", "24.04"),
    ("oracular", "24.10"),
    ("plucky", "25.04"),
    ("questing", "25.10"),
    ("resolute", "26.04"),
];

/// One entry of a meta-release file.
#[derive(Debug, Clone, PartialEq)]
struct UbuntuRelease {
    codename: String,
    number: String,
    supported: bool,
}

/// Parse a changelogs.ubuntu.com meta-release body into releases, in
/// file order (oldest first).
fn parse_ubuntu_meta_release(text: &str) -> Vec<UbuntuRelease> {
    let mut releases = Vec::new();
    let mut current: Option<(String, String, bool)> = None;

    fn flush(current: &mut Option<(String, String, bool)>, releases: &mut Vec<UbuntuRelease>) {
        if let Some((codename, number, supported)) = current.take() {
            if !number.is_empty() {
                releases.push(UbuntuRelease {
                    codename,
                    number,
                    supported,
                });
            }
        }
    }

    for line in text.lines() {
        if let Some(dist) = line.strip_prefix("Dist: ") {
            flush(&mut current, &mut releases);
            current = Some((dist.trim().to_string(), String::new(), false));
        } else if let Some(version) = line.strip_prefix("Version: ") {
            // "22.04", "22.04 LTS", "24.04.1 LTS" — normalise to YY.MM,
            // the granularity ubuntu-base publishes.
            let raw = version.split_whitespace().next().unwrap_or("");
            if let Some((_, number, _)) = current.as_mut() {
                *number = normalise_ubuntu_number(raw);
            }
        } else if line.trim_start().starts_with("Supported: 1") {
            if let Some((_, _, supported)) = current.as_mut() {
                *supported = true;
            }
        }
    }
    flush(&mut current, &mut releases);
    releases
}

/// Fetch a meta-release file and return its releases.
fn fetch_ubuntu_meta_releases(url: &str) -> anyhow::Result<Vec<UbuntuRelease>> {
    let client = crate::download::http_client(Some(Duration::from_secs(30)))?;
    let text = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to fetch {}", url))?
        .text()
        .with_context(|| format!("Failed to read {}", url))?;
    Ok(parse_ubuntu_meta_release(&text))
}

/// The YY.MM number of the newest supported release in a meta-release file.
/// Pass the LTS-only URL for the latest LTS, the full URL for the latest
/// release.
fn fetch_ubuntu_latest_number(url: &str) -> anyhow::Result<String> {
    fetch_ubuntu_meta_releases(url)?
        .into_iter()
        .filter(|r| r.supported)
        .next_back()
        .map(|r| r.number)
        .ok_or_else(|| anyhow!("no supported Ubuntu release listed in {}", url))
}

/// "24.04.1" names the same base release as "24.04": ubuntu-base
/// publishes one tarball per YY.MM series.
fn normalise_ubuntu_number(raw: &str) -> String {
    let mut parts = raw.splitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        _ => raw.to_string(),
    }
}

/// A codename becomes its YY.MM number: from the static table, or —
/// for a release younger than this build's table — from meta-release.
fn ubuntu_codename_to_number(codename: &str) -> anyhow::Result<String> {
    let lower = codename.to_lowercase();
    if let Some((_, number)) = UBUNTU_CODENAMES.iter().find(|(name, _)| *name == lower) {
        return Ok((*number).to_string());
    }
    let releases = fetch_ubuntu_meta_releases("https://changelogs.ubuntu.com/meta-release")?;
    releases
        .iter()
        .find(|r| r.codename == lower)
        .map(|r| r.number.clone())
        .ok_or_else(|| {
            anyhow!(
                "Unknown Ubuntu version {codename:?}: use the YY.MM number (e.g. 26.04) \
                 or a known codename (e.g. resolute, noble)"
            )
        })
}

/// Fetch the current Alpine minirootfs version from latest-releases.yaml on the CDN.
/// The `latest-stable/` directory is a server-side symlink; the YAML file it contains
/// tells us the exact version number needed for the tarball filename.
/// Fetch the latest minirootfs version for a given Alpine CDN branch.
/// `branch` is the directory name on the CDN, e.g. `"latest-stable"` or `"v3.23"`.
fn fetch_alpine_version_from_branch(branch: &str, arch: &str) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct AlpineRelease {
        file: Option<String>,
        version: Option<String>,
    }

    let url = format!(
        "https://dl-cdn.alpinelinux.org/alpine/{}/releases/{}/latest-releases.yaml",
        branch, arch
    );
    let client = crate::download::http_client(Some(Duration::from_secs(30)))?;
    let text = client
        .get(&url)
        .send()
        .with_context(|| {
            format!(
                "Failed to fetch Alpine latest-releases.yaml for branch {}",
                branch
            )
        })?
        .text()
        .context("Failed to read Alpine latest-releases.yaml")?;

    let releases: Vec<AlpineRelease> =
        serde_yaml::from_str(&text).context("Failed to parse Alpine latest-releases.yaml")?;

    for release in releases {
        let is_minirootfs = release
            .file
            .as_deref()
            .map(|f| f.contains("minirootfs"))
            .unwrap_or(false);
        if is_minirootfs {
            // Prefer explicit `version:` field; fall back to parsing the filename.
            // Filename format: alpine-minirootfs-3.23.0-x86_64.tar.gz
            if let Some(v) = release.version {
                return Ok(v);
            }
            if let Some(v) = release.file.as_deref().and_then(|f| f.split('-').nth(2)) {
                return Ok(v.to_string());
            }
        }
    }

    Err(anyhow!(
        "Could not find minirootfs entry in Alpine latest-releases.yaml for branch {}",
        branch
    ))
}

fn fetch_alpine_latest_version(arch: &str) -> anyhow::Result<String> {
    fetch_alpine_version_from_branch("latest-stable", arch)
}

/// Resolve a `major.minor` Alpine series (e.g. `"3.24"`) to its current
/// patch release by querying the CDN branch `v{minor}`.
fn fetch_alpine_minor_version(minor: &str, arch: &str) -> anyhow::Result<String> {
    fetch_alpine_version_from_branch(&format!("v{}", minor), arch)
}

/// Resolve the canonical version string for a distro, performing a network lookup
/// only when the requested version is a floating alias (e.g. "latest", "lts").
/// The returned string is suitable for use as a stable cache key.
pub fn resolve_distro_version(
    distro: &Distro,
    version: Option<&str>,
    arch: &str,
) -> Result<String> {
    Ok(match distro {
        Distro::Ubuntu => resolve_ubuntu_version(version),
        Distro::Alpine => resolve_alpine_version(version, arch),
    }?)
}

fn resolve_ubuntu_version(version: Option<&str>) -> anyhow::Result<String> {
    let version = version.unwrap_or("latest");
    match version {
        "latest" => fetch_ubuntu_latest_number("https://changelogs.ubuntu.com/meta-release"),
        "lts" | "latest-lts" => {
            fetch_ubuntu_latest_number("https://changelogs.ubuntu.com/meta-release-lts")
        }
        other => {
            if is_ubuntu_version_number(other) {
                Ok(normalise_ubuntu_number(other))
            } else {
                ubuntu_codename_to_number(other)
            }
        }
    }
}

/// Returns true for strings of the form "YY.MM" or "YY.MM.Z" — every
/// dot-separated component numeric. Point releases normalise to YY.MM.
fn is_ubuntu_version_number(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn resolve_alpine_version(version: Option<&str>, arch: &str) -> anyhow::Result<String> {
    let alpine_arch = map_arch(Distro::Alpine, arch);
    Ok(match version.unwrap_or("latest") {
        "latest" | "stable" => fetch_alpine_latest_version(&alpine_arch)?,
        // edge is a rolling branch; query the CDN to get the current
        // date-stamped version (e.g. "20250401") so the URL and cache key
        // are correct.  resolve_alpine_url maps this date string back to the
        // "edge" CDN directory via the all-digits guard in its release match.
        "edge" => fetch_alpine_version_from_branch("edge", &alpine_arch)?,
        v => {
            // "3.23"   — one dot: major.minor series → fetch current patch from CDN
            // "3.23.0" — two dots: full version already → pass through as-is
            let dots = v.chars().filter(|&c| c == '.').count();
            if dots == 1 {
                fetch_alpine_minor_version(v, &alpine_arch)?
            } else {
                v.to_string()
            }
        }
    })
}

fn resolve_ubuntu_url(version: Option<&str>, arch: &str) -> anyhow::Result<String> {
    let number = resolve_ubuntu_version(version)?;
    let arch = map_arch(Distro::Ubuntu, arch);
    Ok(format!(
        "https://cdimage.ubuntu.com/ubuntu-base/releases/{number}/release/ubuntu-base-{number}-base-{arch}.tar.gz"
    ))
}

fn resolve_alpine_url(version: Option<&str>, arch: &str) -> anyhow::Result<String> {
    let version_str = version.unwrap_or("latest");
    let alpine_arch = map_arch(Distro::Alpine, arch);
    let version_num = resolve_alpine_version(Some(version_str), arch)?;

    // Derive the CDN release directory from the version string.
    // After resolve_distro_version runs, version_str may already be the
    // *resolved* value rather than the original alias:
    //   "latest"/"stable" → e.g. "3.23.1"  (dots present → v3.23 below)
    //   "edge"            → e.g. "20250401" (all digits, no dots)
    //   "3.23"            → e.g. "3.23.1"  (dots present → v3.23 below)
    // The all-digit check catches resolved edge dates and maps them back to
    // the "edge" CDN directory.
    let release = match version_str {
        "latest" | "stable" => "latest-stable".to_string(),
        "edge" => "edge".to_string(),
        v if v.chars().all(|c| c.is_ascii_digit()) => "edge".to_string(),
        other => {
            // "3.23" or "3.23.1" → "v3.23"
            let mut parts = other.splitn(3, '.');
            let major = parts.next().unwrap_or("0");
            let minor = parts.next().unwrap_or("0");
            format!("v{}.{}", major, minor)
        }
    };

    Ok(format!(
        "https://dl-cdn.alpinelinux.org/alpine/{}/releases/{}/alpine-minirootfs-{}-{}.tar.gz",
        release, alpine_arch, version_num, alpine_arch
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_artifact_url_per_distro() {
        assert_eq!(
            checksum_artifact_url(
                &Distro::Alpine,
                "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/alpine-minirootfs-3.23.0-x86_64.tar.gz"
            )
            .as_deref(),
            Some(
                "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/alpine-minirootfs-3.23.0-x86_64.tar.gz.sha256"
            )
        );
        assert_eq!(
            checksum_artifact_url(
                &Distro::Ubuntu,
                "https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/ubuntu-base-26.04-base-amd64.tar.gz"
            )
            .as_deref(),
            Some(
                "https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/SHA256SUMS"
            )
        );
    }

    fn oci(registry: &str, repository: &str, tag: &str) -> ImageSource {
        ImageSource::OciImage {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag: tag.to_string(),
            digest: None,
            architecture: "amd64".to_string(),
        }
    }

    fn parse(input: &str) -> ImageSource {
        parse_oci_ref(input, "amd64").expect("parse failed")
    }

    #[test]
    fn test_bare_name() {
        // "ubuntu" → Docker Hub library, tag=latest
        let src = parse("ubuntu");
        assert_eq!(src, oci("docker.io", "library/ubuntu", "latest"));
    }

    #[test]
    fn test_name_with_tag() {
        let src = parse("ubuntu:noble");
        assert_eq!(src, oci("docker.io", "library/ubuntu", "noble"));
    }

    #[test]
    fn test_org_repo() {
        let src = parse("myorg/myimage:v2");
        assert_eq!(src, oci("docker.io", "myorg/myimage", "v2"));
    }

    #[test]
    fn test_registry_with_port_no_tag() {
        // The colon in "localhost:5000" must NOT be treated as a tag separator
        let src = parse("localhost:5000/myimage");
        assert_eq!(src, oci("localhost:5000", "myimage", "latest"));
    }

    #[test]
    fn test_registry_with_port_and_tag() {
        let src = parse("localhost:5000/myimage:v1");
        assert_eq!(src, oci("localhost:5000", "myimage", "v1"));
    }

    #[test]
    fn test_registry_with_port_and_org() {
        let src = parse("localhost:5000/org/myimage:v1");
        assert_eq!(src, oci("localhost:5000", "org/myimage", "v1"));
    }

    #[test]
    fn test_named_registry() {
        let src = parse("quay.io/centos/centos:stream9");
        assert_eq!(src, oci("quay.io", "centos/centos", "stream9"));
    }

    #[test]
    fn test_docker_hub_fqdn() {
        let src = parse("registry-1.docker.io/library/ubuntu:noble");
        assert_eq!(src, oci("registry-1.docker.io", "library/ubuntu", "noble"));
    }

    /// After resolve_distro_version, alpine:edge carries a date string like
    /// "20250401".  resolve_alpine_url must still produce a URL under the
    /// "edge" CDN directory, not a bogus "v20250401.0" directory.
    #[test]
    fn test_alpine_edge_resolved_date_uses_edge_directory() {
        // Simulate the already-resolved version that main.rs stores after calling
        // resolve_distro_version("edge", …).
        let url = resolve_alpine_url(Some("20250401"), "amd64").unwrap();
        assert!(
            url.contains("/alpine/edge/"),
            "expected URL to contain '/alpine/edge/' but got: {}",
            url
        );
        assert!(
            url.contains("minirootfs-20250401-"),
            "expected URL to contain 'minirootfs-20250401-' but got: {}",
            url
        );
    }

    #[test]
    fn a_dotted_relative_reference_names_a_local_tarball() {
        let local = |p: &str| ImageSource::LocalTarball {
            path: std::path::PathBuf::from(p),
        };
        let src = parse_image_ref("./rootfs.tar.gz", "amd64").expect("parses");
        assert_eq!(src, local("./rootfs.tar.gz"));
        assert_eq!(
            parse_image_ref("../images/rootfs.tar", "amd64").expect("parses"),
            local("../images/rootfs.tar")
        );
        assert_eq!(
            parse_image_ref("/tmp/rootfs.tar.zst", "amd64").expect("parses"),
            local("/tmp/rootfs.tar.zst")
        );
    }

    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn digest_pins_parse_like_docker_references() {
        // Bare digest ref
        let src = parse(&format!("ubuntu@sha256:{SHA256_ABC}"));
        let ImageSource::OciImage {
            registry,
            repository,
            tag,
            digest,
            ..
        } = src
        else {
            panic!("expected OCI source");
        };
        assert_eq!(registry, "docker.io");
        assert_eq!(repository, "library/ubuntu");
        assert_eq!(tag, "latest");
        assert_eq!(
            digest.as_deref(),
            Some(format!("sha256:{SHA256_ABC}").as_str())
        );

        // Tag + digest: the digest wins, the tag rides along
        let src = parse(&format!("ubuntu:24.04@sha256:{SHA256_ABC}"));
        let ImageSource::OciImage { tag, digest, .. } = src else {
            panic!("expected OCI source");
        };
        assert_eq!(tag, "24.04");
        assert!(digest.is_some());

        // Registry-qualified digest ref, port included
        let src = parse(&format!("localhost:5000/org/img@sha256:{SHA256_ABC}"));
        let ImageSource::OciImage {
            registry,
            repository,
            digest,
            ..
        } = src
        else {
            panic!("expected OCI source");
        };
        assert_eq!(registry, "localhost:5000");
        assert_eq!(repository, "org/img");
        assert!(digest.is_some());
    }

    #[test]
    fn malformed_digests_are_named_errors() {
        for bad in [
            "ubuntu@sha256:tooshort",
            "ubuntu@sha256",
            "ubuntu@sha256:zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(
                parse_image_ref(&format!("docker://{bad}"), "amd64").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn path_like_registry_references_are_never_local_files() {
        // A leading path component that is a hostname stays OCI: only
        // "./", "../" and "/" introduce a local archive.
        assert_eq!(
            parse("mydir/image"),
            oci("docker.io", "mydir/image", "latest")
        );
    }
    #[test]
    fn meta_release_parsing_extracts_codename_number_pairs() {
        let text = "Dist: noble\nVersion: 24.04 LTS\nSupported: 1\n\
                    Dist: questing\nVersion: 25.10\nSupported: 1\n\
                    Dist: plucky\nVersion: 25.04\nSupported: 0\n";
        let releases = parse_ubuntu_meta_release(text);
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].codename, "noble");
        assert_eq!(releases[0].number, "24.04");
        assert!(releases[0].supported);
        // A point release normalises to YY.MM
        let text = "Dist: noble\nVersion: 24.04.2 LTS\nSupported: 1\n";
        assert_eq!(parse_ubuntu_meta_release(text)[0].number, "24.04");
    }

    #[test]
    fn ubuntu_numbers_and_codenames_resolve_offline_and_identically() {
        // The alias promise: number and codename are the same image —
        // same resolved version, same URL, same cache key. No network.
        assert_eq!(
            resolve_ubuntu_version(Some("26.04")).unwrap(),
            resolve_ubuntu_version(Some("resolute")).unwrap()
        );
        assert_eq!(resolve_ubuntu_version(Some("resolute")).unwrap(), "26.04");
        assert_eq!(resolve_ubuntu_version(Some("NOBLE")).unwrap(), "24.04");
        // Point releases normalise to the series.
        assert_eq!(resolve_ubuntu_version(Some("24.04.2")).unwrap(), "24.04");
        let url = resolve_ubuntu_url(Some("resolute"), "amd64").unwrap();
        assert_eq!(
            url,
            "https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/ubuntu-base-26.04-base-amd64.tar.gz"
        );
        assert_eq!(
            resolve_ubuntu_url(Some("26.04"), "amd64").unwrap(),
            resolve_ubuntu_url(Some("resolute"), "amd64").unwrap()
        );
    }
}
