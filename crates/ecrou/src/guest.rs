//! The guest init: a script provisioned into the scratch rootfs before
//! boot. It mounts the virtiofs shares, runs the payload, and writes
//! the payload's exit code to the control file. The exit itself ends
//! the machine: libkrun's init blob reaps the script, reports the code,
//! and the VMM terminates the host process with it. This script is
//! deliberately the only thing that runs; there is no guest agent.

/// A share as the init sees it: a virtiofs tag to mount at a guest path.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestShare {
    pub tag: String,
    pub target: String,
    pub read_only: bool,
}

/// The control file, at the guest root: the init writes the payload's
/// exit code here, and `vm::run` reads it back host-side from the same
/// scratch directory.
pub const EXIT_FILE: &str = "/.ecrou-exit";

/// The guest path the capture share mounts at, when the run captures
/// the payload's output. Reserved: no caller share may take it.
pub const CAPTURE_DIR: &str = "/.ecrou-io";
/// The payload's stdout, host-side under the capture directory.
pub const CAPTURE_STDOUT: &str = "/.ecrou-io/stdout";
/// The payload's stderr, host-side under the capture directory.
pub const CAPTURE_STDERR: &str = "/.ecrou-io/stderr";

/// Where the init points the payload's stdout and stderr when the run
/// captures them: guest paths under [`CAPTURE_DIR`], which the caller
/// reads host-side as files in its capture directory.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestCapture {
    pub stdout: String,
    pub stderr: String,
}

/// Shell-quote a string for safe interpolation into the init script.
/// Single quotes wrap everything; an embedded single quote closes,
/// escapes, and reopens.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Render the guest init. The payload runs as a child (never `exec`):
/// the init must outlive it to write the exit code and power the
/// machine off.
pub fn init_script(
    shares: &[GuestShare],
    payload: &[String],
    workdir: &str,
    hostname: &str,
    capture: Option<&GuestCapture>,
) -> String {
    let mut script = String::from(
        "#!/bin/sh\n\
         # ecrou guest init: mount the shares, run the payload, report its\n\
         # exit code, power off. Generated; do not edit.\n\
         set -u\n\n\
         mount_share() {\n\
         \x20   tag=\"$1\"; target=\"$2\"; ro=\"$3\"\n\
         \x20   mkdir -p \"$target\"\n\
         \x20   if [ \"$ro\" = \"ro\" ]; then\n\
         \x20       mount -t virtiofs -o ro \"$tag\" \"$target\"\n\
         \x20   else\n\
         \x20       mount -t virtiofs \"$tag\" \"$target\"\n\
         \x20   fi\n\
         }\n\n",
    );

    for share in shares {
        let ro = if share.read_only { "ro" } else { "rw" };
        script.push_str(&format!(
            "mount_share {} {} {}\n",
            sh_quote(&share.tag),
            sh_quote(&share.target),
            sh_quote(ro),
        ));
    }

    // Best effort: some rootfs trees ship their own /etc/hostname.
    script.push_str(&format!(
        "echo {} > /proc/sys/kernel/hostname 2>/dev/null || true\n",
        sh_quote(hostname)
    ));
    script.push_str(&format!(
        "echo {} > /etc/hostname 2>/dev/null || true\n\n",
        sh_quote(hostname)
    ));

    script.push_str(&format!(
        "cd {} || exit 126\n\n",
        sh_quote(if workdir.is_empty() { "/" } else { workdir })
    ));

    // Captured runs wrap the payload in a redirection group: the
    // streams cross the capture share as files, and the group's exit
    // status - the payload's - is what `code=$?` reads below. The
    // console stays free for kernel and init diagnostics.
    if capture.is_some() {
        script.push_str("{ ");
    }
    for arg in payload {
        script.push_str(&sh_quote(arg));
        script.push(' ');
    }
    if let Some(cap) = capture {
        script.push_str(
            format!(
                "; }} > {} 2> {} ",
                sh_quote(&cap.stdout),
                sh_quote(&cap.stderr)
            )
            .as_str(),
        );
    }
    script.push_str("\n\ncode=$?\n");
    script.push_str(&format!("echo \"$code\" > {}\n", sh_quote(EXIT_FILE)));
    // When this script exits, the guest init blob reaps it, reports the
    // exit code to the VMM, and the VMM ends the host process with it:
    // no poweroff helper is needed in the rootfs. The control file write
    // above must precede this line; it is the diagnostic when the boot
    // dies before the init ran.
    script.push_str("exit \"$code\"\n");
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("$HOME `id` && rm"), "'$HOME `id` && rm'");
        assert_eq!(sh_quote("new\nline"), "'new\nline'");
    }

    #[test]
    fn mounts_each_share_with_its_ro_flag() {
        let shares = vec![
            GuestShare {
                tag: "ws0".into(),
                target: "/workspace".into(),
                read_only: false,
            },
            GuestShare {
                tag: "sk1".into(),
                target: "/mnt/skills".into(),
                read_only: true,
            },
        ];
        let script = init_script(&shares, &["true".to_string()], "/", "ecrou-vm-x", None);
        assert!(script.contains("mount_share 'ws0' '/workspace' 'rw'"));
        assert!(script.contains("mount_share 'sk1' '/mnt/skills' 'ro'"));
    }

    #[test]
    fn payload_runs_unquoted_free_of_interpolation() {
        let payload = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            "echo $HOME; rm -rf '/'".to_string(),
        ];
        let script = init_script(&[], &payload, "/", "ecrou-vm-x", None);
        assert!(script.contains(r"'/bin/bash' '-c' 'echo $HOME; rm -rf '\''/'\''' "));
        // The payload line must appear after the cd and before the code
        // capture, so its exit code is what gets written.
        let cd = script.find("cd '/").unwrap();
        let payload_at = script.find("'/bin/bash'").unwrap();
        let capture = script.find("code=$?").unwrap();
        assert!(cd < payload_at && payload_at < capture);
    }

    #[test]
    fn writes_exit_code_then_exits() {
        let script = init_script(&[], &["true".to_string()], "/work", "ecrou-vm-x", None);
        assert!(script.contains(&format!("echo \"$code\" > '{}'", EXIT_FILE)));
        // The file write must precede the exit: it is the only
        // diagnostic when a boot dies without the init running.
        let write = script.find("echo \"$code\"").unwrap();
        let exit = script.find("exit \"$code\"").unwrap();
        assert!(write < exit);
        // No exec of the payload: the init must survive it to report.
        assert!(!script.contains("exec "));
        // The libkrunfw kernel has no sysrq; ending the machine is the
        // init blob's job when this script exits.
        assert!(!script.contains("sysrq"));
    }

    #[test]
    fn hostname_lands_in_proc_and_etc() {
        let script = init_script(&[], &["true".to_string()], "/", "ecrou-vm-x", None);
        assert!(script.contains("echo 'ecrou-vm-x' > /proc/sys/kernel/hostname"));
        assert!(script.contains("echo 'ecrou-vm-x' > /etc/hostname"));
    }

    #[test]
    fn capture_wraps_the_payload_and_points_at_the_reserved_share() {
        let cap = GuestCapture {
            stdout: CAPTURE_STDOUT.into(),
            stderr: CAPTURE_STDERR.into(),
        };
        let script = init_script(
            &[],
            &["true".to_string()],
            "/work",
            "ecrou-vm-x",
            Some(&cap),
        );
        // The payload line is a redirection group: the group's exit
        // status, which `code=$?` reads, is the payload's.
        assert!(script.contains("{ 'true' ; } > '/.ecrou-io/stdout' 2> '/.ecrou-io/stderr' "));
        let payload_at = script.find("{ 'true'").unwrap();
        let capture_at = script.find("code=$?").unwrap();
        assert!(payload_at < capture_at);
    }

    #[test]
    fn capture_paths_are_quoted_like_any_operand() {
        let cap = GuestCapture {
            stdout: "/.ecrou-io/stdout".into(),
            stderr: "/.ecrou-io/stderr".into(),
        };
        let script = init_script(&[], &["echo it's".to_string()], "/", "x", Some(&cap));
        assert!(script.contains("echo it'\\''s"));
    }

    #[test]
    fn empty_workdir_falls_back_to_root() {
        let script = init_script(&[], &["true".to_string()], "", "ecrou-vm-x", None);
        assert!(script.contains("cd '/'"));
    }
}
