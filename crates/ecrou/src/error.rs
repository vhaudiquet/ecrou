//! Typed error for the public library API.
//!
//! Public functions return [`Result`] carrying [`Error`] so consumers can
//! match on the failure modes that call for different handling, a host
//! without working KVM, an architecture the registry does not serve,
//! versus ordinary failures. Everything without a semantic variant
//! surfaces as [`Error::Other`], which preserves the full anyhow context
//! chain.

/// Errors returned by the ecrou library API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No working KVM on this host, and ecrou has no software-emulation
    /// fallback. The remediation names the usual causes in order.
    #[error(
        "KVM is not available\n\n\
         ecrou boots microVMs and needs /dev/kvm:\n\
         \x20 check the module:   lsmod | grep kvm\n\
         \x20 load it:            sudo modprobe kvm_intel  (or kvm_amd)\n\
         \x20 nested virt (in a VM): enable it on the hypervisor, then\n\
         \x20                     cat /sys/module/kvm_intel/parameters/nested\n\
         \x20 access:             sudo usermod -aG kvm $USER  (re-login)"
    )]
    KvmUnavailable,

    /// The registry publishes no linux manifest for the requested
    /// architecture; `available` lists the architectures it does serve.
    #[error("No manifest found for architecture '{arch}'. Available: {available}")]
    ArchNotAvailable { arch: String, available: String },

    /// Any other failure, preserving the full context chain.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Result alias for the public library API: `Result<T>` carries [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_unavailable_renders_remediation() {
        let msg = Error::KvmUnavailable.to_string();
        assert!(msg.contains("KVM is not available"));
        assert!(msg.contains("modprobe kvm_intel"));
        assert!(msg.contains("usermod -aG kvm"));
    }

    #[test]
    fn other_preserves_chain_through_sources() {
        let inner = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing file",
        ))
        .context("extract failed");
        let e = Error::Other(inner);
        // Walk the source chain: the io error must remain reachable for
        // error-report crates, even though Display shows only the top.
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
        let mut found = false;
        while let Some(err) = source {
            if err.to_string().contains("missing file") {
                found = true;
            }
            source = err.source();
        }
        assert!(found, "chain root lost through Error::Other");
    }
}
