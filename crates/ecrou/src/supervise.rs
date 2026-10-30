//! The machine process: forked from this one, its console relayed to
//! real stdout, its exit status collected. libkrun runs the VMM inside
//! the child, so the process tree stays flat and a SIGKILL of the
//! parent takes the machine with it (SPEC T4).

use std::path::Path;

use anyhow::{anyhow, Context};

use crate::krun::Krun;
use crate::{Error, Result};

/// Fork the machine process and enter the VM loop inside it. The
/// child only touches libkrun configuration before entering the VM
/// loop, which never returns: a normal payload exit ends the child
/// through the VMM's own exit.
pub(crate) fn spawn_machine(
    krun: &Krun,
    ctx: u32,
    console_path: &Path,
    interactive: bool,
) -> Result<libc::pid_t> {
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
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(console_path) {
                let _ = f.write_all(format!("ecrou: {msg}\n").as_bytes());
            }
            eprintln!("Error: {msg}");
            unsafe { libc::_exit(code) }
        };
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // Headless only: unwire the console input (with a tty on
            // fd 0 libkrun puts that terminal into raw mode, which
            // fights whoever owns it, and stops a background caller
            // with SIGTTOU). /dev/null keeps the console on its plain
            // path and still forwards a Ctrl-C sent to the process
            // group into the guest. An interactive machine wants the
            // opposite: fd 0 is its console input by design.
            if !interactive {
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, libc::STDIN_FILENO);
                    if null != libc::STDIN_FILENO {
                        libc::close(null);
                    }
                }
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            }
        }
        if !interactive {
            if let Err(err) = krun.set_console_output(ctx, console_path) {
                bail(format!("{err:#}"), 125);
            }
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

    if child < 0 {
        return Err(Error::Other(
            anyhow::Error::from(std::io::Error::last_os_error())
                .context("forking the machine process"),
        ));
    }
    Ok(child)
}

/// The host terminal's mode, saved before a tty machine boots and
/// restored after it ends.
pub(crate) struct TtyRestore {
    saved: Option<libc::termios>,
}

impl TtyRestore {
    pub(crate) fn save(wanted: bool) -> Self {
        let saved = if wanted && unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            // Safety: tcgetattr on our own stdin; the storage is valid
            // and the call fills the whole struct or fails.
            let mut term: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } == 0 {
                Some(term)
            } else {
                None
            }
        } else {
            None
        };
        TtyRestore { saved }
    }

    pub(crate) fn restore(&self) {
        if let Some(term) = &self.saved {
            // Safety: the termios value came from tcgetattr on the
            // same fd; TCSADRAIN flushes output before the switch.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, term) };
        }
    }
}

/// Relay the console file to real stdout while the machine runs, so
/// output appears as the guest produces it.
pub(crate) fn stream_console(
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
        let r = loop {
            let r = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
            // A caught signal must not abort the run and orphan the
            // machine: EINTR only says look again.
            if r == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            break r;
        };
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
pub(crate) fn wait_machine(
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
    let ret = loop {
        let r = unsafe { libc::waitpid(child, &mut status, 0) };
        // EINTR: a caught signal interrupts the wait, it does not end
        // it. Retry instead of erroring out and orphaning the machine.
        if r == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        break r;
    };
    if ret < 0 {
        return Err(Error::Other(
            anyhow::Error::from(std::io::Error::last_os_error())
                .context("waiting for the machine process"),
        ));
    }
    decode_status(status, exit_file_host)
}

/// Translate the machine child's raw wait status into the result of a
/// run. Only the guest init can mint a guest exit code: it reports the
/// payload's code through the control file and the VMM exits the child
/// with it. Everything else is ecrou's own machinery talking, and is
/// an error, never a code — the shell's 128+n convention would
/// otherwise masquerade a SIGKILLed or OOM-killed VMM as the payload
/// having died by that signal.
fn decode_status(status: libc::c_int, exit_file_host: &Path) -> Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::ExitStatus::from_raw(status);
    if let Some(sig) = status.signal() {
        // The init catches the payload's signals and reports them as
        // codes, so a signal on the machine child means the VMM or the
        // engine died: the payload never reported.
        return Err(Error::Other(anyhow!(
            "the machine process died from signal {sig}; this is an \
             engine or hypervisor failure, not the payload's exit code"
        )));
    }
    let code = status.code().unwrap_or(125);
    match code {
        // Reserved by the machinery itself: 125 is the child's bail
        // code (boot failure), 126 is the init refusing to run the
        // payload (a share that would not mount, a missing workdir). A
        // payload that genuinely exits either code reported through
        // the control file first — the file is the arbiter.
        125 | 126 => {
            if let Ok(s) = std::fs::read_to_string(exit_file_host) {
                if let Ok(c) = s.trim().parse::<i32>() {
                    return Ok(c);
                }
            }
            let cause = if code == 125 {
                "the machine failed to boot"
            } else {
                "the guest init aborted before running the payload: a \
                 share failed to mount, or the workdir does not exist"
            };
            Err(Error::Other(anyhow!(
                "{cause}; check the console output above"
            )))
        }
        _ => Ok(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw waitpid status for a child exited with `code`.
    fn exited(code: i32) -> libc::c_int {
        code << 8
    }

    /// The raw waitpid status for a child killed by `sig`.
    fn signaled(sig: i32) -> libc::c_int {
        sig
    }

    #[test]
    fn a_signaled_machine_is_an_engine_failure() {
        let dir = tempfile::tempdir().unwrap();
        for raw in [signaled(libc::SIGSEGV), signaled(libc::SIGKILL), signaled(libc::SIGTERM)] {
            let err = decode_status(raw, &dir.path().join("absent")).unwrap_err();
            assert!(
                err.to_string().contains("engine or hypervisor failure"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_boot_failure_is_an_error_and_a_payload_code_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let exit_file = dir.path().join(".ecrou-exit");
        // 125 with no report: the child's bail code.
        let err = decode_status(exited(125), &exit_file).unwrap_err();
        assert!(err.to_string().contains("failed to boot"), "{err}");
        // 126 with no report: the init refused (share, workdir).
        let err = decode_status(exited(126), &exit_file).unwrap_err();
        assert!(err.to_string().contains("share failed to mount"), "{err}");
        // A payload that genuinely exited a reserved code reported
        // through the control file first.
        std::fs::write(&exit_file, "7").unwrap();
        assert_eq!(decode_status(exited(125), &exit_file).unwrap(), 7);
        std::fs::write(&exit_file, "3").unwrap();
        assert_eq!(decode_status(exited(126), &exit_file).unwrap(), 3);
    }

    #[test]
    fn an_ordinary_code_needs_no_control_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            decode_status(exited(42), &dir.path().join("absent")).unwrap(),
            42
        );
        assert_eq!(
            decode_status(exited(0), &dir.path().join("absent")).unwrap(),
            0
        );
        // A garbage control file does not turn a normal code into an
        // error either — the code came through the VMM's own exit.
        let exit_file = dir.path().join(".ecrou-exit");
        std::fs::write(&exit_file, "not a number").unwrap();
        assert_eq!(decode_status(exited(42), &exit_file).unwrap(), 42);
    }
}
