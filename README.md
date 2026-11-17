# Nostromo

Keep Cargo build caches from filling your disk. Nostromo gives each workspace a managed build
cache and clears idle caches when space runs low. Keep using `cargo build`, `cargo test`, and
`cargo run` as usual.

- **macOS:** APFS copy-on-write (CoW) lets new Git worktrees reuse a sibling’s build cache.
  Unchanged data shares disk space, and compiled dependencies can make the first build faster.
- **Linux:** managed caches and automatic cleanup, with fresh builds in new worktrees.
  CoW reuse is not supported on Linux.

## Get started

From this checkout, with Rust 1.98 or newer:

```sh
cargo install --path .
nostromo init
nostromo doctor
```

That’s it—run Cargo as usual. Integration uses Cargo’s user config, so it works in terminals
and IDEs without shell setup. Active builds are protected from Nostromo’s cleanup.

On macOS, the store must support CoW (normally APFS). To explicitly accept cleanup without
cache reuse, run `nostromo init --allow-no-cow`.

## Check your caches

```sh
nostromo status          # space used and recent builds
nostromo gc --dry-run    # preview cleanup
```

Before removing the binary, run `nostromo init --uninstall` to restore Cargo’s config.

[Configuration and usage](docs/USAGE.md) · [Contributing](CONTRIBUTING.md) · [Design](docs/SPEC.md)
