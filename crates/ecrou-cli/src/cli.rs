//! Flag definitions and the small parsing helpers they need.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ecrou",
    version,
    about = "One-shot microVM environments on libkrun",
    after_help = "Examples:\n  ecrou run -it alpine                    a shell in the machine\n  ecrou run ./rootfs.tar.gz -- uname -a   boot a local rootfs archive\n  ecrou run -v $PWD:/workspace ubuntu -- make"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Print diagnostic messages
    #[arg(long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a command in a fresh microVM from an image
    Run(RunArgs),
    /// Bake a command into an image's cache entry: the command runs in
    /// a machine over a freshly prepared rootfs, and the result is
    /// persisted, so every later boot of the image carries what the
    /// command installed. Cached entries are reused as-is; --no-cache
    /// re-downloads and re-provisions.
    Provision {
        /// Distribution name or OCI image reference; the spec's `base`
        /// when omitted and the spec carries one
        image: Option<String>,
        /// A provisioning spec file (YAML: base, packages, run) — the
        /// layer it describes is keyed by the file's content, so the
        /// same spec never provisions twice and a changed spec never
        /// reuses an old layer
        #[arg(long)]
        spec: Option<std::path::PathBuf>,
        /// Name the provisioned layer, so runs can boot it by name
        /// (`run --layer NAME`) without re-deriving anything
        #[arg(long)]
        layer: Option<String>,
        /// Ignore the cache: re-download the base and re-provision
        #[arg(long)]
        no_cache: bool,
        /// The provisioning command. An argument vector after `--` is
        /// taken literally; a single quoted line runs through the
        /// image's shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Report what a run would find on this host
    Probe,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Distribution name, OCI image reference, or a local rootfs
    /// archive (a path: ./rootfs.tar.gz, /tmp/rootfs.tar); not needed
    /// with `--layer`, which boots a provisioned cache entry
    #[arg(required_unless_present = "layer")]
    pub image: Option<String>,
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
    /// single quoted line runs through the image's shell.
    #[arg(last = true)]
    pub command: Vec<String>,
    /// Directory to share into the machine as HOST:GUEST[:ro] (repeatable)
    #[arg(short = 'v', long = "volume")]
    pub volumes: Vec<String>,
    /// Guest memory size
    #[arg(short = 'm', long, default_value = "1G")]
    pub memory: String,
    /// Guest working directory
    #[arg(short = 'w', long, default_value = "/")]
    pub workdir: String,
    /// Guest environment KEY=VALUE (repeatable)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Suppress console noise
    #[arg(long)]
    pub quiet: bool,
    /// Capture the payload's stdout and stderr into files in this
    /// directory: DIR/stdout and DIR/stderr. Created when missing.
    #[arg(long)]
    pub capture: Option<std::path::PathBuf>,
    /// Ignore the cache and download a fresh image
    #[arg(long)]
    pub no_cache: bool,
    /// Boot a named provisioned layer (see `provision --layer`)
    /// instead of resolving an image
    #[arg(long)]
    pub layer: Option<String>,
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
        assert_eq!(args.image.as_deref(), Some("alpine"));
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
