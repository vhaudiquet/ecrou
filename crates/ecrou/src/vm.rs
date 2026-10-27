//! One-shot machines: prepare a rootfs, boot it on libkrun, run one
//! payload with live stdio, propagate its exit code, throw the machine
//! away.
//!
//! The flow is: KVM probe, rootfs prepare (cache-aware download and
//! extraction), guest init written into the scratch rootfs, libkrun
//! context wired to it, `krun_start_enter` blocking on this thread, then
//! the control file read back for the exit code. The VMM runs inside
//! this process, so a kill of the process is a kill of the machine (T4).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::guest::{self, GuestShare};
use crate::krun::{self, Krun};
use crate::progress::Progress;
use crate::rootfs::{PrepareRequest, PreparedRootfs, RootfsCache};
use crate::utils::{self, HOSTNAME_SUFFIX_LEN};
use crate::{Error, Result};

/// A directory crossing into the machine: host path, guest mount point,
/// and whether the guest may write.
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    pub host: PathBuf,
    pub guest: String,
    pub read_only: bool,
}

/// Everything a one-shot run needs. Defaults select sane behavior: host
/// arch, 1G of memory, the image's default shell, a generated hostname.
/// The host environment is never inherited; compose `env` instead.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub image: String,
    pub arch: String,
    pub memory: String,
    pub workspace: Option<Share>,
    pub shares: Vec<Share>,
    pub command: Vec<String>,
    pub workdir: String,
    pub env: Vec<(String, String)>,
    pub hostname: String,
    pub quiet: bool,
    pub no_cache: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            image: String::new(),
            arch: String::new(),
            memory: "1G".to_string(),
            workspace: None,
            shares: Vec::new(),
            command: Vec::new(),
            workdir: "/".to_string(),
            env: Vec::new(),
            hostname: String::new(),
            quiet: false,
            no_cache: false,
        }
    }
}

/// libkrun log levels (0 off, 1 error, 2 warn, 3 info, 4 debug).
const LOG_LEVEL_INFO: u32 = 3;
const LOG_LEVEL_QUIET: u32 = 1;

/// Run one machine to completion and return the payload's exit code.
pub fn run(options: &RunOptions, progress: &dyn Progress) -> Result<i32> {
    if options.image.is_empty() {
        return Err(Error::Other(anyhow!("no image given")));
    }

    krun::kvm_probe()?;
    let krun = Krun::load()?;

    let ram_mib = parse_memory_mib(&options.memory).map_err(Error::Other)?;
    let vcpus = vcpus();

    let mut request = PrepareRequest::new(options.image.clone());
    request.arch = options.arch.clone();
    request.no_cache = options.no_cache;
    let cache = RootfsCache::new(RootfsCache::default_dir()?);
    let rootfs = cache
        .prepare(&request, progress)
        .context("preparing the rootfs")?;

    let hostname = if options.hostname.is_empty() {
        format!("ecrou-vm-{}", utils::random_suffix(HOSTNAME_SUFFIX_LEN))
    } else {
        options.hostname.clone()
    };

    let share_pairs = collect_shares(&rootfs, options).map_err(Error::Other)?;
    let shares = share_pairs
        .iter()
        .map(|(_, gs)| gs.clone())
        .collect::<Vec<_>>();
    let payload = payload_command(options, rootfs.dir());
    let env = compose_env(rootfs.dir(), options);

    // The guest init is generated for every run: it is the exit-code
    // contract (it writes the control file), the share mounter, and the
    // hostname applier.
    let init_path = rootfs.dir().join("ecrou-init");
    let script = guest::init_script(&shares, &payload, &options.workdir, &hostname);
    std::fs::write(&init_path, script)
        .with_context(|| format!("writing {}", init_path.display()))?;
    set_exec_bit(&init_path)?;

    let exit_file_host = rootfs.dir().join(
        guest::EXIT_FILE
            .strip_prefix('/')
            .expect("EXIT_FILE is absolute at the guest root"),
    );
    let _ = std::fs::remove_file(&exit_file_host);

    krun.set_log_level(if options.quiet {
        LOG_LEVEL_QUIET
    } else {
        LOG_LEVEL_INFO
    })?;
    let ctx = krun.create_ctx()?;
    krun.set_vm_config(ctx, vcpus, ram_mib)?;
    krun.set_root(ctx, rootfs.dir())?;
    for (share, gs) in &share_pairs {
        krun.add_virtiofs(ctx, &gs.tag, &share.host)?;
    }
    // The init script owns the workdir (it cds before the payload), so
    // the native krun_set_workdir stays unused.
    krun.set_exec(
        ctx,
        "/ecrou-init",
        &[],
        &env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>(),
    )?;

    // Blocks until the guest powers off; the machine dies with this
    // process if the consumer kills us first.
    krun.start_enter(ctx)?;

    let code = std::fs::read_to_string(&exit_file_host)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .ok_or_else(|| {
            Error::Other(anyhow!(
                "the guest did not report an exit code through {}; the machine \
                 failed to boot or the init did not run to completion",
                guest::EXIT_FILE
            ))
        })?;
    Ok(code)
}

/// Validate and tag the shares: the workspace first, then the named
/// ones, each with a stable virtiofs tag. Returns the vm share (host
/// path) paired with its guest view (tag, target, ro flag) so the
/// script and the virtiofs setup stay in the same order. Host paths
/// must exist, and no share may overlap the scratch rootfs, or the
/// guest could rewrite its own init.
fn collect_shares(
    rootfs: &PreparedRootfs,
    options: &RunOptions,
) -> anyhow::Result<Vec<(Share, GuestShare)>> {
    let mut all = Vec::new();
    if let Some(ws) = &options.workspace {
        all.push(ws.clone());
    }
    all.extend(options.shares.iter().cloned());

    let mut out = Vec::with_capacity(all.len());
    for (i, share) in all.iter().enumerate() {
        if !share.host.is_dir() {
            return Err(anyhow!(
                "share host path does not exist: {}",
                share.host.display()
            ));
        }
        if rootfs.dir().starts_with(&share.host) || share.host.starts_with(rootfs.dir()) {
            return Err(anyhow!(
                "share {} overlaps the scratch rootfs",
                share.host.display()
            ));
        }
        out.push((
            share.clone(),
            GuestShare {
                tag: format!("ecrou{i}"),
                target: share.guest.clone(),
                read_only: share.read_only,
            },
        ));
    }
    Ok(out)
}

/// The guest payload: the caller's argv, or the rootfs's default shell.
fn payload_command(options: &RunOptions, rootfs_dir: &Path) -> Vec<String> {
    if !options.command.is_empty() {
        return options.command.clone();
    }
    vec![utils::detect_shell(rootfs_dir).to_string()]
}

/// The guest environment, composed from scratch: a usable base, the
/// workspace mount point, and the caller's pairs. Never the host
/// environment.
fn compose_env(rootfs_dir: &Path, options: &RunOptions) -> Vec<(String, String)> {
    let host_term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let mut env = vec![
        ("HOME".to_string(), "/root".to_string()),
        ("USER".to_string(), "root".to_string()),
        (
            "SHELL".to_string(),
            utils::detect_shell(rootfs_dir).to_string(),
        ),
        ("TERM".to_string(), host_term),
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
    ];
    if let Some(ws) = &options.workspace {
        env.push(("ECROU_WORKSPACE".to_string(), ws.guest.clone()));
    }
    for (k, v) in &options.env {
        env.push((k.clone(), v.clone()));
    }
    env
}

fn set_exec_bit(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))
}

/// libkrun wants a vcpu count; all logical cpus is the sensible default
/// for a one-shot machine.
fn vcpus() -> u8 {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, u8::MAX as usize) as u8)
        .unwrap_or(1)
}

/// Parse "1G" / "512M" / "262144K" into MiB, rounding a fractional MiB
/// up (libkrun's config takes whole MiB; one MiB is the floor).
fn parse_memory_mib(s: &str) -> anyhow::Result<u32> {
    let trimmed = s.trim();
    let (digits, unit) = match trimmed.chars().last() {
        Some(c @ ('K' | 'k' | 'M' | 'm' | 'G' | 'g')) => {
            (&trimmed[..trimmed.len() - 1], c.to_ascii_lowercase())
        }
        Some(_) => (trimmed, 'm'),
        None => return Err(anyhow!("empty memory size")),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid memory size {s:?}"))?;
    if n == 0 {
        return Err(anyhow!("memory size must be positive: {s:?}"));
    }
    let mib: u64 = match unit {
        'k' => n.div_ceil(1024),
        'm' => n,
        'g' => n
            .checked_mul(1024)
            .ok_or_else(|| anyhow!("memory size out of range: {s:?}"))?,
        _ => unreachable!("unit is one of k m g"),
    };
    u32::try_from(mib)
        .map(|m| m.max(1))
        .map_err(|_| anyhow!("memory size out of range: {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_parses_to_whole_mib() {
        assert_eq!(parse_memory_mib("1G").unwrap(), 1024);
        assert_eq!(parse_memory_mib("512M").unwrap(), 512);
        assert_eq!(parse_memory_mib("2048K").unwrap(), 2);
        assert_eq!(parse_memory_mib("4096").unwrap(), 4096);
        assert!(parse_memory_mib("0M").is_err());
        assert!(parse_memory_mib("").is_err());
        assert!(parse_memory_mib("lots").is_err());
        assert!(parse_memory_mib("9999999G").is_err());
    }

    #[test]
    fn default_options_are_sane() {
        let o = RunOptions::default();
        assert_eq!(o.memory, "1G");
        assert_eq!(o.workdir, "/");
        assert!(o.command.is_empty());
        assert!(o.workspace.is_none());
        assert!(!o.no_cache);
    }

    #[test]
    fn ki_rounds_fractional_mib_up() {
        assert_eq!(parse_memory_mib("1K").unwrap(), 1);
        assert_eq!(parse_memory_mib("1024K").unwrap(), 1);
        assert_eq!(parse_memory_mib("1025K").unwrap(), 2);
    }

    #[test]
    fn compose_env_never_inherits_the_host() {
        // compose_env reads the host TERM and nothing else from the
        // process environment.
        let dir = std::env::temp_dir();
        let mut options = RunOptions::default();
        options
            .env
            .push(("ECROU_TEST_MARK".to_string(), "present".to_string()));
        let env = compose_env(&dir, &options);
        assert!(env
            .iter()
            .any(|(k, v)| k == "ECROU_TEST_MARK" && v == "present"));
        assert!(env.iter().all(|(k, _)| k != "ECROU_TEST_UNSET"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "PATH" && v.contains("/usr/bin")));
    }
}
