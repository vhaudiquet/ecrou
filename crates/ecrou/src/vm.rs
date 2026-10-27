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
    /// How much of the engine's own logging reaches stderr
    pub log_level: LogLevel,
    pub no_cache: bool,
}

/// How much of the engine's own logging reaches stderr. The engine
/// logs through the `log` facade, so the consumer's sink decides where
/// lines land; this only picks the filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevel {
    /// Nothing from the engine; the consumer owns all output.
    Quiet,
    /// Warnings and errors. The default.
    #[default]
    Normal,
    /// Engine info as well: lifecycle lines, boot chatter.
    Verbose,
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
            log_level: LogLevel::Normal,
            no_cache: false,
        }
    }
}

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
    let env = compose_env(rootfs.dir(), options, &hostname);

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

    krun.set_log_level(match options.log_level {
        LogLevel::Quiet => 0,   // off
        LogLevel::Normal => 2,  // warn
        LogLevel::Verbose => 3, // info
    })?;
    let ctx = krun.create_ctx()?;
    krun.set_vm_config(ctx, vcpus, ram_mib)?;
    krun.set_root(ctx, rootfs.dir())?;
    for (share, gs) in &share_pairs {
        krun.add_virtiofs(ctx, &gs.tag, &share.host)?;
    }
    // The blob chdirs from KRUN_WORKDIR before exec; the script's cd
    // stays as the fallback path.
    if !options.workdir.is_empty() && options.workdir != "/" {
        krun.set_workdir(ctx, &options.workdir)?;
    }
    krun.set_exec(
        ctx,
        "/ecrou-init",
        &[],
        &env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>(),
    )?;

    // The console goes through a file we stream to real stdout: without
    // a tty, libkrun would route guest output through its logger, and
    // with one, its terminal setup stops a background process with
    // SIGTTOU. A killed parent kills the machine through PDEATHSIG.
    let console_path = rootfs.dir().join(".ecrou-console");
    std::fs::File::create(&console_path)
        .with_context(|| format!("creating {}", console_path.display()))?;

    // Fork with no other threads alive: the child only touches libkrun
    // configuration before entering the VM loop, which never returns (a
    // normal payload exit ends the child through the VMM's own exit).
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Safety: after fork the child runs no threads of its own; the
        // prctl/exit pair has no unwinding preconditions.
        let bail = |msg: String, code: i32| -> ! {
            // The console file is the channel the parent streams: a
            // boot failure belongs in it, next to whatever the guest
            // printed. stderr carries the last-gasp case where the
            // console was never configured; the child is process
            // teardown, not library logging.
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&console_path) {
                let _ = f.write_all(format!("ecrou: {msg}\n").as_bytes());
            }
            eprintln!("Error: {msg}");
            unsafe { libc::_exit(code) }
        };
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // Unwire the console input: with a tty on fd 0 libkrun puts
            // that terminal into raw mode, which fights whoever owns it
            // (and stops a background caller with SIGTTOU). /dev/null
            // keeps the console on its plain path and still forwards a
            // Ctrl-C sent to the process group into the guest.
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if null >= 0 {
                libc::dup2(null, libc::STDIN_FILENO);
                if null != libc::STDIN_FILENO {
                    libc::close(null);
                }
            }
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        }
        if let Err(err) = krun.set_console_output(ctx, &console_path) {
            bail(format!("{err:#}"), 125);
        }
        if let Err(err) = krun.start_enter(ctx) {
            bail(format!("{err:#}"), 125);
        }
        bail("start_enter returned".to_string(), 125);
    }
    if child < 0 {
        return Err(Error::Other(
            anyhow::Error::from(std::io::Error::last_os_error())
                .context("forking the machine process"),
        ));
    }

    let reaped = std::sync::atomic::AtomicI32::new(-1);
    stream_console(&console_path, child, &reaped)?;
    wait_machine(child, &reaped, &exit_file_host)
}

/// Relay the console file to real stdout while the machine runs, so
/// output appears as the guest produces it.
fn stream_console(
    console_path: &Path,
    child: libc::pid_t,
    reaped: &std::sync::atomic::AtomicI32,
) -> Result<()> {
    use std::io::{Read, Seek, Write};
    let mut pos: u64 = 0;
    let mut done = false;
    while !done {
        let len = std::fs::metadata(console_path)
            .with_context(|| format!("stat {}", console_path.display()))?
            .len();
        if len > pos {
            let mut file = std::fs::File::open(console_path)
                .with_context(|| format!("opening {}", console_path.display()))?;
            file.seek(std::io::SeekFrom::Start(pos))
                .with_context(|| format!("seeking {}", console_path.display()))?;
            let mut buf = Vec::with_capacity((len - pos) as usize);
            file.read_to_end(&mut buf)
                .with_context(|| format!("reading {}", console_path.display()))?;
            pos += buf.len() as u64;
            let mut out = std::io::stdout().lock();
            out.write_all(&buf).context("writing console output")?;
            out.flush().ok();
        }
        // Reap-if-exited: the raw status lands in `reaped` for the
        // collector below. WNOWAIT is waitid-only; plain WNOHANG here
        // both reports and reaps.
        let mut status: libc::c_int = 0;
        // Safety: waitpid with WNOHANG only; the pid is our own child.
        let r = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if r == child {
            reaped.store(status, std::sync::atomic::Ordering::SeqCst);
            done = true;
        } else if r < 0 {
            return Err(Error::Other(
                anyhow::Error::from(std::io::Error::last_os_error())
                    .context("waiting for the machine process"),
            ));
        } else {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    // The machine's last console bytes land in the file right before it
    // dies; drain once more past the final metadata check above.
    let len = std::fs::metadata(console_path)
        .with_context(|| format!("stat {}", console_path.display()))?
        .len();
    if len > pos {
        let mut file = std::fs::File::open(console_path)
            .with_context(|| format!("opening {}", console_path.display()))?;
        file.seek(std::io::SeekFrom::Start(pos))
            .with_context(|| format!("seeking {}", console_path.display()))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .with_context(|| format!("reading {}", console_path.display()))?;
        let mut out = std::io::stdout().lock();
        out.write_all(&buf).context("writing console output")?;
        out.flush().ok();
    }
    Ok(())
}

/// Collect the machine: its exit status is the payload's exit code (the
/// guest init blob reports it and the VMM exits the child with it).
fn wait_machine(
    child: libc::pid_t,
    reaped: &std::sync::atomic::AtomicI32,
    exit_file_host: &Path,
) -> Result<i32> {
    let reaped_status = reaped.load(std::sync::atomic::Ordering::SeqCst);
    if reaped_status >= 0 {
        return decode_status(reaped_status, exit_file_host);
    }
    let mut status: libc::c_int = 0;
    // Safety: waitpid on our own child; the pointer points at valid
    // storage.
    let ret = unsafe { libc::waitpid(child, &mut status, 0) };
    if ret < 0 {
        return Err(Error::Other(
            anyhow::Error::from(std::io::Error::last_os_error())
                .context("waiting for the machine process"),
        ));
    }
    decode_status(status, exit_file_host)
}

fn decode_status(status: libc::c_int, exit_file_host: &Path) -> Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let code = if let Some(sig) = std::process::ExitStatus::from_raw(status).signal() {
        128 + sig
    } else {
        std::process::ExitStatus::from_raw(status)
            .code()
            .unwrap_or(125)
    };
    if code == 125 {
        // Boot failure: the init blob never reported a code. The
        // control file is the diagnostic when it ran partway.
        if let Ok(s) = std::fs::read_to_string(exit_file_host) {
            if let Ok(c) = s.trim().parse::<i32>() {
                return Ok(c);
            }
        }
        return Err(Error::Other(anyhow!(
            "the machine failed to boot; run with --quiet off and check the \
             console output above"
        )));
    }
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
    let shell = utils::detect_shell(rootfs_dir).to_string();
    match options.command.as_slice() {
        // No command: the image's shell.
        [] => vec![shell],
        // One quoted shell line: `ecrou run alpine -- "apk add x && y"`.
        // The whole line would otherwise be execvp'd as one program
        // name; the consumer clearly asked for a shell line.
        [line] if line.contains(char::is_whitespace) => {
            vec![shell, "-c".to_string(), line.clone()]
        }
        // An argv vector, taken literally.
        cmd => cmd.to_vec(),
    }
}

/// The guest environment, composed from scratch: a usable base, the
/// workspace mount point, and the caller's pairs. Never the host
/// environment.
fn compose_env(rootfs_dir: &Path, options: &RunOptions, hostname: &str) -> Vec<(String, String)> {
    let host_term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let mut env = vec![
        ("HOME".to_string(), "/root".to_string()),
        ("USER".to_string(), "root".to_string()),
        (
            "SHELL".to_string(),
            utils::detect_shell(rootfs_dir).to_string(),
        ),
        ("TERM".to_string(), host_term),
        ("HOSTNAME".to_string(), hostname.to_string()),
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

    /// A scratch with just a shell, for payload_command tests.
    fn shell_rootfs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin/sh"), "").unwrap();
        dir
    }

    #[test]
    fn payload_defaults_to_the_image_shell() {
        let rootfs = shell_rootfs();
        let options = RunOptions::default();
        assert_eq!(
            payload_command(&options, rootfs.path()),
            vec!["/bin/sh".to_string()]
        );
    }

    #[test]
    fn payload_argv_is_taken_literally() {
        let rootfs = shell_rootfs();
        let options = RunOptions {
            command: vec!["uname".to_string(), "-a".to_string()],
            ..RunOptions::default()
        };
        assert_eq!(
            payload_command(&options, rootfs.path()),
            vec!["uname".to_string(), "-a".to_string()]
        );
        // Even when it looks like a shell invocation: the consumer
        // asked for argv.
        let options = RunOptions {
            command: vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
            ..RunOptions::default()
        };
        assert_eq!(
            payload_command(&options, rootfs.path()),
            vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()]
        );
    }

    #[test]
    fn payload_single_quoted_line_runs_through_the_shell() {
        let rootfs = shell_rootfs();
        let options = RunOptions {
            command: vec!["apk add fastfetch && fastfetch".to_string()],
            ..RunOptions::default()
        };
        assert_eq!(
            payload_command(&options, rootfs.path()),
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "apk add fastfetch && fastfetch".to_string()
            ]
        );
    }

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
        let env = compose_env(&dir, &options, "ecrou-vm-x");
        assert!(env
            .iter()
            .any(|(k, v)| k == "ECROU_TEST_MARK" && v == "present"));
        assert!(env.iter().all(|(k, _)| k != "ECROU_TEST_UNSET"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "PATH" && v.contains("/usr/bin")));
    }
}
