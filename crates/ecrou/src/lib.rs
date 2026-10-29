//! ecrou - one-shot microVM environments on libkrun.
//!
//! This crate is the library behind the `ecrou` CLI. It resolves distro
//! and OCI image references, downloads them through a content cache,
//! extracts the rootfs into a scratch directory, and boots it as a
//! one-shot microVM: one command runs as the machine's only workload,
//! its stdio streams through, its exit code comes back, and the machine
//! is thrown away. The rootfs crosses as a virtiofs share, the workspace
//! as explicit shares the consumer names.
//!
//! The founding rule: the host process never mounts a filesystem and
//! never enters a namespace. Every boundary crossing is a file or a
//! socket, which is what lets ecrou run inside a strict snap's seccomp
//! allowlist. See SPEC.md.
//!
//! The main entry points are [`rootfs`] (cache-aware rootfs preparation
//! and persistence) and [`vm`] (running a command inside a prepared
//! rootfs). Public functions return [`Result`] carrying the [`Error`]
//! enum, so consumers can match on host-readiness failures
//! ([`Error::KvmUnavailable`], [`Error::ArchNotAvailable`]) versus
//! ordinary failures ([`Error::Other`], which preserves the full context
//! chain).
//!
//! ## Crate split
//!
//! `ecrou` is the library and carries no UI: progress flows through
//! the [`Progress`] sink, engine logging through the `log` facade
//! filtered by [`LogLevel`], and results come back as typed errors.
//! `ecrou-cli` owns everything a human sees: flags, the indicatif
//! rendering of progress events, the log sink, exit codes. A consumer
//! embeds the library exactly the way the CLI does - a [`RootfsCache`]
//! to prepare a rootfs, then [`run`] to boot it.

pub mod distro;
pub mod download;
pub mod error;
pub mod extract;
pub mod guest;
pub mod krun;
pub mod progress;
mod registry;
pub mod rootfs;
pub mod spec;
pub mod utils;
pub mod vm;

pub use error::{Error, Result};
pub use progress::{NoProgress, Progress, Unit};
pub use rootfs::{PrepareRequest, PreparedRootfs, RootfsCache};
pub use vm::{run, LogLevel, RunOptions, Share};
