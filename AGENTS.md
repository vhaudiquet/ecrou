# AGENTS.md

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
  - `supervise` - machine process fork and wait (`crates/ecrou/src/supervise.rs`)
  - `krun` - the libkrun FFI (`crates/ecrou/src/krun.rs`)
  - `guest` - the guest init script (`crates/ecrou/src/guest.rs`)
  - `rootfs` - cache-aware preparation and persistence
    (`crates/ecrou/src/rootfs.rs`)
  - `cache` - the rootfs entry store: naming, locking, freshness,
    layer pointers (`crates/ecrou/src/cache.rs`)
  - `image` - ext4 image building and inspection
    (`crates/ecrou/src/image.rs`)
  - `distro` - distro definitions and mirrors (`crates/ecrou/src/distro.rs`)
  - `download` - image download (`crates/ecrou/src/download.rs`)
  - `extract` - tar decoding and the OCI layer merge feeding the image
    build (`crates/ecrou/src/extract.rs`)
  - `registry` - OCI registry client (`crates/ecrou/src/registry.rs`)
  - `utils`, `error`, `progress` - shared helpers
  - `deps` - dependency additions/bumps (manifests, lockfile)
- Omit the scope entirely for repo-wide changes that do not belong to a
  single component (README.md, SPEC.md, root config).
- Summary: imperative mood ("add", never "added" or "adds"), lowercase
  first letter, no trailing period, max ~72 characters.
- Body (optional): separated by a blank line, wrapped at 72 columns;
  explain why rather than what. Reference issues as `#123`.
  Keep body short for small changes (max. 4 lines) and expand into
  max. 3 paragraphs for big changes.
- A commit touching several components should be split into one commit
  per component when practical; otherwise use the dominant scope.
  **Never break bisectability.**
  Each commit should build and work on their own.
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
- Design decisions that outlive the session go to `SPEC.md`, not to
  new docs or repos.
