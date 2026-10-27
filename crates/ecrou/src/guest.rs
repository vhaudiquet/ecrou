//! The guest init: a script provisioned into the scratch rootfs before
//! boot. It mounts the virtiofs shares, runs the payload, writes the
//! payload's exit code to the control file the host reads back, and
//! powers the machine off. As PID 1 it is deliberately the only thing
//! that runs; there is no guest agent.

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

/// Shell-quote a string for safe interpolation into the init script.
/// Single quotes wrap everything; an embedded single quote closes,
/// escapes, and reopens.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Render the guest init. The payload runs as a child (never `exec`):
/// the init must outlive it to write the exit code and power the
/// machine off.
pub fn init_script(shares: &[GuestShare], payload: &[String], workdir: &str) -> String {
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

    script.push_str(&format!(
        "\ncd {} || exit 126\n\n",
        sh_quote(if workdir.is_empty() { "/" } else { workdir })
    ));

    for arg in payload {
        script.push_str(&sh_quote(arg));
        script.push(' ');
    }
    script.push_str("\n\ncode=$?\n");
    script.push_str(&format!("echo \"$code\" > {}\n", sh_quote(EXIT_FILE)));
    // sysrq poweroff: works without a poweroff binary in the rootfs. If
    // sysrq is disabled the machine lingers and the consumer's kill
    // (T4) is the backstop.
    script.push_str("echo o > /proc/sysrq-trigger\nexit \"$code\"\n");
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
        let script = init_script(&shares, &["true".to_string()], "/");
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
        let script = init_script(&[], &payload, "/");
        assert!(script.contains(r"'/bin/bash' '-c' 'echo $HOME; rm -rf '\''/'\''' "));
        // The payload line must appear after the cd and before the code
        // capture, so its exit code is what gets written.
        let cd = script.find("cd '/").unwrap();
        let payload_at = script.find("'/bin/bash'").unwrap();
        let capture = script.find("code=$?").unwrap();
        assert!(cd < payload_at && payload_at < capture);
    }

    #[test]
    fn writes_exit_code_and_powers_off() {
        let script = init_script(&[], &["true".to_string()], "/work");
        assert!(script.contains(&format!("echo \"$code\" > '{}'", EXIT_FILE)));
        assert!(script.contains("echo o > /proc/sysrq-trigger"));
        assert!(script.contains("exit \"$code\""));
        // No exec of the payload: the init must survive it to report.
        assert!(!script.contains("exec "));
    }

    #[test]
    fn empty_workdir_falls_back_to_root() {
        let script = init_script(&[], &["true".to_string()], "");
        assert!(script.contains("cd '/'"));
    }
}
