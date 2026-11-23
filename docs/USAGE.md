# Using Nostromo

[Back to README](../README.md)

## Installation and Cargo integration

`nostromo init` adds two settings to `~/.cargo/config.toml` (inside marked lines).
For example, with the default macOS store:

```toml
[build]
build-dir = "~/Library/Caches/nostromo/build/{workspace-path-hash}"   # expanded
rustc-wrapper = "~/.cargo/bin/nostromo"                                # expanded
```

An existing `build-dir` line is commented out; `nostromo init --uninstall` removes nostromo's lines
and restores it. Uninstall before deleting the binary: while the config points at a missing
`rustc-wrapper`, every cargo build fails.

Cleanup is triggered by Cargo builds, using their existing permission context. No background service,
Full Disk Access request, or automatic Time Machine configuration is installed.
While no builds run, cleanup waits until the next build
or an explicit `nostromo gc`.

## Use

Run `cargo` as usual. Nostromo adds no output to stdout, stderr, or the terminal during normal
builds; wrapped-command output and exit status pass through unchanged. Disk-pressure warnings stay
in the store for explicit inspection. To opt into Nostromo diagnostics, run
`NOSTROMO_DEBUG=1 cargo …` (adds Nostromo messages to stderr).

Occasionally:

```sh
nostromo status          # store size, free space, warnings, build-dirs by last use
nostromo gc --dry-run    # what eviction would remove
```

Per command: `NOSTROMO_DISABLE=1 cargo …` skips seeding and tracking (the build-dir stays in the
store); `CARGO_BUILD_BUILD_DIR=… cargo …` builds elsewhere entirely. A `RUSTC_WRAPPER` set in the
environment (e.g. sccache) replaces nostromo's wrapper: builds still use the store, without
seeding or tracking.

## Cleanup scope

`nostromo gc` is the managed cleanup path: it applies the configured limits and protects
build-dirs whose Cargo locks are busy. Inspect `nostromo gc --dry-run` before a manual cleanup.

Plain `cargo clean` is Cargo's own destructive cleanup, outside Nostromo's eviction policy.
With no options, it removes the workspace's configured intermediate build-dir
and final-artifact directory across profiles. Under the installed hash template, different
worktree paths have separate intermediate namespaces; sessions using the same worktree share
one namespace. Explicit Cargo configuration can change that scope.

Do not run plain `cargo clean` while another build uses those directories: whole-directory
cleaning does not acquire the build locks. Nostromo is a rustc wrapper and cannot intercept
that operation. Cargo's reported byte total is summed file lengths, not measured physical
space reclaimed; sparse files and APFS clones make the distinction matter.

After an external clean, missing build-dirs are excluded from Nostromo's cached size totals.
Their index records may remain for inspection and future reuse.

Idle directories containing only Cargo locks and probe metadata are hidden from `status`.
They are removed after seven days without use when their Cargo and seeding locks are free.
This housekeeping runs during build-triggered maintenance even below the disk limits, or during
manual `nostromo gc`. Status and dry runs never remove them. Untracked caches containing build
artifacts remain visible as `orphan (unknown)`.

## Configure

`~/.config/nostromo/config.toml`, every key optional:

| Key | Default |
|---|---|
| `store` | macOS: `~/Library/Caches/nostromo`; Linux: `$XDG_CACHE_HOME/nostromo` or `~/.cache/nostromo` |
| `allow_no_cow` | `false`; macOS consent recorded by `init --allow-no-cow` |
| `max_size` | `25%` of the store volume (soft cap, enforced by build-triggered cleanup) |
| `min_free_space` | max(20 GB, 5% of the volume) |
| `min_idle` | `10m` before `max_size` may evict a build-dir |
| `keep_recent` | `2` most recently used build-dirs, never evicted |
| `min_eviction_gain` | `100MB`: skip build-dirs whose eviction would free less |

Changing `store` needs `nostromo init` again (it is part of the Cargo config).

Design: [specification](SPEC.md).

## Platform details

Linux uses basic mode: managed storage, usage tracking and idle eviction. New worktrees build
from scratch; Linux CoW seeding is not supported. Basic mode never copies or hardlinks donor caches.

On macOS, put the store on APFS to enable copy-on-write (CoW) reuse between Git worktrees.
Nostromo can clone a sibling worktree’s build cache: unchanged data shares disk blocks, while
later writes get their own blocks. This can save space and let a fresh worktree reuse compiled
dependencies. Cargo still checks freshness and rebuilds what changed.

To choose a different store:

```sh
nostromo init --store /Volumes/Fast/nostromo
```

On macOS, installation fails if the chosen store does not support CoW. To explicitly accept
basic mode, pass the override on each installation:

```sh
nostromo init --store /Volumes/SSD/nostromo --allow-no-cow
```

The override is saved as `allow_no_cow = true`; `nostromo doctor` and `nostromo status` report
the active mode. Linux does not need this override.

Basic-mode scans count allocated blocks, deduplicate hardlinks, and exclude symlinks. They do not
deduplicate externally created reflinks on Linux; footprint and dry-run reclamation figures on such
filesystems are estimates. Actual free-space cleanup rechecks volume availability after eviction.

## Uninstall

Remove the Cargo integration **before** removing the binary:

```sh
nostromo init --uninstall
cargo uninstall nostromo
```

Build caches remain in the store.
