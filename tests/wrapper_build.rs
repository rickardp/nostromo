//! The config-only install with real cargo: `nostromo init` writes Cargo's user config, and plain
//! `cargo` (no nostromo on PATH, no shell profile) then builds in the store, records, seeds new
//! worktrees and refills evicted skeletons. Uses the synthetic APFS fixture in `tests/fixtures/workspace`.

mod common;
use common::*;
use nostromo::store::{self, Store};
use std::path::{Path, PathBuf};
use std::time::Duration;

const WS_CRATES: [&str; 5] = ["app", "consumer", "gen", "native", "pm"];

fn only_bd(st: &Store, wt: &Path) -> PathBuf {
    let recs: Vec<_> = st.records().into_iter().filter(|r| r.workspace_root.as_deref() == Some(wt)).collect();
    assert_eq!(recs.len(), 1, "one record for {}: {recs:?}", wt.display());
    recs[0].build_dir.clone()
}

#[test]
fn init_build_seed_evict_reseed() {
    let h = Home::new("build");
    let st = h.store();
    let (wt1, wt2) = (worktree("w1"), worktree("w2"));
    for wt in [&wt1, &wt2] {
        let _ = std::fs::remove_dir_all(wt.join("target"));
    }

    // init writes ~/.cargo/config.toml of the fake HOME.
    let o = run_ok(h.nostromo(&["init", "--store", h.store.to_str().unwrap()]));
    let cfg = std::fs::read_to_string(h.home.join(".cargo/config.toml")).unwrap();
    assert!(cfg.contains(&st.template()) && cfg.contains("rustc-wrapper"), "{cfg}\n{}", String::from_utf8_lossy(&o.stdout));
    assert!(!h.home.join("Library/LaunchAgents").exists());

    // Cold build: in the store, recorded with workspace and toolchain.
    let o = run_ok(h.cargo(&wt1, &["build", "--offline"]));
    assert!(!String::from_utf8_lossy(&o.stderr).lines().any(|line| line.trim_start().starts_with("nostromo:")), "{}", String::from_utf8_lossy(&o.stderr));
    let bd1 = only_bd(&st, &wt1);
    assert!(st.contains(&bd1) && bd1.join("debug").is_dir());
    let r1 = st.load_record(&bd1).unwrap();
    assert!(r1.project.is_some() && r1.seeded_from.is_none());
    assert!(r1.toolchain.as_deref().is_some_and(|t| t.contains(if cfg!(target_os = "linux") { "linux" } else { "apple-darwin" })), "{:?}", r1.toolchain);
    let cow = nostromo::util::cow_available(&st.root).unwrap();
    assert!(store::used_path(&bd1).exists(), "compile hook touched the marker");
    let app = |wt: &Path| String::from_utf8_lossy(&run_ok(std::process::Command::new(wt.join("target/debug/app"))).stdout).into_owned();
    let cold = app(&wt1);
    assert!(cold.starts_with("hello="));

    // New worktree: seeded from wt1, only workspace crates compile, identical output.
    let o = run_ok(h.cargo(&wt2, &["build", "--offline"]));
    let bd2 = only_bd(&st, &wt2);
    assert_eq!(st.load_record(&bd2).unwrap().seeded_from.as_deref(), cow.then_some(bd1.as_path()));
    let comp = compiling(&o.stderr);
    assert!(!cow || comp.iter().all(|c| WS_CRATES.contains(&c.as_str())), "only workspace crates rebuild: {comp:?}");
    assert_eq!(app(&wt2), cold);
    assert!(compiling(&run_ok(h.cargo(&wt2, &["build", "--offline"])).stderr).is_empty());

    // Background tick accounts both once idle.
    run_ok(h.nostromo(&["gc", "--auto"]));
    assert!(wait_until(Duration::from_secs(5), || st.records().iter().all(|r| r.footprint.is_some())));

    // Evict everything evictable: wt1 (older) becomes a skeleton with its lock file in place.
    h.write_config("min_free_space = \"1GB\"\nmin_idle = \"0\"\nkeep_recent = 1\nmin_eviction_gain = \"0\"\n");
    let lock = bd1.join("debug/.cargo-build-lock");
    let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap());
    let o = run_ok(h.nostromo(&["gc", "--to", "0"]));
    assert!(String::from_utf8_lossy(&o.stdout).contains("evicted"), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(store::is_unpopulated(&bd1));
    assert_eq!(std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap()), ino);
    assert!(!store::is_unpopulated(&bd2), "keep_recent kept wt2");

    // Rebuilding wt1 refills the skeleton from wt2.
    let o = run_ok(h.cargo(&wt1, &["build", "--offline"]));
    assert_eq!(st.load_record(&bd1).unwrap().seeded_from.as_deref(), cow.then_some(bd2.as_path()));
    let comp = compiling(&o.stderr);
    assert!(!cow || comp.iter().all(|c| WS_CRATES.contains(&c.as_str())), "{comp:?}");
    assert_eq!(app(&wt1), cold);

    // Uninstall restores the Cargo config.
    run_ok(h.nostromo(&["init", "--uninstall"]));
    let cfg = std::fs::read_to_string(h.home.join(".cargo/config.toml")).unwrap();
    assert!(!cfg.contains("nostromo") && !cfg.contains("build-dir"), "{cfg}");

    for wt in [&wt1, &wt2] {
        remove_worktree(wt);
    }
    let _ = std::fs::remove_dir_all(&h.store);
    let _ = std::fs::remove_dir_all(&h.home);
}
