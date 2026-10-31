use std::path::{Path, PathBuf};

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
// Unique scratch paths for atomic writes
// ============================================================================

/// Sequence counter for [`partial_path`], unique within the process.
static PARTIAL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A sibling scratch path for writing `dest` before an atomic rename:
/// the pid separates concurrent processes, the sequence separates
/// threads. Two writers racing on the same cache entry each complete
/// their own file, so whichever rename lands last, the bytes at `dest`
/// are whole.
pub fn partial_path(dest: &Path) -> PathBuf {
    let seq = PARTIAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dest.with_extension(format!("partial.{}-{seq}", std::process::id()))
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
// External tool lookup
// ============================================================================

/// The Homebrew prefixes the lookup falls back to. Empty off macOS:
/// Homebrew does not exist there and PATH is the whole story.
#[cfg(target_os = "macos")]
pub const BREW_PREFIXES: &[&str] = &["/opt/homebrew", "/usr/local"];
#[cfg(not(target_os = "macos"))]
pub const BREW_PREFIXES: &[&str] = &[];

/// The keg-only formulae the run's external tools come from, in the
/// order they should be preferred. e2fsprogs is keg-only in Homebrew
/// (its sbin never reaches PATH), and the tap ships its own variant
/// built with libarchive, which outranks the core bottle.
#[cfg(target_os = "macos")]
const BREW_TOOL_FORMULAE: &[&str] = &["e2fsprogs-libarchive", "e2fsprogs"];
#[cfg(not(target_os = "macos"))]
const BREW_TOOL_FORMULAE: &[&str] = &[];

/// Locate an external tool the run shells out to: PATH first, then
/// the Homebrew keg directories. Returns the path, so callers spawn a
/// deterministic binary instead of a whatever-came-first `Command`.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for prefix in BREW_PREFIXES {
        for formula in BREW_TOOL_FORMULAE {
            for sub in ["sbin", "bin"] {
                let candidate = Path::new(prefix)
                    .join("opt")
                    .join(formula)
                    .join(sub)
                    .join(name);
                if is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The brew kegs whose dylibs are loaded by bare soname at runtime:
/// libkrunfw (loaded by libkrun) and libarchive (loaded by a
/// libarchive-capable mke2fs). dyld's fallback search never reaches
/// the brew prefix, so these directories have to land in
/// DYLD_FALLBACK_LIBRARY_PATH before any of those loads happen.
#[cfg(target_os = "macos")]
const DYLD_KEGS: &[&str] = &["libkrunfw", "libarchive"];
#[cfg(not(target_os = "macos"))]
const DYLD_KEGS: &[&str] = &[];

/// Directories the libarchive dylib can be dlopen'd from, joined for
/// DYLD_FALLBACK_LIBRARY_PATH. Superseded by dyld_fallback_dirs; kept
/// only until the image builder stops calling it.
#[cfg(target_os = "macos")]
pub fn libarchive_fallback_dirs() -> Option<String> {
    let mut dirs = Vec::new();
    for prefix in BREW_PREFIXES {
        let dir = Path::new(prefix).join("opt/libarchive/lib");
        if dir.is_dir() {
            dirs.push(dir.as_os_str().to_os_string());
        }
    }
    if dirs.is_empty() {
        None
    } else {
        Some(
            std::env::join_paths(dirs)
                .ok()?
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(not(target_os = "macos"))]
pub fn libarchive_fallback_dirs() -> Option<String> {
    None
}

/// Directories the bare-soname dlopens can be satisfied from, joined
/// for DYLD_FALLBACK_LIBRARY_PATH, or None when none of them exists.
pub fn dyld_fallback_dirs() -> Option<String> {
    let mut dirs = Vec::new();
    for prefix in BREW_PREFIXES {
        for keg in DYLD_KEGS {
            let dir = Path::new(prefix).join("opt").join(keg).join("lib");
            if dir.is_dir() {
                dirs.push(dir.into_os_string());
            }
        }
    }
    if dirs.is_empty() {
        None
    } else {
        Some(
            std::env::join_paths(dirs)
                .ok()?
                .to_string_lossy()
                .into_owned(),
        )
    }
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
}
