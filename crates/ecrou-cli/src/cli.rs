//! Flag definitions and the small parsing helpers they need.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ecrou",
    version,
    about = "One-shot microVM environments on libkrun",
    after_help = "Run 'ecrou run IMAGE -- COMMAND' to boot a machine for one command."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Print diagnostic messages
    #[arg(short, long, global = true)]
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
        /// Distribution name or OCI image reference
        image: String,
        /// Ignore the cache and download a fresh image
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
    /// Distribution name or OCI image reference
    pub image: String,
    /// Command to run in the machine; the image's default shell when
    /// omitted. An argument vector after `--` is taken literally; a
    /// single quoted line runs through the image's shell.
    #[arg(last = true)]
    pub command: Vec<String>,
    /// Directory to share into the machine as HOST:GUEST[:ro] (repeatable)
    #[arg(long = "share")]
    pub shares: Vec<String>,
    /// Guest memory size
    #[arg(short = 'm', long, default_value = "1G")]
    pub memory: String,
    /// Guest working directory
    #[arg(long, default_value = "/")]
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
}

/// Parse one `HOST:GUEST[:ro]` share specification. The guest path must
/// be absolute; the host side is checked for existence at run time.
pub fn parse_share(spec: &str) -> anyhow::Result<ecrou::Share> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, guest, ro) = match parts.as_slice() {
        [h, g] => (*h, *g, false),
        [h, g, "ro"] => (*h, *g, true),
        [_, _, other] => {
            anyhow::bail!("share suffix must be 'ro', got ':{}' in {:?}", other, spec)
        }
        _ => anyhow::bail!("share must be HOST:GUEST[:ro], got {:?}", spec),
    };
    if host.is_empty() || guest.is_empty() {
        anyhow::bail!("share must be HOST:GUEST[:ro], got {:?}", spec);
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
    fn shares_parse_rw_and_ro() {
        let s = parse_share("/home/me/work:/workspace").unwrap();
        assert_eq!(s.host, std::path::Path::new("/home/me/work"));
        assert_eq!(s.guest, "/workspace");
        assert!(!s.read_only);

        let s = parse_share("/skills:/skills:ro").unwrap();
        assert!(s.read_only);
    }

    #[test]
    fn shares_reject_malformed_specs() {
        assert!(parse_share("/only-host").is_err());
        assert!(parse_share(":/guest").is_err());
        assert!(parse_share("/host:relative").is_err());
        assert!(parse_share("/h:/g:rw").is_err());
        assert!(parse_share("/h:/g:ro:extra").is_err());
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
