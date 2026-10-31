//! Runtime bindings to libkrun's stable C ABI.
//!
//! The library (`libkrun.so.1` on Linux, `libkrun.1.dylib` on macOS)
//! loads through dlopen so a missing install is a remediation message
//! rather than a link error. The
//! symbol signatures mirror the `stable-1.19.x` headers; the main
//! branch is mid-rewrite to a vtable API and is not the target.
//!
//! The founding rule holds for this module too: libkrun's VMM runs in
//! our process and touches only files and sockets (plus `/dev/kvm` on
//! Linux), all inside the confinement budget (SPEC.md).

use std::ffi::{c_char, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::Error;

/// The ABI version libkrun reports for a working `/dev/kvm`
/// (`KVM_GET_API_VERSION`, the first request of the KVMIO range).
#[cfg(target_os = "linux")]
const KVM_GET_API_VERSION: libc::c_ulong = 0xAE00;
#[cfg(target_os = "linux")]
const KVM_API_VERSION_EXPECTED: libc::c_int = 12;

#[cfg(target_os = "macos")]
pub const SONAME: &str = "libkrun.1.dylib";
#[cfg(not(target_os = "macos"))]
pub const SONAME: &str = "libkrun.so.1";

/// Remediation shown when libkrun cannot be loaded: on macOS the
/// official Homebrew tap bottles the library; Linux distro packaging
/// is still sparse (Fedora carries it; Debian and Ubuntu do not yet).
#[cfg(target_os = "macos")]
const LOAD_REMEDIATION: &str = "install it with `brew install libkrun/krun/libkrun`, \
or point DYLD_LIBRARY_PATH at the directory holding it";
#[cfg(not(target_os = "macos"))]
const LOAD_REMEDIATION: &str = "install libkrun, or point LD_LIBRARY_PATH \
at the directory holding it (a snap stages it next to the binary)";

/// The paths dlopen is tried against, in order. A bare soname relies
/// on the loader's own search (LD_LIBRARY_PATH on Linux; on macOS it
/// never reaches the Homebrew prefix, so the keg paths are named).
fn load_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from(SONAME)];
    for prefix in crate::utils::BREW_PREFIXES {
        candidates.push(
            std::path::Path::new(prefix)
                .join("opt/libkrun/lib")
                .join(SONAME),
        );
    }
    candidates
}

macro_rules! symbol {
    ($lib:expr, $name:expr, $ty:ty) => {
        symbol!($lib, $name, $ty, "libkrun too old?")
    };
    ($lib:expr, $name:expr, $ty:ty, $hint:expr) => {{
        // Safety: the symbol is used through the library handle with the
        // signature from libkrun.h written at the call site.
        unsafe { $lib.get::<$ty>($name) }.with_context(|| {
            format!(
                "symbol {} missing: {}",
                String::from_utf8_lossy($name),
                $hint
            )
        })?
    }};
}

/// Map a negative libkrun return (a negated errno) onto an error.
fn krun_ret(ret: i32, name: &str) -> Result<()> {
    if ret < 0 {
        Err(anyhow::Error::from(std::io::Error::from_raw_os_error(-ret)))
            .with_context(|| format!("{name} failed"))
    } else {
        Ok(())
    }
}

/// An open libkrun library handle. Methods map one-to-one onto the C
/// API; negative returns become errno errors with the symbol named.
pub struct Krun {
    lib: libloading::Library,
}

impl Krun {
    /// dlopen libkrun, failing with the install remediation.
    pub fn load() -> Result<Self> {
        let candidates = load_candidates();
        let mut last: Option<anyhow::Error> = None;
        for candidate in &candidates {
            // Safety: dlopen of a fixed name; the handle lives in
            // `self` and every symbol below is used through it.
            let lib = unsafe { libloading::Library::new(candidate) };
            match lib {
                Ok(lib) => return Ok(Krun { lib }),
                Err(err) => last = Some(anyhow::Error::new(err)),
            }
        }
        let tried = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no library name to try")))
            .with_context(|| format!("cannot load {SONAME} (tried {tried}): {LOAD_REMEDIATION}"))
    }

    /// `krun_set_log_level`
    pub fn set_log_level(&self, level: u32) -> Result<()> {
        let f = symbol!(
            self.lib,
            b"krun_set_log_level\0",
            unsafe extern "C" fn(u32) -> i32
        );
        krun_ret(unsafe { f(level) }, "krun_set_log_level")
    }

    /// `krun_create_ctx`, returning the new context id.
    pub fn create_ctx(&self) -> Result<u32> {
        let f = symbol!(
            self.lib,
            b"krun_create_ctx\0",
            unsafe extern "C" fn() -> i32
        );
        let ret = unsafe { f() };
        if ret < 0 {
            krun_ret(ret, "krun_create_ctx")?;
            unreachable!("krun_create_ctx returns a ctx id or a negative errno");
        }
        Ok(ret as u32)
    }

    /// `krun_free_ctx`
    pub fn free_ctx(&self, ctx_id: u32) -> Result<()> {
        let f = symbol!(
            self.lib,
            b"krun_free_ctx\0",
            unsafe extern "C" fn(u32) -> i32
        );
        krun_ret(unsafe { f(ctx_id) }, "krun_free_ctx")
    }

    /// `krun_set_vm_config`
    pub fn set_vm_config(&self, ctx_id: u32, vcpus: u8, ram_mib: u32) -> Result<()> {
        let f = symbol!(
            self.lib,
            b"krun_set_vm_config\0",
            unsafe extern "C" fn(u32, u8, u32) -> i32
        );
        krun_ret(unsafe { f(ctx_id, vcpus, ram_mib) }, "krun_set_vm_config")
    }

    /// `krun_add_disk`: a raw image as a virtio-blk device. An engine
    /// built without the blk feature ships no such symbol, and the
    /// lookup failure names that, not age.
    pub fn add_disk(
        &self,
        ctx_id: u32,
        block_id: &str,
        path: &Path,
        read_only: bool,
    ) -> Result<()> {
        let block_c = CString::new(block_id).context("block id contains a NUL")?;
        let path_c = path_cstring(path)?;
        let f = symbol!(
            self.lib,
            b"krun_add_disk\0",
            unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32,
            "the engine was built without the blk feature, so it ships no block devices"
        );
        // Safety: both CStrings are NUL-terminated and outlive the call.
        krun_ret(
            unsafe { f(ctx_id, block_c.as_ptr(), path_c.as_ptr(), read_only) },
            "krun_add_disk",
        )
    }

    /// `krun_set_root_disk_remount`: which block device the init blob
    /// mounts as `/` and chroots into. A disk must have been added
    /// first; the mount options stay at their defaults.
    pub fn set_root_disk_remount(&self, ctx_id: u32, device: &str, fstype: &str) -> Result<()> {
        let device_c = CString::new(device).context("device contains a NUL")?;
        let fstype_c = CString::new(fstype).context("fstype contains a NUL")?;
        let f = symbol!(
            self.lib,
            b"krun_set_root_disk_remount\0",
            unsafe extern "C" fn(u32, *const c_char, *const c_char, *const c_char) -> i32,
            "the engine was built without the blk feature, so it ships no block devices"
        );
        // Safety: the CStrings are NUL-terminated and outlive the call.
        krun_ret(
            unsafe {
                f(
                    ctx_id,
                    device_c.as_ptr(),
                    fstype_c.as_ptr(),
                    std::ptr::null(),
                )
            },
            "krun_set_root_disk_remount",
        )
    }

    /// `krun_add_virtiofs`: an extra tagged share, mounted guest-side.
    pub fn add_virtiofs(&self, ctx_id: u32, tag: &str, path: &Path) -> Result<()> {
        let tag_c = CString::new(tag).context("share tag contains a NUL")?;
        let path_c = path_cstring(path)?;
        let f = symbol!(
            self.lib,
            b"krun_add_virtiofs\0",
            unsafe extern "C" fn(u32, *const c_char, *const c_char) -> i32
        );
        // Safety: both CStrings are NUL-terminated and outlive the call.
        krun_ret(
            unsafe { f(ctx_id, tag_c.as_ptr(), path_c.as_ptr()) },
            "krun_add_virtiofs",
        )
    }

    /// Attach the vsock device with explicit TSI flags. Without this
    /// the engine runs TSI in heuristics mode, where guest TLS traffic
    /// crawls (~17 KB/s against a CDN); hijacking AF_INET outright
    /// puts it on the fast path. Flag bit 0 hijacks AF_INET/AF_INET6,
    /// bit 1 AF_UNIX.
    pub fn add_vsock(&self, ctx_id: u32, tsi_flags: u32) -> Result<()> {
        // The engine materializes an implicit vsock by default; the
        // explicit one is only accepted once that is switched off.
        let f = symbol!(
            self.lib,
            b"krun_disable_implicit_vsock\0",
            unsafe extern "C" fn(u32) -> i32
        );
        // Safety: plain integer context id.
        krun_ret(unsafe { f(ctx_id) }, "krun_disable_implicit_vsock")?;
        let f = symbol!(
            self.lib,
            b"krun_add_vsock\0",
            unsafe extern "C" fn(u32, u32) -> i32
        );
        // Safety: plain integers; the engine validates the flag bits.
        krun_ret(unsafe { f(ctx_id, tsi_flags) }, "krun_add_vsock")
    }

    /// `krun_set_workdir`
    pub fn set_workdir(&self, ctx_id: u32, workdir: &str) -> Result<()> {
        let workdir_c = CString::new(workdir).context("workdir contains a NUL")?;
        let f = symbol!(
            self.lib,
            b"krun_set_workdir\0",
            unsafe extern "C" fn(u32, *const c_char) -> i32
        );
        // Safety: workdir_c is NUL-terminated and outlives the call.
        krun_ret(unsafe { f(ctx_id, workdir_c.as_ptr()) }, "krun_set_workdir")
    }

    /// `krun_set_exec`: the payload path (relative to the guest root),
    /// its argv, and its envp. A NULL envp would inherit the host
    /// environment, which ecrou never does; callers always pass one.
    pub fn set_exec(
        &self,
        ctx_id: u32,
        exec_path: &str,
        argv: &[String],
        envp: &[String],
    ) -> Result<()> {
        let path_c = CString::new(exec_path).context("exec path contains a NUL")?;
        let argv_c = CArgs::build(argv)?;
        let envp_c = CArgs::build(envp)?;
        let f = symbol!(
            self.lib,
            b"krun_set_exec\0",
            unsafe extern "C" fn(
                u32,
                *const c_char,
                *const *const c_char,
                *const *const c_char,
            ) -> i32
        );
        // Safety: path_c and both pointer arrays are NUL-terminated and
        // outlive the call; the pointed-to CStrings live in the CArgs
        // values held across it.
        krun_ret(
            unsafe { f(ctx_id, path_c.as_ptr(), argv_c.ptr(), envp_c.ptr()) },
            "krun_set_exec",
        )
    }

    /// `krun_set_console_output`: redirect the guest console to a file.
    pub fn set_console_output(&self, ctx_id: u32, path: &Path) -> Result<()> {
        let path_c = path_cstring(path)?;
        let f = symbol!(
            self.lib,
            b"krun_set_console_output\0",
            unsafe extern "C" fn(u32, *const c_char) -> i32
        );
        // Safety: path_c is NUL-terminated and outlives the call.
        krun_ret(
            unsafe { f(ctx_id, path_c.as_ptr()) },
            "krun_set_console_output",
        )
    }

    /// `krun_start_enter`: enter the VM loop on the calling thread.
    /// Blocks until the guest powers off.
    pub fn start_enter(&self, ctx_id: u32) -> Result<()> {
        let f = symbol!(
            self.lib,
            b"krun_start_enter\0",
            unsafe extern "C" fn(u32) -> i32
        );
        krun_ret(unsafe { f(ctx_id) }, "krun_start_enter")
    }

    /// `krun_check_nested_virt`: true when nested virtualization is
    /// available.
    pub fn check_nested_virt(&self) -> Result<bool> {
        let f = symbol!(
            self.lib,
            b"krun_check_nested_virt\0",
            unsafe extern "C" fn() -> i32
        );
        let ret = unsafe { f() };
        if ret < 0 {
            krun_ret(ret, "krun_check_nested_virt")?;
            unreachable!("krun_check_nested_virt returns 0/1 or a negative errno");
        }
        Ok(ret == 1)
    }
}

/// NUL-terminated pointer array for argv/envp-style C arguments. The
/// strings live as long as the value, so the caller holds it across the
/// FFI call.
struct CArgs {
    // Holds the CStrings the pointer array points into; never read
    // directly, but dropping it would dangle every pointer.
    #[allow(dead_code)]
    strings: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl CArgs {
    fn build(items: &[String]) -> Result<Self> {
        let strings = items
            .iter()
            .map(|s| CString::new(s.as_str()).context("argument contains a NUL"))
            .collect::<Result<Vec<_>>>()?;
        let mut ptrs = strings.iter().map(|c| c.as_ptr()).collect::<Vec<_>>();
        ptrs.push(std::ptr::null());
        Ok(CArgs { strings, ptrs })
    }

    fn ptr(&self) -> *const *const c_char {
        self.ptrs.as_ptr()
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains a NUL: {}", path.display()))
}

/// Probe the host hypervisor. On Linux that is KVM: open `/dev/kvm`
/// read-write, then check `KVM_GET_API_VERSION` returns 12. On macOS
/// libkrun runs on the Hypervisor.framework, which exists on Apple
/// Silicon Macs and nothing else.
#[cfg(target_os = "linux")]
pub fn hypervisor_probe() -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        // The variant rides at the top of the chain so it survives the
        // conversion into `crate::Error`; the open(2) error stays
        // reachable as its source.
        .map_err(|err| anyhow::Error::new(Error::KvmUnavailable(err)))?;
    // Safety: the fd is open and an ioctl on /dev/kvm with
    // KVM_GET_API_VERSION reads nothing; only the kernel writes the int
    // return.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), KVM_GET_API_VERSION) };
    if ret != KVM_API_VERSION_EXPECTED {
        return Err(anyhow::Error::new(Error::KvmUnavailable(
            std::io::Error::other(format!(
                "/dev/kvm reports API version {}, expected {}; the kernel module \
                 and the userspace disagree",
                ret, KVM_API_VERSION_EXPECTED
            )),
        )));
    }
    Ok(())
}

/// The Hypervisor.framework check, in two steps. `sysctl
/// kern.hv_support` says the machine has the framework; a real
/// `hv_vm_create` says this process may use it — on current macOS the
/// entitlement check inside the framework is what kills most
/// processes, and the denial used to surface only as an opaque
/// `krun_start_enter` EINVAL after a full image build.
#[cfg(target_os = "macos")]
pub fn hypervisor_probe() -> Result<()> {
    // libkrun's macOS backend is HVF and arm64-only; there is no
    // build for Intel Macs to probe for.
    #[cfg(not(target_arch = "aarch64"))]
    {
        return Err(anyhow::Error::new(Error::HvUnavailable(
            std::io::Error::other(
                "libkrun ships no macOS backend for Intel Macs; an Apple Silicon host is required",
            ),
        )));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let mut supported: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&supported);
        // Safety: sysctlbyname with a fixed name and an int-sized
        // output buffer; the kernel writes only into `supported`.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.hv_support".as_ptr(),
                (&mut supported as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(anyhow::Error::new(Error::HvUnavailable(err))
                .context("reading sysctl kern.hv_support"));
        }
        if supported != 1 {
            return Err(anyhow::Error::new(Error::HvUnavailable(
                std::io::Error::other("this Mac reports no Hypervisor.framework support"),
            )));
        }
        hypervisor_entitlement_probe()
    }
}

/// Create and destroy a real framework VM. `HV_DENIED` is the denial
/// every macOS packaging step has to design around: the framework
/// grants VM creation only to binaries signed with the
/// `com.apple.security.hypervisor` entitlement, so the fix is a
/// re-sign, not a system change. The framework is dlopen'd by
/// absolute path — ecrou links nothing, and the path is stable across
/// releases; if it cannot be opened at all, the probe defers to the
/// engine instead of inventing a verdict.
#[cfg(target_os = "macos")]
fn hypervisor_entitlement_probe() -> Result<()> {
    const HV_DENIED: u32 = 0xfae94007;
    let framework =
        std::path::Path::new("/System/Library/Frameworks/Hypervisor.framework/Hypervisor");
    // Safety: dlopen of a fixed absolute framework path; symbols are
    // resolved immediately and the handle lives for this function.
    let lib = match unsafe { libloading::Library::new(framework) } {
        Ok(lib) => lib,
        Err(_) => return Ok(()),
    };
    let create = symbol!(
        &lib,
        b"hv_vm_create\0",
        unsafe extern "C" fn(*const std::ffi::c_void) -> u32
    );
    let destroy = symbol!(&lib, b"hv_vm_destroy\0", unsafe extern "C" fn() -> u32);
    // Safety: plain calls into the framework; the VM is destroyed
    // right away, or never created.
    let ret = unsafe { create(std::ptr::null()) };
    if ret == 0 {
        unsafe { destroy() };
        return Ok(());
    }
    let reason = if ret == HV_DENIED {
        "the framework denied the process (HV_DENIED): sign the binary with the \
         com.apple.security.hypervisor entitlement, the way packaging does — \
         `codesign --force --sign - --entitlements <plist> <binary>`"
    } else {
        "the framework refused VM creation"
    };
    Err(anyhow::Error::new(Error::HvUnavailable(
        std::io::Error::other(format!("{reason} (hv error 0x{ret:x})")),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_probe_constant_matches_kvm_abi() {
        // KVMIO base 0xAE; KVM_GET_API_VERSION is the first request.
        assert_eq!(KVM_GET_API_VERSION, 0xAE00);
        assert_eq!(KVM_API_VERSION_EXPECTED, 12);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_probe_reports_unavailability_or_works() {
        match hypervisor_probe() {
            Ok(()) => {}
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(msg.contains("/dev/kvm"), "unhelpful probe error: {msg}");
                assert!(msg.contains("usermod"), "remediation missing: {msg}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hypervisor_probe_reports_unavailability_or_works() {
        match hypervisor_probe() {
            Ok(()) => {}
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("Hypervisor.framework"),
                    "unhelpful probe error: {msg}"
                );
            }
        }
    }

    #[test]
    fn load_fails_with_remediation_or_round_trips() {
        match Krun::load() {
            Ok(krun) => {
                // A real libkrun: context create/free round-trips.
                let ctx = krun.create_ctx().expect("create_ctx on real libkrun");
                krun.free_ctx(ctx).expect("free_ctx");
            }
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(msg.contains(SONAME), "bad load error: {msg}");
                // The remediation names the platform's install path:
                // the loader variable on Linux, the brew tap on macOS.
                let hint = if cfg!(target_os = "macos") {
                    "brew install"
                } else {
                    "LD_LIBRARY_PATH"
                };
                assert!(msg.contains(hint), "remediation missing: {msg}");
            }
        }
    }

    #[test]
    fn load_candidates_start_with_the_bare_soname() {
        assert_eq!(load_candidates()[0], std::path::PathBuf::from(SONAME));
        // The brew keg paths exist only as candidates: dlopen tries
        // them in order and they are listed even when absent.
        if cfg!(target_os = "macos") {
            assert!(load_candidates().len() > 1);
        }
    }

    #[test]
    fn ptr_array_is_null_terminated() {
        let args = CArgs::build(&["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(args.ptrs.len(), 3);
        assert!(args.ptrs[2].is_null());
        assert_eq!(args.strings[0].to_str().unwrap(), "a");
    }

    #[test]
    fn ptr_array_rejects_nul_bytes() {
        let items = vec!["bad\0arg".to_string()];
        assert!(CArgs::build(&items).is_err());
    }

    #[test]
    fn krun_ret_maps_negated_errno() {
        let err = krun_ret(-2, "krun_set_vm_config").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("krun_set_vm_config failed"), "{msg}");
        assert!(msg.contains("No such file"), "{msg}");
    }
}
