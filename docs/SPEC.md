# nostromo v1 spec

Status: implemented in v0.1.0, config-file install. This document describes the design;
workload-specific measurements and field reports are kept outside the repository.

## Purpose

nostromo primarily conserves disk space used by Cargo build-dirs on macOS and Linux.

1. **Primary: keep build-dirs from filling the disk.** Idle build-dirs are evicted least recently
   used first when the store exceeds `max_size` or free space drops below `min_free_space`.
   Evaluate effectiveness through store footprint, space reclaimed, and free-space headroom.
2. **Secondary: reuse worktree caches.** A new worktree can be seeded by cloning a sibling's
   build-dir with `clonefile`. Faster builds are a bonus, not the main objective or a prerequisite
   for the project to be useful.

See [disk-conservation requirements](specs/features/disk-conservation.md). Passing builds alone
establish neither disk-space savings nor build-time savings.

Correctness comes first. A cache miss is acceptable. A corrupted build, a deleted build-dir that is
in use, or a full disk is not.

## Non-goals (v1)

- `./target` is never touched: no symlinks, no redirection. `build`/`test`/`run` in the same
  worktree keep waiting on Cargo's artifact lock, as they do today.
- Concurrent commands in one worktree share its build-dir and wait on Cargo's lock, as with plain
  Cargo. (The PATH shim of the first design gave them separate slots; see "Why a config file".)
- No Cargo fork, FUSE, kext, VM, privileged service, PATH shim or per-project config.
- No hard size cap. `max_size` is soft and enforced between builds.
- No Linux CoW seeding (Linux uses basic disk management; see ADR-002).

## Why a config file

The first design was a `cargo` shim first on `PATH`, installed through `~/.zshenv` and
`~/.bash_profile`. Many agent harnesses and orchestrators read no shell profile, so they ran plain
cargo. Cargo's user config (`$CARGO_HOME/config.toml`, default `~/.cargo/config.toml`) is read by
every cargo of the user, whatever started it. nostromo therefore installs two settings there:

```toml
[build]
build-dir = "<store>/build/{workspace-path-hash}"
rustc-wrapper = "<path to nostromo>"
```

- `build-dir` routes every build into the store. Cargo applies it itself, even if nostromo is
  broken. Nested cargo runs, external subcommands (`cargo nextest`) and IDEs read the same config.
- `rustc-wrapper` makes Cargo run `nostromo <rustc> <args>` for every rustc invocation, which is
  where nostromo hooks in (next section). Warm no-op builds never invoke it.

With `env -i HOME=… PATH=~/.cargo/bin:/usr/bin:/bin`, cargo builds in the store,
seeds, and records.

Costs: no per-command slots (above); `RUSTC_WRAPPER` in the environment replaces nostromo's
wrapper (builds still use the store but are neither seeded nor tracked); deleting the binary
without `nostromo init --uninstall` breaks every build until it is reinstalled.

## Components

One binary, `nostromo`. Invoked by Cargo as `nostromo <path-to-rustc> …` it is the wrapper;
otherwise the CLI.

| Part | Role |
|---|---|
| wrapper | Probe hook: resolve workspace, record, seed. Compile hook: LRU marker, watchdog trigger. Always ends in `exec rustc` |
| build-triggered maintenance | Cargo wrapper starts `nostromo gc --auto`: free-space watchdog, accounting, `max_size` |
| CLI | `init`, `doctor`, `status`, `gc` |

Dependencies: `libc`, `serde` + `toml` + `serde_json`. No async runtime.

## Store and build-dir naming

nostromo owns a **separate store** and replaces any `build.build-dir` configured before it (the
old line is commented out and restored on uninstall). Cleaning up the old build-dir is not
nostromo's job.

- Store root: `store` in config, default `~/Library/Caches/nostromo` on macOS and `$XDG_CACHE_HOME/nostromo`
  (or `~/.cache/nostromo`) on Linux. Everything under
  `<store>/build/` is nostromo's to account and evict; nothing outside it is ever touched.
- Build-dirs are `<store>/build/{workspace-path-hash}` = `<store>/build/<xx>/<hash>`, named by
  Cargo. A template, never a fixed path: a nested build of another workspace gets its own
  build-dir instead of deadlocking on the parent's lock.
- The store must be on one volume, because eviction renames into
  `<store>/.nostromo/trash`.
- A project config or `CARGO_BUILD_BUILD_DIR` that points elsewhere wins; nostromo then leaves that
  build alone.

## The wrapper

Everything is best effort and ends in `exec rustc`; a panic hook also execs rustc. Skipped
entirely with `NOSTROMO_DISABLE=1` or inside nostromo's own nested cargo runs (`NOSTROMO_ACTIVE`).

**Probe hook.** Cargo runs `rustc - --crate-name ___ --print=…` through the wrapper only when the
build-dir has no `.rustc_info.json` cache: a new or evicted build-dir, or a new toolchain. This
happens before Cargo takes the build-dir lock and before it computes freshness. The hook:
1. Reads the parent cargo's argv (`sysctl(KERN_PROCARGS2)`) for `--manifest-path` and the
   subcommand (`clean` is skipped), and uses its cwd.
2. Resolves the workspace (`cargo locate-project`, git project key) and its build-dir (`cargo
   metadata --no-deps`, `build_directory`). Cached per (cwd, manifest, env) in
   `<store>/.nostromo/cache/`. Nested cargo runs get `RUSTC_WRAPPER=""` and a timeout.
3. If the build-dir is outside the store, stops.
4. Takes `<store>/.nostromo/seed-<id>.lock` (blocking), so a second cold cargo in the same
   worktree waits and then finds the build-dir populated.
5. Writes the record (workspace, project key, toolchain, last_used) and seeds if the build-dir is
   unpopulated.

**Compile hook** (`--out-dir <build-dir>/…`), rate-limited to once per 30 s each:
- touches `<build-dir>.nostromo-used` (its mtime is `last_used`, so LRU sees builds that never
  probe);
- starts a detached `nostromo gc --auto` (new session, no inherited fds), so free space is watched
  during builds.

## Seeding a new build-dir

Triggered by the wrapper's probe hook (below) when the build-dir is missing or a skeleton left by
eviction.

1. **Pick the donor.** A donor must have the same **project key** as the new worktree:
   - the git common dir (`git rev-parse --path-format=absolute --git-common-dir`), which every
     worktree of one repo shares; and
   - the workspace root's path relative to the worktree top level
     (`git rev-parse --show-toplevel`). This stops a monorepo's `backend/` workspace from being
     seeded with `tools/`'s build-dir.

   Among matches, prefer the donor last built with the same toolchain (`rustc -vV` of the active
   toolchain), then the most recently used. Each index record stores its project key and
   toolchain, written on first use. A workspace outside git has no project key and is never
   seeded or used as a donor. Separate clones of the same repo have different common dirs and do
   not seed each other in v1. With no donor, the build starts cold.

   A wrong donor costs time, not correctness: Cargo's fingerprints still decide what is fresh, so
   mismatched artifacts are rebuilt. Only the fix-up rules in step 4 are load-bearing.
2. **Take every Cargo build-dir lock** (`<bd>/<profile>/.cargo-build-lock`, including
   `<bd>/<triple>/<profile>/`) of the donor non-blockingly. If any is busy, try the next donor.
3. **`clonefile`** the donor to a temporary name in the store, then release the donor's locks.
4. **Fix up the clone:**
   - Delete any profile dir whose lock was not held. Cargo may have created it during the clone

   - Rewrite donor→new paths in `build/*/output` and `build/*/root-output`, keeping the files'
     mtimes.
   - Delete `.fingerprint/*/{bin,test,bench,example}-*`, which forces a relink so debug info
     points into the new build-dir. (Mostly subsumed by the next rule, since bins and tests
     belong to workspace packages.)
   - **Invalidate every package with editable sources, and everything that depends on it.** Take
     the packages with `source == null` in `cargo metadata` (workspace members and path
     dependencies), close the set over reverse dependencies and over packages sharing a name, and
     delete their `.fingerprint/<name>-<hash>` dirs. Cargo decides freshness of such
     packages by mtime, so without this a checkout whose files are older than the donor's
     artifacts silently gets the donor's code. Registry and git packages are immutable and keep
     their fingerprints.
   - **Never delete fingerprints outside that closed set.** Rebuilding a crate while a dependent
     keeps its fingerprint causes E0460 (crate hash mismatch).
5. **Install.** A missing target is a single `rename`. A skeleton target is filled entry by entry
   while holding its Cargo locks, keeping its lock files (and their inodes) in place. If another
   process won the race or the skeleton is in use, discard the clone.
6. Record the donor (`seeded_from`). Report seeding only when `NOSTROMO_DEBUG=1` is enabled.

The `cargo metadata` call for the closure runs with `RUSTC_WRAPPER=""` (no recursion), a scratch
build-dir (so it never creates the target), and a 120 s timeout.


## Build-triggered cleanup and accounting

Nostromo must introduce no permission prompt or grant beyond ordinary Cargo writing its cache to
the same location from the same application/context. Existing grants for that baseline must
suffice. `init` configures Cargo and creates the store; it installs no background service and
does not configure Time Machine. The wrapper starts `nostromo gc --auto` at most every 30 s during builds, as a child
of the existing Cargo process context. Starting a new session detaches stdio/job control; it must
not introduce an independent permission context.

While no builds run, maintenance waits for the next build or explicit `nostromo gc`. In
particular, a build that finishes after the last tick may be accounted on the next build.
macOS permissions must not be reset, bypassed, or expanded to recover from a blocked store.
Each tick:
1. **Free space.** If `f_bavail < min_free_space`, run eviction (`MinFree`).
2. **Accounting.** Rescan every build-dir used since its last scan, once its Cargo locks are free.
3. **Size.** If the sum of cached footprints exceeds `max_size`, run eviction (`MaxSize`).

Accounting rules:
- Footprint is allocated bytes from `getattrlistbulk`, deduplicated by `ATTR_CMNEXT_CLONEID`.
- Cached totals for diagnostics and size-limit triggers include only build-dirs still on disk.
  `cargo clean` may leave an index record behind; it must not count as occupied space.
- This overcounts files that are partly shared between clones, never undercounts.
- The sum of per-dir cached footprints overcounts blocks that seeded siblings share. Before
  evicting for `max_size`, a full parallel scan recomputes the union
  footprint and what each dir would free.

## Eviction

Only one process evicts at a time: `<store>/.nostromo/evict.lock` (`LOCK_EX|LOCK_NB`). Others skip
eviction, because someone is already doing it.

A build-dir is **evictable** only if:
- a non-blocking exclusive lock succeeds on every Cargo build-dir lock inside it (Cargo holds it
  for the whole build);
- for `max_size` and manual evictions, it has been idle at least `min_idle` (default 10 min).
  `min_free_space` emergencies ignore `min_idle`;
- it is not among the `keep_recent` (default 2) most recently used build-dirs of live workspaces.
  This holds for **both** kinds of eviction: even when free space is below `min_free_space`,
  nostromo never empties the store. The latest builds survive, and if that is not enough,
  nostromo warns instead of evicting them.

Order, from first to last evicted:
1. **Leftover trash** from an interrupted eviction.
2. **Orphans**: build-dirs whose worktree no longer exists (path gone, or no longer a git
   worktree), least recently used first.
3. **Live build-dirs**, least recently used first.

Skip a candidate whose eviction would free less than `min_eviction_gain` (100 MB). Clones share
blocks, so a freshly seeded build-dir can free almost nothing while still being useful. The freed
estimate is the candidate's blocks not shared with any build-dir that remains, from the full scan.
It is recomputed as dirs are evicted, because a block shared by two dirs becomes private to the
survivor. A second pass evicts groups of idle clones that only free space together.

Known limitation: eviction is whole build-dirs only. A build-dir in constant use (the main
worktree) is always recent, so it is never evicted, and Cargo never removes its stale units. It
grows until it is idle long enough to be evicted. Pruning inside a build-dir is future work.

Steps, until the target is met:
1. Holding the locks, move everything in the build-dir **except the Cargo lock files** into
   `<store>/.nostromo/trash/<name>.<ts>/`. The skeleton left behind (directories + lock files)
   keeps the lock inodes, so a cargo already blocked on a lock proceeds on the same file and
   simply builds cold. If a lock that was not held is busy afterwards (a cargo created a new
   profile dir meanwhile), its entry is moved back.
2. Remove the record and the `.nostromo-used` marker, release the locks, delete the trash entry.
   Deletion retries on `ENOTEMPTY`, because Finder and Spotlight write `.DS_Store` files.
3. Skeletons untouched for 7 days whose Cargo and seeding locks are free are removed. This
   housekeeping runs on every automatic maintenance tick even below the disk limits, as well
   as during manual GC. Age includes nested files, the usage marker and the tracking record;
   contents and age are rechecked under the locks. Unknown files, symlinks and inspection errors
   prevent classification as a trivial skeleton. Tracking metadata is removed only after a
   successful move to trash, which is deleted in the same pass.

The next build in a skeleton probes (its `.rustc_info.json` went to the trash), so the probe hook
re-seeds it from a sibling.

If nothing more is evictable and free space stays below `min_free_space`, a warning goes to
`<store>/.nostromo/warnings`. Ordinary builds never print or consume it; `nostromo status` lists
it.

## Configuration

`$XDG_CONFIG_HOME/nostromo/config.toml`, which is `~/.config/nostromo/config.toml` by default.
Every key is optional.

| Key | Default | Notes |
|---|---|---|
| `max_size` | 25% of the store volume's capacity | Soft cap on the store footprint |
| `min_free_space` | max(20 GB, 5% of the volume) | Floor; triggers emergency eviction |
| `min_idle` | `10m` | Grace before a `max_size` eviction |
| `keep_recent` | `2` | Most recently used build-dirs that are never evicted, even under `min_free_space` |
| `min_eviction_gain` | `100MB` | Skip evicting a build-dir that would free less than this (clones share blocks) |
| `store` | `~/Library/Caches/nostromo` | Root of everything nostromo manages. Changing it needs `nostromo init` again |

Environment variables:
- `NOSTROMO_DISABLE=1`: the wrapper only execs rustc (the build-dir stays in the store).
- `NOSTROMO_CONFIG`, `NOSTROMO_STORE`: override the config file and store.
- `NOSTROMO_DEBUG=1`: explicitly opt into wrapper diagnostics on stderr (hook errors, recovery,
  seeding notices and stored warnings). Without this opt-in, Nostromo adds no output to stdout,
  stderr or the controlling terminal, including during recoverable internal failures. Wrapped
  command output and exit status pass through unchanged. Failure to execute the wrapped tool
  still reports an error and exits 127.

## State

```
<store>/build/<xx>/<hash>/                build-dirs
<store>/build/<xx>/<hash>.nostromo-used   LRU marker (mtime)
<store>/.nostromo/
  index/<id>.json   one record per build-dir: path, worktree, project key, toolchain, last_used,
                    footprint, scanned_at, seeded_from. Written atomically.
  cache/            workspace resolution cache
  tmp/              clones being fixed up
  trash/            evicted contents awaiting deletion
  evict.lock, auto.lock, auto.stamp, seed-<id>.lock
  warnings
```

The store root gets `.metadata_never_index`.

## CLI

- **`nostromo init [--store <dir>] [--allow-no-cow]`**: writes the store to the nostromo config (with `--store`),
  creates the store, edits Cargo's user config (marked block; an existing `build-dir` is commented
  out; an existing `rustc-wrapper` aborts with an explanation).
  Does not request additional permissions or configure Time Machine.
- **`nostromo init --uninstall`**: removes the block (restoring commented-out lines). The store
  is left in place.
- **`nostromo doctor`** checks: Cargo ≥ 1.91; the Cargo config carries the right `build-dir` and
  an existing `rustc-wrapper`; environment overrides; what `cargo metadata` actually resolves;
  CoW/basic mode and explicit macOS consent; free space.
- **`nostromo status [--scan]`**: footprint against `max_size`, free space against
  `min_free_space`, pending warnings, build-dirs by last use (in use / orphan).
  Idle trivial skeletons are omitted from rows and footprint totals; busy or uninspectable
  directories and populated caches without workspace records remain visible. Status is read-only.
- **`nostromo gc [--dry-run] [--to <size>]`**: runs eviction now. **`gc --auto`**: one build-triggered maintenance tick.

## Testing

| Area | How |
|---|---|
| Unit | Config parsing, Cargo-config editing (idempotent, restores user lines, refuses foreign wrappers), fix-up rewrite, LRU ordering, wrapper argument detection |
| Integration (real cargo) | `wrapper_build`: init into a fake HOME, cold build, seeding, eviction to a skeleton, refill, uninstall. `seed_lab` (ignored): editable-source correctness oracle on stable and nightly layouts |
| Eviction | Synthetic stores: order, busy locks, keep_recent, min_idle, clone groups, dry run |
| Fixtures | Synthetic Rust workspaces in `tests/fixtures/workspace`; no workload snapshots or local receipts |

## Open

- Verify inherited macOS TCC responsibility for detached maintenance on an already authorized
  external volume. Local integration tests do not establish external-volume permission behavior.
- Bound filesystem operations so blocked store access cannot stall the compiler hook indefinitely.

## Basic mode

[ADR-002](adrs/002-basic-mode-without-cow.md) extends managed storage and eviction to Linux and
macOS stores without CoW. No donor is copied or hardlinked. macOS installation requires an explicit
`--allow-no-cow` on each non-CoW `init` invocation; consent is recorded for diagnostics.
Linux uses basic mode by default. Non-APFS scanning uses allocated blocks and inode identity,
including hardlinks outside the scanned store when estimating reclamation. Externally created Linux
reflinks are not deduplicated. Existing APFS seeding and clone-aware scanning remain in use.
