//! Run against a disposable non-CoW macOS volume, selected by NOSTROMO_NO_COW_TEST_STORE.
#![cfg(target_os = "macos")]
mod common;
use common::*;
use nostromo::{store::Store, util};

#[test]
#[ignore = "requires NOSTROMO_NO_COW_TEST_STORE on a disposable non-CoW macOS volume"]
fn explicit_override_build_evict_rebuild_and_uninstall() {
    let root = std::env::var_os("NOSTROMO_NO_COW_TEST_STORE").expect("set NOSTROMO_NO_COW_TEST_STORE");
    let path = std::path::PathBuf::from(root).join(format!("nostromo-basic-{}", std::process::id()));
    assert!(!util::cow_available(&path).unwrap());
    let h = Home::new("basic-macos");
    let original = std::fs::read(h.config_path()).unwrap();
    let args = ["init", "--store", path.to_str().unwrap()];
    let o = h.nostromo(&args).output().unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("--allow-no-cow"));
    assert_eq!(std::fs::read(h.config_path()).unwrap(), original);
    assert!(!h.home.join(".cargo/config.toml").exists());
    assert!(!path.exists(), "rejection must happen before creating the store");
    let mut c = h.nostromo(&args);
    c.arg("--allow-no-cow");
    run_ok(c);
    let cfg = std::fs::read_to_string(h.config_path()).unwrap();
    assert!(cfg.contains("allow_no_cow = true"));
    assert!(!h.nostromo(&args).status().unwrap().success(), "reinstall needs explicit flag too");
    run_ok(h.nostromo(&["doctor"]));
    let store = Store::new(&path);
    let a = worktree("basic-a");
    let b = worktree("basic-b");
    for wt in [&a, &b] { run_ok(h.cargo(wt, &["build", "--offline"])); }
    assert_eq!(store.records().len(), 2);
    assert!(store.records().iter().all(|r| r.seeded_from.is_none()));
    let app = |wt: &std::path::Path| run_ok(std::process::Command::new(wt.join("target/debug/app"))).stdout;
    let expected = app(&a);
    assert_eq!(expected, app(&b));
    // Set small test-only limits; keep_recent still protects the newer workspace.
    std::fs::write(h.config_path(), format!("store = {:?}\nallow_no_cow = true\nmin_free_space = \"0\"\nmin_idle = \"0\"\nkeep_recent = 1\nmin_eviction_gain = \"0\"\n", path.to_str().unwrap())).unwrap();
    run_ok(h.nostromo(&["gc", "--to", "0"]));
    assert_eq!(store.build_dirs().iter().filter(|p| nostromo::store::is_unpopulated(p)).count(), 1);
    run_ok(h.cargo(&a, &["build", "--offline"]));
    assert_eq!(expected, app(&a));
    run_ok(h.nostromo(&["status", "--scan"]));
    run_ok(h.nostromo(&["init", "--uninstall"]));
    let cfg = std::fs::read_to_string(h.home.join(".cargo/config.toml")).unwrap();
    assert!(!cfg.contains("rustc-wrapper"));
    remove_worktree(&a);
    remove_worktree(&b);
    std::fs::remove_dir_all(path).unwrap();
}
