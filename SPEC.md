# ecrou - implementation specification

## Identity

ecrou is a microVM runtime for disposable Linux environments. It descends
from ecr, the ephemeral chroot: the name keeps the ancestor, and the
answer to "chroot ou VM" is now VM. As a bonus, an écrou is a nut: the
small part that keeps a bolt from walking out of the wall.

One sentence: resolve an image reference, boot it as a one-shot microVM,
run one command with live stdio, propagate its exit code, keep whatever
crossed the workspace share, throw the machine away.

The project is library-first like ecr: `crates/ecrou` is the library
(package `ecrou`), `crates/ecr-cli` becomes `crates/ecrou-cli` (binary
`ecrou`), a thin front-end that parses flags, maps them onto the library
API, and propagates exit codes. The library never writes to the
terminal: diagnostics go through the `log` facade, long operations
report through the `Progress` sink, all output is the CLI's.

The first consumer is rhema's sandbox seam, which shells out to the
binary the way it shells out to `docker` and `lxc` today. The library
API is designed so a second consumer can arrive without forking the
runtime.

## What carries over from ecr

| module | disposition |
|---|---|
| `distro`, `download`, `extract`, `rootfs`, `utils`, `progress` | ported; the image machinery is confinement-neutral plain fs and network |
| `qemu_vm` (cpio builder, KVM probe, launch) | ported and grown; becomes `initramfs` + `qemu` + `vm` |
| `kernel` | ported; Alpine `linux-virt` fetch, cache entry renamed |
| `namespace`, `chroot`, `mount`, `exec` | dropped; they stay in ecr, their ancestor. Their error variants (`UserNamespacesUnavailable`, `BinfmtNotRegistered`, `MissingSubidRange`) leave with them |

No cache compatibility with ecr: the formats may match but the directory
is `~/.cache/ecrou` and never shared.

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

- fs: read, write, stat, rename, unlink, mkdir under the cache directory
  and the share roots it was given, plus its own private tmpdir
- exec: the kernel it boots and `qemu-system-*` (later: libkrun), found
  in PATH or staged in the snap
- `/dev/kvm`: open, ioctl, mmap. Optional; TCG is the fallback when the
  probe finds no working KVM (`KVM_GET_API_VERSION` != 12)
- net: TCP and UDP client sockets for registry fetches, slirp's host
  side, and localhost listeners only through explicit `hostfwd`
- process: fork, exec, waitpid, and signals for its own children

Denied by design: `mount`, `umount2`, `unshare`, `setns`, `pivot_root`,
`open_tree`, `move_mount`, `mount_setattr`, `fsopen`, `fsmount`,
`ptrace`, kernel module loading, keyring calls. All absent from snapd's
seccomp allowlist, which is where the allowed list above is drawn from.
Host `mknod` is allowlisted but AppArmor mediates what it can create;
ecrou has no use for it either way.

## Execution model

One-shot machines. `ecrou run IMAGE -- CMD` boots one VM whose init
execs CMD as the only workload; when CMD exits, the machine powers off.
There is no exec into a running machine. No engine in the family has
that API: libkrun's `krun_set_exec` is per-context before start, and
crun's krun handler boots a fresh VM for every `crun exec`. A guest
agent and long-lived machines are a later row; per-command boots are the
v1 shape, and the boot budget below is what keeps them honest.

Ephemeral rootfs, durable workspace. The base system is an initramfs:
it exists only in RAM and dies with the machine. Persistence crosses a
workspace share or it does not happen.

**Workspace shares.** `--share HOST:GUEST[:ro]`, served by 9p
(`qemu -virtfs local`). The host side is plain fs under one more
directory the budget covers; the guest init mounts it. There is no
implicit share: an empty sandbox is the default, and the consumer
chooses what crosses. This is deliberate; a sandbox runtime does not
silently attach the caller's cwd.

**Exit codes.** The machine has a private control share: a host tmpdir
the API does not surface, mounted read-write at a fixed guest path. The
init writes CMD's exit code to a file there, then powers off; ecrou
reads it back and exits with it. Signal deaths map to 128+signal, the
shell convention the init already produces. Alternatives considered and
rejected for v1: isa-debug-exit (x86-only, awkward from a shell init)
and semihosting exit (arch-dependent). If the exit file is absent and
QEMU itself failed, that is an engine failure, reported as one, never as
a guest code.

**Cancellation is a kill.** ecrou-cli spawns QEMU with
`PR_SET_PDEATHSIG` and a kill-on-drop guard, so a killed consumer kills
the machine. No orphan QEMU survives the CLI. This is stronger than
`docker exec`'s cancel, which kills only the local client; the tests
keep it true (T4).

**Console.** `-serial stdio`, no monitor mux. The QEMU monitor is a
host-level control hatch over the machine; a sandbox runtime does not
mux it into the workload's stdio by accident. `--monitor` restores
`mon:stdio` for development.

**Networking.** slirp user-mode only. The init configures the guest
statically (10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3); no DHCP
client is assumed. No tap, no bridges, ever: they need host setup the
budget cannot afford. `hostfwd` stays available for a later
sandboxd-in-guest.

**Env and argv** ride the kernel command line base64-encoded, ecr's
trick, extended to the environment. Sane size is a few KB; oversized
payloads are a caller bug and fail loudly.

**Memory.** Default 1G, `-m` to override. ecr's CLI defaulted 2G; a
per-command machine is provisioned per command, and agent consumers run
several concurrently.

## The guest side

The init is ours, a generated shell script packed into the initramfs and
selected with `rdinit=`. It grows from ecr's (hostname, shell detection,
argv decode, sysrq poweroff fallback) with static net config, share
mounts, the workdir `cd`, and the exit-code handshake added. It uses
only what the image ships; `busybox` is not assumed.

Kernel: Alpine `linux-virt` per target arch (the `linux-lts` fallback
when no virt flavor exists), fetched and cached as today. The pack step
adds the booted kernel's `/lib/modules` tree into the initramfs: 9p and
virtio_net are modules in `linux-virt`, and the distro image's module
tree does not match the kernel we boot. The size cost is measured in
T5, and gzip compression of the initramfs is the knob if it hurts.

## Workspace layout

Cargo workspace, two crates, the ecr split:

- `crates/ecrou` - the library. Modules: `config`, `distro`,
  `download`, `error`, `extract`, `initramfs`, `kernel`, `progress`,
  `qemu`, `rootfs`, `utils`, `vm`.
- `crates/ecrou-cli` - the CLI (binary `ecrou`).

The library must not depend on CLI types; `clap` is a CLI-only
dependency. Everything else from ecr's rules stands: anyhow internal,
`ecrou::Error` semantic variants at the sites that detect them, `log`
for narration, `Progress` for long operations.

## Library API

### Rootfs lifecycle (ported)

`RootfsCache::new(dir)` / `default_dir()`, `prepare`, `prepare_provisioned`,
`persist`, `PrepareRequest`, `PreparedRootfs`: ecr's semantics verbatim,
including digest freshness on `:latest` and the `.provisioned` hot-cell
marker. See ecr's SPEC for the reference description; differences are
the cache directory and the kernel entry names only.

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
    pub workdir: String,        // guest cwd; empty = first share's guest path, else /
    pub env: Vec<(String, String)>, // composed envp; host env never inherited
    pub hostname: String,       // empty = generated ecrou-vm-XXXXXX
    pub quiet: bool,
    pub require_kvm: bool,
}
pub fn run(options: &RunOptions, progress: &dyn Progress) -> Result<i32>
```

stdio is inherited; the return value is the guest exit code. A
`require_kvm` run with no working KVM is `Error::KvmUnavailable` with
the remediation text. Everything else is `Error::Other` over the anyhow
chain, ecr's shape. The v1 API has no captured-output variant and no
long-lived machine; consumers pump the child's stdio themselves when
they drive the CLI.

## CLI

```
ecrou run [OPTIONS] <IMAGE> [-- COMMAND...]
ecrou probe
```

| Flag | Default | Description |
|------|---------|-------------|
| `-a, --arch <arch>` | host arch | Target architecture |
| `--share <HOST:GUEST[:ro]>` | none | Workspace share; repeatable |
| `-m, --memory <size>` | 1G | Guest memory |
| `--workdir <guest path>` | first share, else `/` | Guest cwd |
| `-e, --env <KEY=VALUE>` | none | Guest env; repeatable |
| `--require-kvm` | false | Fail instead of falling back to TCG |
| `--monitor` | false | Dev: mux the QEMU monitor onto stdio |
| `--quiet` | false | Suppress kernel log on the console |
| `--no-cache` | false | Fresh download, ignore the cache |
| `-v, --verbose` | false | Diagnostics |

`ecrou probe` reports what a run would find: KVM working or the TCG
fallback, the QEMU binary, the kernel in cache, the cache directory
writable. It is the confinement diagnostic and the support tool.

## File layout

```
~/.cache/ecrou/
├── ubuntu-noble-amd64.tar.gz
├── alpine-edge-x86_64.tar.gz
├── <...>.digest                        # manifest digest sidecar
├── <...>.provisioned                   # hot-cell marker
└── ecrou-default-kernel-x86_64.vmlinuz
```

`~/.config/ecrou.yaml` carries over (DNS overrides today).

## Snap packaging

Confinement: strict. Plugs: `network`, `home`, `kvm`. Manual
`snap connect` for kvm is accepted for now; auto-connect is a store
request and a later product decision.

The first milestone is the spike snap, ecr's VM mode behind strict
confinement, before any porting: stage `qemu-system-x86_64` and the
kernel in the snap, or fetch both through the cache in $HOME. Both fit
the budget; T1 is the gate that proves the shape under real AppArmor
and seccomp, and everything else in this document is design until it
passes.

## Falsification tests

- **T1, confinement.** The spike snap runs `ecrou run alpine -- uname
  -a` under strict confinement. The guest kernel version differing from
  the host is the microVM proof, the same check crun's krun man page
  prescribes.
- **T2, workspace coherence.** In a shared workspace, the guest runs
  git init, add, commit, status; the host sees the commits; a second
  machine sees the first's writes. Git's mmap'd index is the trap this
  test exists for.
- **T3, exit codes.** `ecrou run alpine -- sh -c "exit 7"` exits 7.
  A guest signal death exits 128+signal. A killed QEMU reports an
  engine failure, never a guest code.
- **T4, kill chain.** SIGKILL ecrou-cli mid-run; QEMU is gone within a
  second, no orphan, verified by scanning the process table.
- **T5, boot economics.** KVM boot-to-init for alpine with
  `linux-virt` lands under 1.5 s including the module-tree pack cost.
  TCG numbers are recorded, not gated.
- **T6, no-mount audit.** `strace -f -e trace=mount,umount2,unshare,
  setns,pivot_root,open_tree,move_mount,fsopen,fsmount` over a full run
  with shares: zero hits. This test is the founding rule.
- **T7, guest network.** An https fetch from the guest succeeds.

## Later

- **libkrun engine.** The FFI lives here; ecrou already carries unsafe
  libc, and rhema keeps its `forbid(unsafe_code)`. Sub-second boots,
  the kernel bundled in libkrunfw, TSI networking instead of slirp.
  Same budget, one more engine.
- **Tiny initramfs.** A fixed small init with the rootfs itself served
  over 9p, killing the per-run pack cost and the RAM floor.
- **Guest agent, long-lived machines.** Exec without a boot; the shape
  to grow into when per-command VMs stop being right.
- **passt** if slirp throughput ever matters.
- **sandboxd-in-guest** over hostfwd: rhema's http backend pointed
  into a machine.
- **Rootfs persistence** through `prepare_provisioned` hot cells,
  already in the ported code.

## Open questions

- 9p versus virtiofsd as the share transport. 9p needs no extra daemon;
  virtiofsd is the modern path but is another staged binary. Decide
  when T2 runs; the `Share` API does not change either way.
- Initramfs compression default: uncompressed boots fastest, gzip
  halves the RAM floor. Measure in T5.
- Whether `probe` belongs in the library or stays CLI-only.
