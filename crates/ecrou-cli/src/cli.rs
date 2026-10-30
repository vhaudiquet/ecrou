//! Flag definitions and the small parsing helpers they need.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ecrou",
    version,
    about = "Run commands in disposable microVMs on libkrun",
    after_help = "Examples:\n  ecrou run -it alpine                      a shell in the machine\n  ecrou run ubuntu -- uname -a              one command\n  ecrou run -v $PWD:/src ubuntu -- make     share the working tree into the machine\n  ecrou pull alpine                         fetch or refresh an image\n  ecrou probe                               check for requirements"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Print diagnostics, including engine messages
    #[arg(long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a command in a microVM from an image
    ///
    /// IMAGE is a distro name (alpine, ubuntu:noble), an OCI image
    /// reference (ghcr.io/org/image:tag, ubuntu@sha256:...), or a
    /// local rootfs archive (a path: ./rootfs.tar.gz). The machine
    /// boots once for the command, streams its stdio, and the run
    /// exits with the command's exit code; the machine is thrown
    /// away. Persistence crosses a --volume share or it does not
    /// happen.
    Run(RunArgs),
    /// Download an image from a registry
    ///
    /// Unpinned OCI tags are re-checked against the registry digest
    /// and floating distro aliases (latest, lts, edge) re-resolved;
    /// only an absent or moved entry is downloaded. `run` never
    /// contacts a registry on a cached image: this is the freshness
    /// operation, docker-pull semantics.
    Pull {
        /// Distribution name or OCI image reference
        image: String,
        /// Re-download even when the entry is present and current
        #[arg(long)]
        no_cache: bool,
    },
    /// Check for ecrou requirements
    ///
    /// KVM working (device, API version, nested-virt state), the
    /// engine (libkrun.so.1) loadable, and the image cache usable.
    /// The support tool and the confinement diagnostic.
    Probe,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Distro name, OCI image reference, or a local rootfs archive
    /// path (./rootfs.tar.gz)
    pub image: String,
    /// Keep the machine's stdin open: the host's stdin feeds the
    /// guest, so shells and prompts read typed input
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Run the guest console on the terminal (implies --interactive);
    /// needs a terminal on stdin and stdout
    #[arg(short = 't', long)]
    pub tty: bool,
    /// Command to run in the machine; the image's default shell when
    /// omitted. An argument vector after `--` is taken literally; a
    /// single quoted line runs through the image's shell as `sh -c`.
    #[arg(last = true)]
    pub command: Vec<String>,
    /// Share a host directory into the machine as HOST:GUEST[:ro]
    /// (repeatable; the guest path must be absolute)
    #[arg(short = 'v', long = "volume")]
    pub volumes: Vec<String>,
    /// Guest memory size in MiB, or with a K/M/G suffix (2048, 512M, 1G)
    #[arg(short = 'm', long, default_value = "1G")]
    pub memory: String,
    /// Working directory inside the machine
    #[arg(short = 'w', long, default_value = "/")]
    pub workdir: String,
    /// Guest environment KEY=VALUE (repeatable; the host environment
    /// is never inherited)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Silence engine and kernel messages on the console
    #[arg(long)]
    pub quiet: bool,
    /// Capture the payload's stdout and stderr into DIR/stdout and
    /// DIR/stderr (incompatible with --tty; created when missing)
    #[arg(long)]
    pub capture: Option<std::path::PathBuf>,
    /// Ignore the cache: download the image fresh
    #[arg(long)]
    pub no_cache: bool,
}

/// Parse one `HOST:GUEST[:ro]` volume specification. The guest path
/// must be absolute; the host side is checked for existence at run time.
pub fn parse_volume(spec: &str) -> anyhow::Result<ecrou::Share> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, guest, ro) = match parts.as_slice() {
        [h, g] => (*h, *g, false),
        [h, g, "ro"] => (*h, *g, true),
        [_, _, other] => {
            anyhow::bail!("share suffix must be 'ro', got ':{}' in {:?}", other, spec)
        }
        _ => anyhow::bail!("volume must be HOST:GUEST[:ro], got {:?}", spec),
    };
    if host.is_empty() || guest.is_empty() {
        anyhow::bail!("volume must be HOST:GUEST[:ro], got {:?}", spec);
    }
    if !guest.starts_with('/') {
        anyhow::bail!("share guest path must be absolute, got {:?}", guest);
    }
    Ok(ecrou::Share {
        host: host.into(),
        guest: guest.to_string(),
        read_only: ro,
    })
}

/// Parse one `KEY=VALUE` environment pair.
pub fn parse_env(spec: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("environment must be KEY=VALUE, got {:?}", spec))?;
    if k.is_empty() {
        anyhow::bail!("environment key is empty: {:?}", spec);
    }
    Ok((k.to_string(), v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_parse_rw_and_ro() {
        let s = parse_volume("/home/me/work:/workspace").unwrap();
        assert_eq!(s.host, std::path::Path::new("/home/me/work"));
        assert_eq!(s.guest, "/workspace");
        assert!(!s.read_only);

        let s = parse_volume("/skills:/skills:ro").unwrap();
        assert!(s.read_only);
    }

    #[test]
    fn volumes_reject_malformed_specs() {
        assert!(parse_volume("/only-host").is_err());
        assert!(parse_volume(":/guest").is_err());
        assert!(parse_volume("/host:relative").is_err());
        assert!(parse_volume("/h:/g:rw").is_err());
        assert!(parse_volume("/h:/g:ro:extra").is_err());
        let err = format!("{:#}", parse_volume("/only-host").unwrap_err());
        assert!(err.contains("volume"), "{err}");
    }

    #[test]
    fn it_combines_into_interactive_and_tty() {
        let cli =
            Cli::try_parse_from(["ecrou", "run", "-it", "alpine", "--", "sh"]).expect("-it parses");
        let Command::Run(args) = cli.command else {
            panic!("run expected");
        };
        assert!(args.interactive);
        assert!(args.tty);
        assert_eq!(args.image, "alpine");
    }

    #[test]
    fn v_is_the_volume_short_at_the_run_level() {
        let cli = Cli::try_parse_from([
            "ecrou",
            "run",
            "-v",
            "/work:/work:ro",
            "-v",
            "/data:/data",
            "alpine",
        ])
        .expect("-v parses");
        let Command::Run(args) = cli.command else {
            panic!("run expected");
        };
        assert_eq!(args.volumes.len(), 2);
        assert!(parse_volume(&args.volumes[0]).unwrap().read_only);
        // Verbose stays reachable by its long form.
        let cli = Cli::try_parse_from(["ecrou", "--verbose", "probe"]).expect("--verbose parses");
        assert!(cli.verbose);
        assert!(
            Cli::try_parse_from(["ecrou", "-v", "probe"]).is_err(),
            "-v belongs to run --volume, not the global verbose"
        );
    }

    #[test]
    fn w_aliases_workdir() {
        let cli = Cli::try_parse_from(["ecrou", "run", "-w", "/src", "alpine"]).expect("-w parses");
        let Command::Run(args) = cli.command else {
            panic!("run expected");
        };
        assert_eq!(args.workdir, "/src");
    }

    #[test]
    fn env_pairs_split_on_the_first_equals() {
        let (k, v) = parse_env("KEY=value=with=equals").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "value=with=equals");
        assert!(parse_env("NOEQUALS").is_err());
        assert!(parse_env("=novalue").is_err());
    }
}
