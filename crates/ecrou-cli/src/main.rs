//! The ecrou CLI: flags onto the library API, exit codes out. All
//! terminal output lives here; the library stays silent behind `log`
//! and the progress sink.

mod cli;
mod logger;
mod progress;

use clap::Parser;
use ecrou::vm::RunOptions;
use ecrou::{NoProgress, Progress};

use cli::{parse_env, parse_share, Cli, Command, RunArgs};
use progress::TermProgress;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    logger::init(cli.verbose);
    match cli.command {
        Command::Run(args) => cmd_run(&args, cli.verbose),
        Command::Probe => cmd_probe(),
    }
}

fn cmd_run(args: &RunArgs, cli_verbose: bool) -> std::process::ExitCode {
    let mut options = RunOptions {
        image: args.image.clone(),
        command: args.command.clone(),
        memory: args.memory.clone(),
        workdir: args.workdir.clone(),
        quiet: args.quiet,
        verbose: cli_verbose,
        no_cache: args.no_cache,
        ..RunOptions::default()
    };
    if let Some(arch) = &args.arch {
        options.arch = arch.clone();
    }
    for spec in &args.env {
        match parse_env(spec) {
            Ok((k, v)) => options.env.push((k, v)),
            Err(err) => return fail(&err),
        }
    }
    let mut shares = Vec::new();
    for spec in &args.shares {
        match parse_share(spec) {
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
    match ecrou::rootfs::RootfsCache::default_dir() {
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
