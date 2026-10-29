//! Provisioning specs: the checked-in description of what a layer
//! carries beyond its base image. A spec is data, not a command —
//! the artifact under review is a diff in git, and the layer it
//! produces is a pure function of the file's content and its base.

use serde::Deserialize;
use std::fmt::Write as _;

/// A parsed provisioning spec file (YAML: base, packages, run).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProvisionSpec {
    /// The image to provision over. Optional: without it the caller's
    /// positional image is the base.
    #[serde(default)]
    pub base: Option<String>,

    /// Packages installed through whatever package manager the base
    /// carries (apt, apk, dnf).
    #[serde(default)]
    pub packages: Vec<String>,

    /// A shell script run after the packages, for everything a
    /// package name cannot say.
    #[serde(default)]
    pub run: Option<String>,
}

impl ProvisionSpec {
    /// Parses the spec from raw file bytes. Empty and missing optional
    /// fields stay optional; an unparseable file is an error — a typo
    /// in the spec must not silently provision a bare base.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let spec: ProvisionSpec = serde_yaml::from_slice(bytes)
            .map_err(|err| anyhow::anyhow!("the provisioning spec does not parse: {err}"))?;
        if spec.base.is_none() && spec.packages.is_empty() && spec.run.is_none() {
            anyhow::bail!("the provisioning spec is empty");
        }
        Ok(spec)
    }

    /// The layer key: content the cache entries off. Hash of the raw
    /// file bytes and the base reference — the bytes, not the parsed
    /// form, so the key is stable across ecrou versions; reformatting
    /// the file rebuilds the layer, which is harmless and visible.
    pub fn key(spec_bytes: &[u8], base_ref: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(spec_bytes);
        hasher.update([0]);
        hasher.update(base_ref.as_bytes());
        let digest = hasher.finalize();
        digest[..8].iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    /// The provisioning script: the packages through the base's
    /// package manager, then the raw `run` block. Nothing here knows
    /// which distro it lands on ahead of time — the machine itself
    /// says, at provision time.
    pub fn script(&self) -> String {
        let mut script = String::from("set -e\n");
        if !self.packages.is_empty() {
            let packages = self.packages.join(" ");
            let _ = write!(
                script,
                r#"if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends {packages}
elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache {packages}
elif command -v dnf >/dev/null 2>&1; then
    dnf install -y {packages}
else
    echo "no known package manager in the base image" >&2
    exit 1
fi
"#
            );
        }
        if let Some(run) = &self.run {
            script.push_str(run.trim_end());
            script.push('\n');
        }
        script
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_parses_with_all_parts_optional_but_none_empty() {
        let spec = ProvisionSpec::parse(b"packages:\n  - jq\n").expect("parses");
        assert_eq!(spec.packages, vec!["jq".to_owned()]);
        assert!(spec.base.is_none());

        let full = ProvisionSpec::parse(
            b"base: ghcr.io/example/base:v1\npackages:\n  - go\nrun: |\n  cargo install just\n",
        )
        .expect("parses");
        assert_eq!(full.base.as_deref(), Some("ghcr.io/example/base:v1"));
        assert!(full.run.as_deref().unwrap().contains("cargo install"));

        assert!(ProvisionSpec::parse(b"\n").is_err(), "empty is an error");
        assert!(
            ProvisionSpec::parse(b"packages: [\n").is_err(),
            "unparseable yaml is an error"
        );
    }

    #[test]
    fn the_key_tracks_the_spec_bytes_and_the_base() {
        let a = ProvisionSpec::key(b"packages: [jq]\n", "alpine:3.23");
        assert_eq!(a.len(), 16);
        assert_eq!(a, ProvisionSpec::key(b"packages: [jq]\n", "alpine:3.23"));
        assert_ne!(a, ProvisionSpec::key(b"packages: [jq]\n", "alpine:3.24"));
        assert_ne!(a, ProvisionSpec::key(b"packages: [jq] \n", "alpine:3.23"));
    }

    #[test]
    fn the_script_speaks_the_base_package_manager_and_then_runs() {
        let spec = ProvisionSpec {
            base: None,
            packages: vec!["jq".into(), "go".into()],
            run: Some("go install m/tool@latest\n".into()),
        };
        let script = spec.script();
        assert!(script.starts_with("set -e"));
        assert!(script.contains("apt-get install -y -qq --no-install-recommends jq go"));
        assert!(script.contains("apk add --no-cache jq go"));
        assert!(script.ends_with("go install m/tool@latest\n"));

        // packages only: no pm block would try to install nothing
        let bare = ProvisionSpec {
            packages: vec![],
            run: Some("echo hi".into()),
            base: None,
        };
        let script = bare.script();
        assert!(!script.contains("apt-get"));
        assert!(script.contains("echo hi"));
    }
}
