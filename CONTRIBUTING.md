# Contributing to Nostromo

Requires Rust 1.98 or newer. Nostromo supports macOS and Linux; APFS is needed to exercise
copy-on-write seeding on macOS. Cargo integration requires Cargo 1.91 or newer.

## Build and test

```sh
cargo build
cargo test
```

`cargo test` runs the reusable regression suite with synthetic workspaces. Tests create their
own temporary repositories and stores; they do not require a particular developer's checkout
or cache. `NOSTROMO_TEST_DIR` can select a temporary root on an APFS volume.

The slower seeding oracle is available with `cargo test --test seed_lab -- --ignored`; its
nightly-layout case requires a nightly toolchain. Offline test builds require the dependency
versions used by Nostromo to be present in Cargo's local cache.

Keep workload-specific reports, receipts, measurements, and experiments outside the checkout.
The `research/`, `docs/research/`, and `.local/` paths are ignored if used for local scratch work.

## Design and behavior

- [Design specification](docs/SPEC.md)
- [Disk conservation requirements](docs/specs/features/disk-conservation.md)
- [Build-triggered maintenance decision](docs/adrs/001-build-triggered-maintenance.md)
- [Basic mode and macOS override decision](docs/adrs/002-basic-mode-without-cow.md)

Keep changes consistent with these documents and update the relevant documentation when behavior changes.
