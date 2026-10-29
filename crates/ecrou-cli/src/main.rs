//! The ecrou CLI: flags onto the library API, exit codes out. All
//! terminal output lives here; the library stays silent behind `log`
//! and the progress sink.

mod cli;
mod logger;
mod progress;

use clap::Parser;
use ecrou::vm::RunOptions;
use ecrou::{LogLevel, NoProgress, Progress};

use cli::{parse_env, parse_volume, Cli, Command, RunArgs};
use progress::TermProgress;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    logger::init(cli.verbose);
    match cli.command {
        Command::Run(args) => cmd_run(&args, cli.verbose),
        Command::Provision {
            image,
            spec,
            layer,
            no_cache,
            command,
        } => cmd_provision(image, spec, layer, no_cache, &command, cli.verbose),
        Command::Probe => cmd_probe(),
    }
}

fn cmd_provision(
    image: Option<String>,
    spec: Option<std::path::PathBuf>,
    layer: Option<String>,
    no_cache: bool,
    command: &[String],
    cli_verbose: bool,
) -> std::process::ExitCode {
    use ecrou::cache::RootfsCache;
    use ecrou::rootfs::PrepareRequest;
    use ecrou::spec::ProvisionSpec;

    // Two shapes: `provision IMAGE -- COMMAND` bakes a one-off
    // command; `provision --spec FILE --layer NAME` bakes a checked-in
    // spec into a named, content-keyed layer. A spec plus a command
    // has no single meaning.
    if spec.is_none() == command.is_empty() {
        return fail(&anyhow::anyhow!(
            "provision needs either a spec file (--spec) or a command: \
             ecrou provision <IMAGE> -- <COMMAND...>"
        ));
    }

    let level = if cli_verbose {
        LogLevel::Verbose
    } else {
        LogLevel::Normal
    };
    let mut options = RunOptions {
        log_level: level,
        // The machine exists to change the rootfs, not to cross a
        // share: the workspace would ride the persist into the cache.
        ..RunOptions::default()
    };
    options.workdir = "/".to_owned();

    let progress: &dyn Progress = &TermProgress::new();
    let dir = match RootfsCache::default_dir() {
        Ok(dir) => dir,
        Err(err) => return fail(&anyhow::Error::new(err)),
    };
    let cache = RootfsCache::new(dir);

    // The spec decides the request: its base (or the positional
    // image), its key, and its script — the packages through the
    // base's package manager, then the run block. The key hashes the
    // raw bytes and the base reference, so the cache entries two
    // projects see can never cross.
    let mut request = PrepareRequest::new(String::new());
    request.no_cache = no_cache;
    let mut script;
    match &spec {
        Some(path) => {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) => return fail(&anyhow::anyhow!("reading {}: {err}", path.display())),
            };
            let parsed = match ProvisionSpec::parse(&bytes) {
                Ok(parsed) => parsed,
                Err(err) => return fail(&err),
            };
            let base = match parsed.base.clone().or_else(|| image.clone()) {
                Some(base) => base,
                None => {
                    return fail(&anyhow::anyhow!(
                        "the spec has no base and no image was named: \
                         ecrou provision <IMAGE> --spec {path}",
                        path = path.display()
                    ))
                }
            };
            script = parsed.script();
            let key = ProvisionSpec::key(&bytes, &base);
            request.image = base;
            request.layer_key = Some(key);
        }
        None => {
            if layer.is_some() {
                return fail(&anyhow::anyhow!(
                    "--layer names a provisioned spec, not a bare command"
                ));
            }
            request.image = image
                .clone()
                .expect("clap requires an image without --spec");
            script = command.join(" ");
            script.push('\n');
        }
    }
    options.image = request.image.clone();
    options.command = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];

    let outcome = cache.prepare_provisioned(&request, progress, |rootfs| {
        let code = ecrou::vm::run_prepared(rootfs, &options)?;
        if code != 0 {
            anyhow::bail!(
                "the provisioning command exited {code}; the cache entry was left unprovisioned"
            );
        }
        Ok(())
    });
    match outcome {
        Ok(_rootfs) => {
            // The pointer lands only after the entry does: a name
            // that resolves is a layer that boots.
            if let (Some(name), Some(key)) = (&layer, &request.layer_key) {
                let path = match cache.resolved_entry_path(
                    &request.image,
                    &request.resolved_arch(),
                    Some(key),
                ) {
                    Ok(path) => path,
                    Err(err) => return fail(&anyhow::Error::new(err)),
                };
                let filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Err(err) = cache.write_layer_pointer(name, &filename) {
                    return fail(&anyhow::Error::new(err));
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(err) => fail(&anyhow::Error::new(err)),
    }
}

fn cmd_run(args: &RunArgs, cli_verbose: bool) -> std::process::ExitCode {
    // A tty drives the payload's output through the terminal while the
    // capture share points the same streams at files: one of them has
    // to lose, so the run refuses instead of picking.
    if args.tty && args.capture.is_some() {
        return fail(&anyhow::anyhow!(
            "--capture and --tty cannot be combined: the payload's output \
             cannot land in the capture files and on the terminal"
        ));
    }
    let mut options = RunOptions {
        // Without a layer the image is required by clap; with one the
        // field is dead — the layer entry carries the rootfs.
        image: args.image.clone().unwrap_or_default(),
        command: args.command.clone(),
        memory: args.memory.clone(),
        workdir: args.workdir.clone(),
        log_level: if args.quiet {
            LogLevel::Quiet
        } else if cli_verbose {
            LogLevel::Verbose
        } else {
            LogLevel::Normal
        },
        interactive: args.interactive,
        tty: args.tty,
        no_cache: args.no_cache,
        layer: args.layer.clone(),
        capture: args.capture.clone(),
        ..RunOptions::default()
    };
    for spec in &args.env {
        match parse_env(spec) {
            Ok((k, v)) => options.env.push((k, v)),
            Err(err) => return fail(&err),
        }
    }
    let mut shares = Vec::new();
    for spec in &args.volumes {
        match parse_volume(spec) {
            Ok(share) => shares.push(share),
            Err(err) => return fail(&err),
        }
    }
    // The first read-write share is the workspace (its guest path rides
    // into the guest as ECROU_WORKSPACE); everything else is a plain
    // share.
    if let Some(pos) = shares.iter().position(|s| !s.read_only) {
        options.workspace = Some(shares.remove(pos));
    }
    options.shares = shares;

    let progress: &dyn Progress = if args.quiet {
        &NoProgress
    } else {
        &TermProgress::new()
    };
    match ecrou::vm::run(&options, progress) {
        Ok(code) => std::process::ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => fail(&anyhow::Error::new(err)),
    }
}

fn cmd_probe() -> std::process::ExitCode {
    use ecrou::krun;

    let mut ok = true;

    print!("kvm:          ");
    match krun::kvm_probe() {
        Ok(()) => println!("ok"),
        Err(err) => {
            ok = false;
            println!("unavailable\n  {:#}", err);
        }
    }

    print!("libkrun:      ");
    match krun::Krun::load() {
        Ok(krun) => {
            println!("ok");
            print!("nested virt:  ");
            match krun.check_nested_virt() {
                Ok(true) => println!("available"),
                Ok(false) => println!("not available (only matters for nested runs)"),
                Err(err) => println!("unknown ({err:#})"),
            }
        }
        Err(err) => {
            ok = false;
            println!("not loadable\n  {:#}", err);
        }
    }

    print!("cache:        ");
    match ecrou::cache::RootfsCache::default_dir() {
        Ok(dir) => {
            // The run flow creates the cache on demand; probe mirrors it.
            if let Err(err) = std::fs::create_dir_all(&dir) {
                ok = false;
                println!("{} not creatable ({})", dir.display(), err);
                return probe_done(ok);
            }
            let probe = dir.join(".ecrou-probe");
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    println!("{} (writable)", dir.display());
                }
                Err(err) => {
                    ok = false;
                    println!("{} not writable ({})", dir.display(), err);
                }
            }
        }
        Err(err) => {
            ok = false;
            println!("unavailable ({err:#})");
        }
    }

    probe_done(ok)
}

fn probe_done(ok: bool) -> std::process::ExitCode {
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn fail(err: &anyhow::Error) -> std::process::ExitCode {
    eprintln!("Error: {err:#}");
    std::process::ExitCode::FAILURE
}

/// T6, the founding rule as a test: strace a full run and fail on any
/// denied syscall. Needs the built binary, libkrun, and strace, so it
/// runs in CI (`cargo test -- --ignored`).
#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "needs the ecrou binary, libkrun, and strace; runs in CI"]
    fn t6_no_mount_or_namespace_syscalls() {
        let denied = "mount,umount2,unshare,setns,pivot_root,open_tree,move_mount,fsopen,fsmount";
        // CARGO_BIN_EXE_* exists only for integration tests, which the
        // conventions forbid; CI points ECROU_BIN at the built binary.
        let exe = std::env::var("ECROU_BIN")
            .expect("T6 needs ECROU_BIN pointing at the built ecrou binary");
        let trace_path = std::env::temp_dir().join("ecrou-t6.strace");
        let out = std::process::Command::new("strace")
            .args([
                "-f",
                "-e",
                &format!("trace={denied}"),
                "-o",
                trace_path.to_str().unwrap(),
                &exe,
                "run",
                "alpine:3.23",
                "--",
                "true",
            ])
            .status()
            .expect("strace run");
        assert!(out.success(), "the traced run failed; see the strace log");
        let trace = std::fs::read_to_string(&trace_path).unwrap();
        let _ = std::fs::remove_file(&trace_path);
        for line in trace.lines() {
            let denied_call = denied.split(',').any(|syscall| line.contains(syscall));
            let failed = line.contains("= -1 EPERM") || line.contains("= -1 ENOSYS");
            assert!(
                !(denied_call && failed),
                "denied syscall in the trace: {line}"
            );
        }
    }
}
