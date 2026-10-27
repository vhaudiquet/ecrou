use anyhow::anyhow;

use crate::Result;
use std::path::Path;

// ============================================================================
// Constants
// ============================================================================

/// Length of the random hostname suffix (36⁶ ≈ 2×10⁹ distinct values)
pub const HOSTNAME_SUFFIX_LEN: usize = 6;

/// Random lowercase-alphanumeric suffix for hostnames.  Seeded from the
/// current time and the PID so calls in rapid succession — including from
/// separate processes — produce different values; time alone truncated to
/// the suffix space repeats within a tight loop.
pub fn random_suffix(len: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let mut state = hasher.finish();

    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    // Knuth multiplicative LCG — each step advances the full 64-bit state.
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            CHARS[(state >> 33) as usize % CHARS.len()] as char
        })
        .collect()
}

// ============================================================================
// Architecture handling
// ============================================================================

/// Architecture representation in different naming conventions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// x86-64 (AMD64, x86_64)
    Amd64,
    /// ARM 64-bit (AArch64)
    Arm64,
    /// ARM 32-bit hard-float
    Armhf,
    /// RISC-V 64-bit
    Riscv64,
    /// PowerPC 64-bit little-endian
    Ppc64el,
    /// IBM s390x
    S390x,
    /// Unknown architecture
    Unknown,
}

impl Arch {
    /// Get the architecture from a string (any common naming convention)
    /// Not a FromStr impl: unrecognized names map to Arch::Unknown rather
    /// than an error, by design.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "amd64" | "x86_64" | "x64" => Arch::Amd64,
            "arm64" | "aarch64" | "arm64v8" => Arch::Arm64,
            "armhf" | "armv7" | "armv7l" | "arm" => Arch::Armhf,
            "riscv64" => Arch::Riscv64,
            "ppc64el" | "ppc64le" => Arch::Ppc64el,
            "s390x" => Arch::S390x,
            _ => Arch::Unknown,
        }
    }

    /// Get the Debian/Ubuntu style name
    pub fn debian_name(&self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
            Arch::Armhf => "armhf",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc64el",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }

    /// Get the OCI/Docker registry style name
    pub fn oci_name(&self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
            // OCI uses "arm" for 32-bit ARM with variant field
            Arch::Armhf => "arm",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc64le",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }

    /// Get the Alpine style name
    pub fn alpine_name(&self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64",
            Arch::Arm64 => "aarch64",
            Arch::Armhf => "armv7",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc64le",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }

    /// Get the QEMU binary suffix (e.g., "qemu-system-x86_64")
    pub fn qemu_system_name(&self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64",
            Arch::Arm64 => "aarch64",
            Arch::Armhf => "arm",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc64",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }

    /// Get the QEMU binfmt_misc name
    pub fn qemu_binfmt_name(&self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64",
            Arch::Arm64 => "aarch64",
            Arch::Armhf => "arm",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc64le",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }

    /// Get the package suffix for QEMU system emulator
    pub fn qemu_package_suffix(&self) -> &'static str {
        match self {
            Arch::Amd64 => "x86",
            Arch::Arm64 => "aarch64",
            Arch::Armhf => "arm",
            Arch::Riscv64 => "riscv64",
            Arch::Ppc64el => "ppc",
            Arch::S390x => "s390x",
            Arch::Unknown => "unknown",
        }
    }
}

/// Get the host system architecture using uname(2) syscall
/// This returns the runtime machine string, which is correct even when
/// the binary itself is running under emulation.
pub fn get_host_arch() -> Arch {
    // Safety: utsname is a valid zeroed libc struct; uname(2) writes only
    // within it and NUL-terminates each field before we read machine.
    let mut utsname: libc::utsname = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::uname(&mut utsname) };
    if rc != 0 {
        panic!("uname(2) failed: {}", std::io::Error::last_os_error());
    }
    let machine = unsafe { std::ffi::CStr::from_ptr(utsname.machine.as_ptr()) };
    Arch::from_str(machine.to_string_lossy().as_ref())
}

/// Map ecrou architecture names to distro-specific names
pub fn map_arch_for_distro(distro: &str, arch: &str) -> String {
    let arch_enum = Arch::from_str(arch);
    match distro.to_lowercase().as_str() {
        "ubuntu" => arch_enum.debian_name().to_string(),
        "alpine" => arch_enum.alpine_name().to_string(),
        _ => arch_enum.oci_name().to_string(),
    }
}

/// Map architecture to OCI registry standard names
pub fn map_oci_arch(arch: &str) -> String {
    Arch::from_str(arch).oci_name().to_string()
}

// ============================================================================
// Shell detection
// ============================================================================

/// Detect the best available shell in a rootfs
/// Checks for bash first, falls back to sh
/// Returns the path relative to the rootfs (e.g., "/bin/bash")
pub fn detect_shell(rootfs: &Path) -> &'static str {
    // Check for bash first (preferred)
    if rootfs.join("bin/bash").exists() {
        "/bin/bash"
    } else if rootfs.join("bin/sh").exists() {
        "/bin/sh"
    } else if rootfs.join("usr/bin/bash").exists() {
        "/usr/bin/bash"
    } else if rootfs.join("usr/bin/sh").exists() {
        "/usr/bin/sh"
    } else {
        "/bin/sh" // Will fail with clear error if not present
    }
}

// ============================================================================
// Memory string validation
// ============================================================================

/// Validate a QEMU memory size string (e.g., "512M", "2G")
/// Returns an error if the format is invalid
pub fn validate_memory_string(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(anyhow!("Memory size cannot be empty").into());
    }

    // Must end with a valid suffix or be a plain number
    let suffix = s.chars().last().unwrap();
    let has_suffix = suffix.is_ascii_alphabetic();

    let numeric_part = if has_suffix { &s[..s.len() - 1] } else { s };

    // Check for negative numbers
    if numeric_part.starts_with('-') {
        return Err(anyhow!("Memory size cannot be negative: {}", s).into());
    }

    // Must be a valid positive number
    if numeric_part.is_empty() {
        return Err(anyhow!("Memory size must have a numeric value: {}", s).into());
    }

    // Check if it's a valid integer
    if !numeric_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "Memory size must be a positive integer with optional suffix: {}",
            s
        )
        .into());
    }

    // Check suffix is valid
    if has_suffix {
        let valid_suffixes = ['K', 'M', 'G', 'T'];
        let suffix_upper = suffix.to_ascii_uppercase();
        if !valid_suffixes.contains(&suffix_upper) {
            return Err(anyhow!(
                "Invalid memory suffix '{}'. Valid suffixes: K, M, G, T",
                suffix
            )
            .into());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arch_from_str() {
        assert_eq!(Arch::from_str("amd64"), Arch::Amd64);
        assert_eq!(Arch::from_str("x86_64"), Arch::Amd64);
        assert_eq!(Arch::from_str("arm64"), Arch::Arm64);
        assert_eq!(Arch::from_str("aarch64"), Arch::Arm64);
        assert_eq!(Arch::from_str("armhf"), Arch::Armhf);
        assert_eq!(Arch::from_str("armv7"), Arch::Armhf);
        assert_eq!(Arch::from_str("riscv64"), Arch::Riscv64);
        assert_eq!(Arch::from_str("ppc64el"), Arch::Ppc64el);
        assert_eq!(Arch::from_str("ppc64le"), Arch::Ppc64el);
        assert_eq!(Arch::from_str("s390x"), Arch::S390x);
    }

    #[test]
    fn test_arch_alpine_name() {
        assert_eq!(Arch::Amd64.alpine_name(), "x86_64");
        assert_eq!(Arch::Arm64.alpine_name(), "aarch64");
        assert_eq!(Arch::Armhf.alpine_name(), "armv7");
    }

    #[test]
    fn test_arch_oci_name() {
        assert_eq!(Arch::Amd64.oci_name(), "amd64");
        assert_eq!(Arch::Arm64.oci_name(), "arm64");
        assert_eq!(Arch::Armhf.oci_name(), "arm");
    }

    #[test]
    fn test_validate_memory_string_valid() {
        assert!(validate_memory_string("512M").is_ok());
        assert!(validate_memory_string("2G").is_ok());
        assert!(validate_memory_string("1024").is_ok());
        assert!(validate_memory_string("1T").is_ok());
        assert!(validate_memory_string("256K").is_ok());
        assert!(validate_memory_string("2g").is_ok()); // lowercase
    }

    #[test]
    fn test_validate_memory_string_invalid() {
        assert!(validate_memory_string("").is_err());
        assert!(validate_memory_string("-1G").is_err());
        assert!(validate_memory_string("2X").is_err());
        assert!(validate_memory_string("abc").is_err());
        assert!(validate_memory_string("G").is_err());
        assert!(validate_memory_string("1.5G").is_err());
    }
}
