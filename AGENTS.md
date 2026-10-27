# AGENTS.md

Conventions for working in this tree. They apply to every commit; the
whole history follows them.

## The founding rule

The SPEC states it; this file makes it operational. The host process
never calls `mount`, `umount2`, `unshare`, `setns`, `pivot_root`, or
any of the new mount API. New code stays inside the confinement budget
(SPEC, "Confinement budget"). Test T6 in `vm.rs` guards the rule; keep
it green. `unsafe` code lives in `krun.rs` (the FFI) and in explicit
`libc` call sites; every unsafe block carries a comment stating the
invariant that makes it sound.

## Before every commit

Run, in order, and make sure they are clean before committing:

```sh
cargo fmt --all
cargo clippy --all-targets   # no new warnings
cargo test                   # keep green when touching behavior
```

These mirror CI. `cargo fmt` may amend files you did not touch; include
those changes in the commit (or in a separate `chore:` commit) rather
than leaving the tree dirty.

## Commit messages

Follow Conventional Commits with a component scope:

```
<type>(<scope>): <short summary>
```

Rules:

- Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`,
  `ci`, `chore`.
- Scopes match the component touched, the module name under
  `crates/ecrou/src/` (library) or `crates/ecrou-cli/src/` (CLI):
  - `cli` - entry point and flags (`crates/ecrou-cli/src/main.rs`,
    `crates/ecrou-cli/src/cli.rs`)
  - `vm` - the run flow, one-shot machines (`crates/ecrou/src/vm.rs`)
  - `krun` - the libkrun FFI (`crates/ecrou/src/krun.rs`)
  - `guest` - the guest init script (`crates/ecrou/src/guest.rs`)
  - `rootfs` - cache-aware preparation and persistence
    (`crates/ecrou/src/rootfs.rs`)
  - `distro` - distro definitions and mirrors (`crates/ecrou/src/distro.rs`)
  - `download` - image download (`crates/ecrou/src/download.rs`)
  - `extract` - rootfs extraction (`crates/ecrou/src/extract.rs`)
  - `config` - config file (`crates/ecrou/src/config.rs`)
  - `utils`, `error`, `progress` - shared helpers
  - `deps` - dependency additions/bumps (manifests, lockfile)
- Omit the scope entirely for repo-wide changes that do not belong to a
  single component (README.md, SPEC.md, root config).
- Summary: imperative mood ("add", never "added" or "adds"), lowercase
  first letter, no trailing period, max ~72 characters.
- Body (optional): separated by a blank line, wrapped at 72 columns;
  explain why rather than what. Reference issues as `#123`.
- A commit touching several components should be split into one commit
  per component when practical; otherwise use the dominant scope.
- **No merge commits.** Integrate work by rebasing onto the target
  branch; the history stays linear.

### Examples

```
feat(krun): bind the eleven-call stable ABI
feat(vm): propagate guest exit codes through the control file
fix(rootfs): reject image references with empty repo
test(vm): cover the control-file exit-code read
deps: add libloading for runtime dlopen
docs: document the spike snap gate
```

## Code

- Tests live inline as `#[cfg(test)]` modules at the bottom of each
  `src/` module; there is no `tests/` directory. Scope test commits to
  the module under test.
- Comments state constraints the code cannot show; no narration.
- Anything user-facing (CLI flags, config keys, error text users act
  on) is reflected in `README.md` before commit.
- Design decisions that outlive the session go to `SPEC.md`, not to
  new docs or repos.
