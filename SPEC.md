# ecrou - contracts and invariants

The code is the source of truth for how ecrou works. This document
holds what the code cannot carry: the founding rule and the
confinement budget, the run contracts, the trust model, and the
falsification tests that keep them honest. Anything descriptive of the
current code lives with the code.

## What ecrou is

Resolve an image reference, boot it as a one-shot microVM on libkrun,
run one command with live stdio, propagate its exit code, keep
whatever crossed a share, throw the machine away.

The split is library-first and enforced by the dependency edges: the
library links no clap and no rendering crate, and never writes to the
terminal. Diagnostics go through the `log` facade, filtered by
`RunOptions::log_level`; long operations report through the `Progress`
sink; in-process failures come back as typed errors. The one exception
is the forked machine child's last-gasp diagnostics, which land in the
console file the parent streams. The CLI owns clap, rendering, log
output, and exit codes. A consumer embeds the library exactly the way
the CLI does: a `RootfsCache` to prepare a rootfs, then `run` to boot
it. The first consumer shells out to the binary the way it shells out
to `docker`; the API must let a second consumer arrive without forking
the runtime.

## The engine

One engine, libkrun, driven through its stable C ABI (the
`stable-1.19.x` headers; the main branch is mid-rewrite to a vtable
API and is not the target). ecrou `dlopen`s `libkrun.so.1` at runtime
and fails with a remediation message naming the install when it is
missing; nothing links at build time, and the snap bundles the library
plus libkrunfw next to itself.

KVM is required; there is no TCG fallback. A machine without working
KVM is `Error::KvmUnavailable` with the remediation text, and `ecrou
probe` reports it before any run. The VMM runs inside the machine
child, not on the caller's thread (see Cancellation).

libkrun's mapped-volumes entry point is explicitly unsupported on
Linux — its header asks for user namespaces and bind mounts, precisely
what ecrou cannot do — so persistence crosses a virtiofs share mounted
by the guest init instead.

## The founding rule

ecrou's host process never mounts a filesystem and never creates or
enters a namespace. Every boundary crossing is a file or a socket. This
is the property that lets ecrou run inside a strict snap's default-deny
seccomp allowlist, where every namespace-based runtime, runc, crun,
podman, and ecr's own chroot mode, dies at `mount`.

The rule is enforced by audit, not by trust: test T6 straces a full run
and fails on a single hit from the denied set. It runs in CI.

Guest-side mounts are not host mounts. Confinement binds the host
process; the guest kernel may mount proc, devtmpfs, and the shares
inside the machine, and nothing a guest does lands on the host beyond
what the shares expose.

### Confinement budget

Everything the host process may do, and nothing else:

- fs: read, write, stat, rename, unlink, mkdir, fallocate under the
  cache directory and the share roots it was given, plus the scratch
  directory holding the run's image copy
- exec: itself; libraries via `dlopen` (`libkrun.so.1`, libkrunfw);
  e2fsprogs helpers (`mkfs.ext4`, `debugfs`) as child processes, and
  `cp` for the per-run image copy
- `/dev/kvm`: open, ioctl, mmap
- net: TCP and UDP client sockets; with TSI these carry the guest's
  connections too, proxied host-side
- process: threads, futexes, eventfds, timers; one forked child, the
  machine, carrying PR_SET_PDEATHSIG

Denied by design: `mount`, `umount2`, `unshare`, `setns`, `pivot_root`,
`open_tree`, `move_mount`, `mount_setattr`, `fsopen`, `fsmount`,
`ptrace`, kernel module loading, keyring calls. All absent from snapd's
seccomp allowlist, which is where the allowed list above is drawn from.
Two absences need a spike-time check because VMMs plausibly touch them:
`sched_setaffinity` and `openat2` (T1). The spike straces for EPERM on
allowed-looking calls, and anything libkrun genuinely needs that the
allowlist denies goes on a snapd issue, never into a workaround.

## Execution model

One-shot machines. `ecrou run IMAGE -- CMD` prepares a rootfs, boots
one VM whose payload is CMD, streams its stdio, and exits with CMD's
code when the machine powers off. There is no exec into a running
machine: no engine in the family has that API, and per-command boots
are the shape. A guest agent and long-lived machines are a Later row.

Ephemeral rootfs, durable workspace. The base system is a scratch copy
of the image; it exists only for the run and dies with it. Persistence
crosses a share or it does not happen. The rootfs crosses as a raw
ext4 image: mke2fs builds a cache image straight from the image
tarball, the run copies it into a scratch directory, and the engine
attaches it as a virtio-blk disk the init blob mounts at `/`. No
initramfs and no cpio builder, and no extraction to a host tree —
an unprivileged extract cannot preserve a tar's owners, and the
tarball-to-image pipeline never needs one.

**Workspace shares.** `-v HOST:GUEST[:ro]`, one virtiofs share per
entry. The host side is plain fs; the guest init mounts each share at
its guest path. There is no implicit share: an empty machine is the
default, and the consumer chooses what crosses — a sandbox runtime
does not silently attach the caller's cwd.

Ownership is real inside the machine. The image bakes the ownership
the archive records — mke2fs reads uid and gid from the tarball — and
the guest runs as root on a real filesystem, so `chown` inside the
machine works, including recursive ones that a virtiofs share owned by
the host user refuses with EPERM. Shares remain the honest exception:
the host side belongs to the host user, so writes through a share land
owned by the host user and a guest chown there fails exactly as it
would on the host.

**The guest init.** A small script written into the image copy
through debugfs for every run: it mounts each share, cds to the
workdir, runs the
payload, and writes the exit code to the control file. It never execs
— the init must outlive the payload to report it — and has no
share-less special case: the same contract covers a bare shell run.
The payload is an argv vector taken literally; a single argument
containing whitespace is a quoted shell line (`apk add x && y`) and
runs through the image's shell as `sh -c`. Ending the machine is not
the script's job: when it exits, the init blob reports the code and
the VMM powers the machine off.

**Exit codes.** Only the guest init can mint a guest exit code, and
the control file is how: the init writes the payload's code before it
exits, and the run reads it back out of the image after the machine
dies. The file outranks the machine child's own exit code — with a
block root the engine's exit-code channel (an ioctl through the
virtiofs root device) never fires, so the child's code is machinery
talk, not a report. A signal on the machine child means the VMM or the
engine died — the payload never reported — and `run` fails with
an engine-failure error, never a 128+signal guess at a guest code.
125 and 126 are reserved by the machinery itself: the child bails
with 125 on a boot failure and the init refuses with 126 when a share
would not mount or the workdir is missing. Ending on either without a
control-file code is an error naming the cause. The control file
remains the boot diagnostic in every other death.

**Cancellation is a kill.** The VMM lives in a child of the ecrou
process carrying PR_SET_PDEATHSIG, so a killed consumer kills the
machine with it. No orphan survives. This is stronger than `docker
exec`'s cancel, which kills only the local client (T4).

**Console.** Three wirings, chosen by the run's flags.

Headless (the default): guest output reaches real stdout through a
console file the parent streams while the machine runs, with a final
drain after the reap. The child unwires fd 0 to `/dev/null`: with a
tty on fd 0 libkrun's terminal setup stops a background caller with
SIGTTOU, and /dev/null keeps the console plain while still forwarding
a Ctrl-C sent to the process group into the guest.

`-t`: the machine owns the terminal. The guest console rides the
process's own stdin and stdout, the tty goes raw, and the parent saves
and restores the termios around the run — libkrun's exit observers do
not run in a SIGKILLed machine. Input is the typed keyboard; Ctrl-C
reaches the guest as a byte (raw mode has no ISIG) and the guest's
line discipline signals its own foreground job. The run refuses to
start without a terminal on stdin and stdout, and `--capture` is
refused with `-t`: the payload's streams cannot land in files and on
the terminal at once.

`-i` without `-t`: pipes. libkrun exposes the run's non-tty stdio
streams as virtio console ports (`krun-stdin`, `krun-stdout`,
`krun-stderr` under `/dev/virtio-ports`), and the guest init points
the payload's stdio onto whichever port nodes exist, so
`echo hi | ecrou run -i alpine -- cat` crosses the machine. A stream
the host left on a tty has no port node and stays on the console.

**Capture.** The console is one stream, kernel and guest mixed, and a
consumer that needs the payload's streams apart cannot read it. With
`--capture DIR` the run adds the directory as an internal rw share at
the reserved guest path `/.ecrou-io`, and the init wraps the payload
so its stdout and stderr land host-side as `<DIR>/stdout` and
`<DIR>/stderr`, written live and simply there when `run` returns. The
console stays on stdout for diagnostics, which is what makes capture
and a nulled stdout composable. A boot failure leaves the files
absent. No caller share may take the reserved guest paths.

**Networking.** TSI, libkrun's default, pinned explicitly instead of
the heuristics mode: the guest has no interface, and its socket calls
are proxied on the host's stack by the VMM process. The guest resolver
is the host's uplink config with loopback nameservers stripped (loopback
never crosses the proxy); DNS is proxied with the rest. Known upstream
limitation: per-stream throughput in this generation of the ln driver
is tens of KB/s against a CDN, TLS included; workable, not fast, and
worth re-measuring on every engine update. No tap, no bridges, ever:
they need host setup the budget cannot afford. Port publishing is how
a later in-guest agent serves the host (Later).

**Memory and CPUs.** Default 1G and all logical CPUs; `-m` overrides.
Each command gets its own machine, and agent consumers run several
concurrently.

## Trust model

What a downloaded image is trusted to be, and where the guarantees
stop:

- Every fetch rides TLS; the trust anchor is the host CA set. There is
  no signature pinning, so a hostile CA or a local interceptor holding
  a trusted certificate is outside what the download code defends
  against.
- OCI layers are content-addressed: each blob is hashed as it streams
  and verified against the digest in the architecture manifest, and
  the architecture manifest is verified against its digest entry in
  the manifest list. The list itself is only TLS-authenticated, the
  same as a plain `docker pull`.
- Direct distro tarballs are checked against the checksum the distro
  publishes next to them. When the checksum artifact cannot be fetched
  the download proceeds unverified with a warning instead of
  hard-failing. That is a deliberate trade, not an oversight.
- The cache trusts its own entries, and `run` never contacts a
  registry or a distro CDN on a cache hit: a present entry is booted
  as-is, the way `docker run` uses the local image. A reference that
  names one entry exactly — an Ubuntu YY.MM, a known codename, a full
  Alpine X.Y.Z — resolves offline; a floating alias (`latest`, `lts`,
  `edge`, an Alpine `X.Y` series) boots the newest cached entry for
  the distro, matching the series or the edge flavor it floats in,
  and needs the network only on a miss. The freshness operation is
  `pull` (the CLI command and the library's `RootfsCache::pull`):
  it re-checks every unpinned OCI tag against the registry digest and
  re-resolves floating distro aliases, downloading only what moved —
  `docker pull` semantics. Digest pins and numbered distro tarballs
  are immutable to both; on a network error `pull` keeps the cached
  image with a warning. `--no-cache` is the force-redownload escape
  hatch, and it resolves upstream like a pull. The trade: a stale
  `:latest` or a stale alias boots until someone pulls, exactly as in
  docker.
- Concurrent runs of the same image are safe, not efficient: each
  writes its own partial file and the last atomic rename wins, so two
  racers download twice rather than corrupt anything.

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
  A guest signal death exits 128+signal (the shell convention crosses
  the control file). A failed VMM reports an engine failure, never a
  guest code.
- **T4, kill chain.** SIGKILL ecrou-cli mid-run; the VM is gone with
  the process, no stray threads or KVM VMs, verified by scanning the
  process table and `/dev/kvm` users.
- **T5, boot economics.** Alpine, cache-warm: image copy plus boot
  lands under 1.5 s to first guest output. Larger images report their
  numbers unblocked; the shared-ro-rootfs row in Later is the fix if
  they hurt.
- **T6, no-mount audit.** `strace -f -e trace=mount,umount2,unshare,
  setns,pivot_root,open_tree,move_mount,fsopen,fsmount` over a full run
  with shares: zero hits. This test is the founding rule.
- **T7, guest network.** An https fetch from the guest succeeds, DNS
  included; a published port is reachable from the host.
- **T8, snap e2e.** CI builds the snap and runs `ecrou run --quiet -e
  DEBIAN_FRONTEND=noninteractive ubuntu -- "apt update && apt install
  -y fastfetch && fastfetch --pipe --logo none"` inside it: strict
  confinement, KVM through the plug, the staged e2fsprogs, apt across
  the TSI proxy, and the payload's exit code back through the control
  file. `--pipe --logo none` keeps the receipt greppable; the console
  is a serial port and fastfetch decorates a tty with cursor escapes.
  A boot of alpine (`uname -r` differing from the host's) runs before
  it so a broken environment fails fast. The apt wall time is the
  standing re-measurement of the ln driver's throughput. First data
  point: 24 s end to end, boot and apt included, cache-warm (snap
  0.1.0 engine, October 2026).

## Snap packaging

Confinement: strict. Plugs: `network`, `home`, `kvm`. The manual
`snap connect` for kvm is accepted until auto-connect is a store
request and a later product decision.

The snap stages `libkrun.so.1` (built with the `blk` feature: the
virtio-blk device is what carries the rootfs) and libkrunfw next to
the binary, plus e2fsprogs for `mkfs.ext4` and `debugfs`; the
kernel ships inside libkrunfw, so there is nothing else to fetch or
stage. No Ubuntu release publishes a `libkrun.so.1` package today, and
libkrunfw5 exists only in the current development series' universe,
not in any LTS the snap bases ship from — so one part builds libkrun
from source and one fetches the libkrunfw5 .deb from the Ubuntu
archives, per architecture (the main archive for amd64 and arm64, the
ports archive for riscv64), each pinned by digest. snapcraft puts
staged libraries on `LD_LIBRARY_PATH` at runtime, which is exactly the
resolution path the `dlopen` in krun.rs takes; there is no ldconfig
and no host dependency. When an LTS base archive ships both packages,
both parts shrink to `stage-packages` entries and the from-source
build is deleted.

## Later

- **QEMU engine.** The TCG fallback for machines without KVM. The
  `RunOptions` API does not change; the engine choice is the probe's.
- **Shared read-only rootfs.** Serve the cache image directly, ro,
  with a guest-side overlay for writes: no copy per run. The
  copy-and-throw-away v1 posture graduates into this.
- **Guest agent, long-lived machines.** Exec without a boot; the shape
  to grow into when per-command VMs stop being right.
- **A build story for hosts that cannot build.** Provision was cut
  from 0.1: a runtime that also builds images is two products, and
  the OCI ecosystem already builds images — CI, digests, scanning.
  What has no answer yet is the locked-down host with no daemon, no
  namespaces, no way to run a builder. There, provision by booting —
  the guest installs, the host stays confined, the artifact is a
  keyed cache entry. Waits on TSI throughput and on someone actually
  needing it.
- **passt** (`krun_set_passt_fd`) if TSI throughput or UDP listening
  ever matters.
- **An agent in the guest** over port publishing: an http backend
  pointed into a machine.
