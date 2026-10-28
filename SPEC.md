# ecrou - implementation specification

## Identity

ecrou is a microVM runtime for disposable Linux environments. It descends
from ecr, the ephemeral chroot: the name keeps the ancestor, and the
answer to "chroot ou VM" is now VM. As a bonus, an écrou is a nut: the
small part that keeps a bolt from walking out of the wall.

One sentence: resolve an image reference, boot it as a one-shot microVM
on libkrun, run one command with live stdio, propagate its exit code,
keep whatever crossed the workspace share, throw the machine away.

The project is library-first like ecr: `crates/ecrou` is the library
(package `ecrou`), `crates/ecrou-cli` is the thin front-end (binary
`ecrou`) that parses flags, maps them onto the library API, and
propagates exit codes. The split is enforced by the dependency edges:
the library links no clap and no rendering crate. It never writes to
the terminal: diagnostics go through the `log` facade, filtered by
`RunOptions::log_level` (an engine-owned enum, not UI booleans), long
operations report through the `Progress` sink, all output is the
CLI's. In-process failures come back as typed errors; the one
exception is the forked machine child's last-gasp diagnostics, which
land in the console file the parent streams. The CLI owns clap, the
indicatif rendering of progress events, the log sink, and exit codes.
A consumer embeds the library exactly the way the CLI does: a
`RootfsCache` to prepare a rootfs, then `run` to boot it.

The first consumer is rhema's sandbox seam, which shells out to the
binary the way it shells out to `docker` and `lxc` today. The library
API is designed so a second consumer can arrive without forking the
runtime.

## What carries over from ecr

| module | disposition |
|---|---|
| `distro`, `download`, `extract`, `rootfs`, `utils`, `progress` | ported; the image machinery is confinement-neutral plain fs and network |
| `kernel` | dropped; the kernel is libkrun's, bundled in libkrunfw |
| `qemu_vm` | dropped from v1; its KVM probe logic (open `/dev/kvm`, `KVM_GET_API_VERSION` == 12) moves into the `krun` module |
| `namespace`, `chroot`, `mount`, `exec` | dropped; they stay in ecr, their ancestor. Their error variants (`UserNamespacesUnavailable`, `BinfmtNotRegistered`, `MissingSubidRange`) leave with them |

No initramfs and no cpio builder: the rootfs crosses as a virtiofs
share, never as a packed archive.

No cache compatibility with ecr: the formats may match but the directory
is `~/.cache/ecrou` and never shared.

## The engine: libkrun

v1 has exactly one engine, libkrun, driven through its stable C ABI
(the `stable-1.19.x` headers; the main branch is mid-rewrite to a
vtable API and is not the target). ecrou `dlopen`s `libkrun.so.1` at
runtime and fails with a remediation message naming the install when it
is missing; nothing links at build time, and a snap bundles the library
plus libkrunfw next to itself.

The VMM runs inside the ecrou process: `krun_start_enter` enters the
VM loop on the calling thread. There is no child process to shepherd,
which makes the kill contract below trivial.

The FFI lives in `crates/ecrou/src/krun.rs`. ecrou carries unsafe libc
code; rhema, its first consumer, keeps `#![forbid(unsafe_code)]` and
drives the CLI.

The v1 API surface is eleven calls: `krun_set_log_level`,
`krun_create_ctx`, `krun_set_vm_config`, `krun_set_root`,
`krun_add_virtiofs`, `krun_set_workdir`, `krun_set_exec`,
`krun_set_env`, `krun_set_console_output`, `krun_start_enter`,
`krun_check_nested_virt`. `krun_set_mapped_volumes` exists but is
explicitly unsupported on Linux (its header says to use user namespaces
and bind-mounts instead, which is precisely what ecrou cannot do); the
workspace is a tagged `krun_add_virtiofs` share mounted by the guest
init instead.

KVM is required. There is no TCG fallback; a machine without working
KVM is an `Error::KvmUnavailable` with the remediation text, and
`ecrou probe` reports it before any run. QEMU as a fallback engine is a
Later row, and its arrival would not change the API.

## The founding rule

ecrou's host process never mounts a filesystem and never creates or
enters a namespace. Every boundary crossing is a file or a socket. This
is the property that lets ecrou run inside a strict snap's default-deny
seccomp allowlist, where every namespace-based runtime, runc, crun,
podman, and ecr's own chroot mode, dies at `mount`.

The rule is enforced by audit, not by trust: test T6 straces a full run
and fails on a single hit from the denied set. It runs in CI.

Guest-side mounts are not host mounts. Confinement binds the host
process; the guest kernel may mount proc, devtmpfs, and the workspace
share inside the machine, and nothing a guest does lands on the host
beyond what the shares expose.

### Confinement budget

Everything the host process may do, and nothing else:

- fs: read, write, stat, rename, unlink, mkdir, fallocate under the
  cache directory and the share roots it was given, plus the scratch
  rootfs TempDir
- exec: itself; libraries via `dlopen` (`libkrun.so.1`, libkrunfw)
- `/dev/kvm`: open, ioctl, mmap
- net: TCP and UDP client sockets; with TSI these carry the guest's
  connections too, proxied host-side
- process: threads, futexes, eventfds, timers; no children in v1

Denied by design: `mount`, `umount2`, `unshare`, `setns`, `pivot_root`,
`open_tree`, `move_mount`, `mount_setattr`, `fsopen`, `fsmount`,
`ptrace`, kernel module loading, keyring calls. All absent from snapd's
seccomp allowlist, which is where the allowed list above is drawn from.
Two absences need a spike-time check because VMMs plausibly touch them:
`sched_setaffinity` and `openat2`. The spike straces for EPERM on
allowed-looking calls, and anything libkrun genuinely needs that the
allowlist denies goes on a snapd issue, never into a workaround.

## Execution model

One-shot machines. `ecrou run IMAGE -- CMD` prepares a rootfs, boots
one VM whose payload is CMD, streams its stdio, and exits with CMD's
code when the machine powers off. There is no exec into a running
machine. No engine in the family has that API: `krun_set_exec` is
per-context before start, and crun's krun handler boots a fresh VM for
every `crun exec`. A guest agent and long-lived machines are a Later
row; per-command boots are the v1 shape, and the boot budget below is
what keeps them honest.

Ephemeral rootfs, durable workspace. The base system is a scratch copy
of the image (a `PreparedRootfs`, TempDir-backed): it exists only for
the run and dies with it. Persistence crosses a workspace share or it
does not happen.

**Workspace shares.** `--share HOST:GUEST[:ro]`, one virtiofs share per
entry (`krun_add_virtiofs`, `krun_set_root` keeps the rootfs itself).
The host side is plain fs; the guest init mounts each share at its
guest path. There is no implicit share: an empty sandbox is the
default, and the consumer chooses what crosses. This is deliberate; a
sandbox runtime does not silently attach the caller's cwd.

Ownership needs no mapping layer. The guest runs as root inside its own
kernel, and libkrun's simplified virtiofs semantics report the
requesting guest uid for ownership while storing permission bits
host-side; writes through a share land owned by the host user. The
rootless subuid problem the podman shape has does not exist here.

**The guest init.** A small script provisioned into the scratch rootfs
before boot, for every run: it mounts each share, cds to the workdir,
runs the payload, and writes the exit code to the control file. The
hostname and workdir ride `krun_set_exec`'s config natively (the init
blob applies both); the script's own hostname and cd lines stay as
fallbacks. There is no exec (the init must outlive the payload to
report it) and no share-less special case: the same contract covers a
bare shell run. The payload itself is an argv vector taken
literally; the one rewrite is a single argument containing whitespace,
which is a quoted shell line ("apk add x && y") and runs through the
image's shell as `sh -c`. Ending the machine is not the script's job
either:
when the script exits, the init blob reports the code and the VMM
powers the machine off (the libkrunfw kernel has no sysrq).

**Exit codes.** On a normal payload exit the VMM terminates its own
process with the blob-reported code, so `krun_start_enter` never
returns: run() forks, the child enters the VM loop, and the parent
reaps it and reads the code from the exit status. Signal deaths map to
128+signal. The control file remains the diagnostic when the boot dies
without the init reporting. The mechanism is one hop shorter than
designed here, and the contract for consumers is unchanged.

**Cancellation is a kill.** The VMM lives in a child of the ecrou
process carrying PR_SET_PDEATHSIG, so a killed consumer kills the
machine with it. No orphan survives. This is stronger than `docker
exec`'s cancel, which kills only the local client; the tests keep it
true (T4).

**Console.** Guest output reaches real stdout through a console file
the parent streams while the machine runs, with a final drain after
the reap. The direct stdio wiring does not survive contact with
reality: with a tty on fd 0 libkrun's terminal setup stops a
background caller with SIGTTOU, and without one its console bytes go
to the log sink, not the terminal. The child therefore unwires fd 0 to
/dev/null (v1 carries no guest input; a Ctrl-C to the process group is
still forwarded into the guest). There is no QEMU monitor to mux;
`--verbose` maps to `krun_set_log_level`.

**Networking.** TSI, libkrun's default: the guest has no interface, and
its socket calls are proxied on the host's stack by the VMM process.
ecrou pins it explicitly (a vsock with AF_INET hijacked) instead of
leaving the heuristics mode to sniff. The guest resolver is the host's
uplink config with loopback nameservers stripped (loopback never
crosses the proxy; verified by hand). DNS is proxied with the rest.
Known upstream limitation: per-stream throughput in this generation of
the ln driver is tens of KB/s against a CDN, TLS included; workable,
not fast, and worth re-measuring on every engine update. Port
publishing (`krun_set_port_map`) is how a later
sandboxd-in-guest serves the host. No tap, no bridges, ever: they need
host setup the budget cannot afford.

**Memory and CPUs.** `krun_set_vm_config`; default 1G and
all-logical-cpus, `-m` to override. A per-command machine is
provisioned per command, and agent consumers run several concurrently.

## Workspace layout

Cargo workspace, two crates, the ecr split:

- `crates/ecrou` - the library. Modules: `config`, `distro`,
  `download`, `error`, `extract`, `guest` (the init script), `krun`
  (the FFI), `progress`, `rootfs`, `utils`, `vm`.
- `crates/ecrou-cli` - the CLI (binary `ecrou`).

The library must not depend on CLI types; `clap` is a CLI-only
dependency. Everything else from ecr's rules stands: anyhow internal,
`ecrou::Error` semantic variants at the sites that detect them, `log`
for narration, `Progress` for long operations.

## Library API

### Rootfs lifecycle (ported)

`RootfsCache::new(dir)` / `default_dir()`, `prepare`,
`prepare_provisioned`, `persist`, `PrepareRequest`, `PreparedRootfs`:
ecr's semantics verbatim, including digest freshness on `:latest` and
the `.provisioned` hot-cell marker. The per-run scratch copy is
`PreparedRootfs` as ecr already builds it. See ecr's SPEC for the
reference description; differences are the cache directory and the
absent kernel entries only.

### `vm::run`

```rust
pub struct Share { pub host: PathBuf, pub guest: String, pub read_only: bool }
pub struct RunOptions {
    pub image: String,          // distro ref or OCI image ref
    pub arch: String,           // empty = host arch
    pub memory: String,         // default "1G"
    pub workspace: Option<Share>,   // the one rw share consumers name
    pub shares: Vec<Share>,     // additional named shares, ro allowed
    pub command: Vec<String>,   // empty = the image's default shell
    pub workdir: String,        // guest cwd; empty = /
    pub env: Vec<(String, String)>, // composed envp; host env never inherited
    pub hostname: String,       // empty = generated ecrou-vm-XXXXXX
    pub log_level: LogLevel,    // Quiet | Normal | Verbose
    pub capture: Option<PathBuf>,   // payload stdout/stderr into files
}
pub fn run(options: &RunOptions, progress: &dyn Progress) -> Result<i32>
```

Guest output reaches real stdout through the streamed console; the
return value is the guest exit code. A run
without working KVM is `Error::KvmUnavailable` with the remediation
text. Everything else is `Error::Other` over the anyhow chain, ecr's
shape. There is no long-lived machine; consumers pump the child's
stdio themselves when they drive the CLI.

**Capture.** The console is one stream, kernel and guest mixed, and a
consumer that needs the payload's stdout and stderr apart - rhema's
sandbox seam reports them as separate fields - cannot read it. With
`capture` set, the run adds the directory as an internal rw share
(guest path `/.ecrou-io`, reserved), and the init wraps the payload in
a redirection group pointing at it: the two streams land host-side as
`<dir>/stdout` and `<dir>/stderr`, written live, owned by the host
user, and simply there when `run` returns. The console stays on stdout
for diagnostics, which is what makes capture and a nulled stdout
composable. A boot failure leaves the files absent.

## CLI

```
ecrou run [OPTIONS] <IMAGE> [-- COMMAND...]
ecrou provision [OPTIONS] <IMAGE> [-- COMMAND...]
ecrou probe
```

| Flag | Default | Description |
|------|---------|-------------|
| `--share <HOST:GUEST[:ro]>` | none | Workspace share; repeatable |
| `-m, --memory <size>` | 1G | Guest memory |
| `--workdir <guest path>` | `/` | Guest cwd |
| `-e, --env <KEY=VALUE>` | none | Guest env; repeatable |
| `--quiet` | false | Suppress kernel log on the console |
| `--capture <DIR>` | none | Payload stdout/stderr into `<DIR>/stdout` and `<DIR>/stderr` |
| `--no-cache` | false | Fresh download, ignore the cache |
| `-v, --verbose` | false | Diagnostics; raises the libkrun log level |

`ecrou provision IMAGE -- COMMAND` bakes a command into the image's
cache entry: the command runs in a machine over a freshly prepared
rootfs and the result is persisted, so every later boot of that image
carries what the command installed — packages, users, whatever a
one-shot boot cannot keep. A marked entry is reused as-is (the
command is skipped), `--no-cache` re-downloads and re-provisions, and
a failing command leaves the entry unprovisioned for the next attempt.
The marker dies with the entry: a digest refresh drops it, so a moved
upstream image is provisioned again, never silently served pristine
under an old marker.

`ecrou probe` reports what a run would find: KVM working (open, ioctl
version, nested-virt state), `libkrun.so.1` loadable, the cache
directory writable. It is the confinement diagnostic and the support
tool.

## File layout

```
~/.cache/ecrou/
├── ubuntu-noble-amd64.tar.gz
├── alpine-edge-x86_64.tar.gz
├── <...>.digest                        # manifest digest sidecar
└── <...>.provisioned                   # hot-cell marker
```

`~/.config/ecrou.yaml` carries over (DNS overrides today).

## Snap packaging

Confinement: strict. Plugs: `network`, `home`, `kvm`. Manual
`snap connect` for kvm is accepted for now; auto-connect is a store
request and a later product decision.

The snap stages `libkrun.so.1` and libkrunfw next to the binary; at
roughly 6 MB combined they cost less than any qemu-system package, and
the kernel ships inside libkrunfw, so there is nothing else to fetch or
stage.

Where the libraries come from, as of now: no Ubuntu release publishes
a `libkrun.so.1` package at all, and libkrunfw5 exists only in the
current development series' universe, not in any LTS the snap bases
ship from. Debian sid has both (source package `libkrun`), so the gap
is sync lag and it should close by the next LTS. Until then the snap
carries both halves itself: a part that builds libkrun from source
(the recipe is small: rust, clang for the C shim, `make build-release`
with the net feature for TSI) and stages `libkrun.so.1`; and a part
that fetches the `libkrunfw5` .deb from the Ubuntu pool and unpacks it
into the staging tree. snapcraft puts staged libraries on
`LD_LIBRARY_PATH` at runtime, which is exactly the resolution path the
`dlopen` in krun.rs takes; there is no ldconfig and no host dependency.
When an LTS base archive ships both packages, both parts shrink to
`stage-packages` entries and the from-source build is deleted.

The first milestone is the spike snap, before any porting: a minimal
libkrun binary behind strict confinement running a VM. T1 is the gate
that proves the shape under real AppArmor and seccomp, and everything
else in this document is design until it passes.

## Falsification tests

- **T1, confinement.** The spike snap runs `ecrou run alpine -- uname
  -a` under strict confinement. The guest kernel version differing from
  the host is the microVM proof, the same check crun's krun man page
  prescribes. The same run straces for EPERM on plausible VMM calls
  (`sched_setaffinity`, `openat2`) and reports findings to the snapd
  issue list.
- **T2, workspace coherence.** In a shared workspace, the guest runs
  git init, add, commit, status; the host sees the commits; a second
  machine sees the first's writes. Git's mmap'd index is the trap this
  test exists for, and libkrun's virtiofs DAX windows are exactly where
  it bites.
- **T3, exit codes.** `ecrou run alpine -- sh -c "exit 7"` exits 7.
  A guest signal death exits 128+signal. A failed VMM reports an
  engine failure, never a guest code.
- **T4, kill chain.** SIGKILL ecrou-cli mid-run; the VM is gone with
  the process, no stray threads or KVM VMs, verified by scanning the
  process table and `/dev/kvm` users.
- **T5, boot economics.** Alpine, cache-warm: extract plus boot lands
  under 1.5 s to first guest output. Larger images report their numbers
  unblocked; the shared-ro-rootfs row in Later is the fix if they hurt.
- **T6, no-mount audit.** `strace -f -e trace=mount,umount2,unshare,
  setns,pivot_root,open_tree,move_mount,fsopen,fsmount` over a full run
  with shares: zero hits. This test is the founding rule.
- **T7, guest network.** An https fetch from the guest succeeds, DNS
  included; a published port is reachable from the host.

## Later

- **QEMU engine.** The TCG fallback for machines without KVM. The
  `RunOptions` API does not change; the engine choice is the probe's.
- **Shared read-only rootfs.** Serve the cache rootfs directly, ro,
  with a guest-side overlay for writes: no extraction per run. The
  correct-and-slow v1 posture graduates into this.
- **Guest agent, long-lived machines.** Exec without a boot; the shape
  to grow into when per-command VMs stop being right.
- **passt** (`krun_set_passt_fd`) if TSI throughput or UDP listening
  ever matters.
- **sandboxd-in-guest** over `krun_set_port_map`: rhema's http backend
  pointed into a machine.
- **Rootfs persistence** through `prepare_provisioned` hot cells,
  already in the ported code.

## Open questions

- Does `krun_start_enter` return the guest exit code natively on
  poweroff? The control file works regardless; if the return value
  carries it, the mechanism shrinks to a read.
- Console wiring: does the default virtio-console land on the VMM's
  stdio, and is stdin carried? Decided by a first-run experiment, with
  `krun_set_console_output` as the fallback.
- TSI DNS: which resolver file the guest needs, and whether lookups
  just work. T7 decides.
- dlopen versus link-time linking for the snap; dlopen is the default
  for the remediation error it enables.
- Which distros package libkrun (Fedora does; Debian and Ubuntu do not
  yet) and what the dev-install story says meanwhile.
