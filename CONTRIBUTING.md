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

## Install from a local checkout

To try your changes with Cargo, run from the repository root:

```sh
cargo install --path .
nostromo init
nostromo doctor
```

On macOS, the store must support CoW (normally APFS). To explicitly accept basic mode without
cache reuse, use `nostromo init --allow-no-cow`.

## Design and behavior

- [Design specification](docs/SPEC.md)
- [Disk conservation requirements](docs/specs/features/disk-conservation.md)
- [Build-triggered maintenance decision](docs/adrs/001-build-triggered-maintenance.md)
- [Basic mode and macOS override decision](docs/adrs/002-basic-mode-without-cow.md)

Keep changes consistent with these documents and update the relevant documentation when behavior changes.

## Releases

The `Release` GitHub Actions workflow runs on every pushed tag. It tests on Linux and macOS,
publishes to crates.io using the `CARGO_REGISTRY_TOKEN` repository secret, and creates a GitHub
release with generated notes. GitHub release permissions come from the built-in `GITHUB_TOKEN`.

1. Update `version` in `Cargo.toml` and run `cargo check` to refresh `Cargo.lock`.
2. Commit and push the version change to `main`.
3. Tag that commit with the matching version and push the tag:

   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```

Use the version you set in step 1; both `v0.1.1` and `0.1.1` are accepted. Tags that do not match
`Cargo.toml` fail before publishing. Versions containing a prerelease suffix (such as `-rc.1`)
create GitHub prereleases.

If publishing succeeds but creating the GitHub release fails, rerun the failed workflow jobs.
An existing crates.io version is left unchanged, and an existing GitHub release is not recreated.
Version `0.1.0` is already on crates.io; tagging it will create only the GitHub release after tests pass.
