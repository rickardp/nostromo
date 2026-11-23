//! `nostromo status` and `nostromo gc` end to end (binary, store and config via env overrides).

#[path = "common_evict/mod.rs"]
mod common;

use common::{Lab, MB, ago};
use std::process::Command;

fn nostromo(lab: &Lab, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_nostromo"))
        .args(args)
        .env("NOSTROMO_STORE", &lab.store.root)
        .env("NOSTROMO_CONFIG", lab.root.join("config.toml"))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn status_and_gc() {
    let lab = Lab::new("cli");
    std::fs::write(lab.root.join("config.toml"), "min_idle = \"0\"\nkeep_recent = 0\nmin_free_space = \"0\"\n").unwrap();
    let ws = lab.workspace("w");
    // Above the default 100 MB "worth evicting" threshold.
    let big = lab.build_dir("aa0001", 120 * MB);
    lab.record(&big, ago(7200), Some(&ws));
    let tiny = lab.build_dir("aa0002", MB);
    lab.record(&tiny, ago(60), Some(&ws));
    let unknown = lab.build_dir("bb0001", MB);

    let (code, s) = nostromo(&lab, &["status"]);
    assert_eq!(code, 0, "{s}");
    assert!(s.contains(&lab.store.root.display().to_string()), "{s}");
    assert!(s.contains("(unknown)") && s.contains(&ws.display().to_string()), "{s}");
    assert!(s.contains("2h") && s.contains("1m"), "{s}");

    let (code, s) = nostromo(&lab, &["status", "--scan"]);
    assert_eq!(code, 0, "{s}");
    assert!(s.contains("127.9 MB of max_size") && s.contains("full scan"), "{s}");

    let (code, s) = nostromo(&lab, &["gc", "--dry-run", "--to", "0"]);
    assert_eq!(code, 0, "{s}");
    assert!(s.contains("would evict 125.8 MB"), "{s}");
    assert!(s.contains("2 would free < 100 MB"), "{s}");
    assert!(big.exists());

    let (code, s) = nostromo(&lab, &["gc", "--to=0"]);
    assert_eq!(code, 0, "{s}");
    assert!(s.contains("evicted 125.8 MB") && s.contains("NOT met"), "{s}");
    assert!(common::gone(&big) && !common::gone(&tiny) && !common::gone(&unknown));

    let (code, s) = nostromo(&lab, &["gc", "--bogus"]);
    assert_eq!(code, 2, "{s}");
}

#[test]
fn doctor_excludes_cached_footprints_after_external_clean() {
    let lab = Lab::new("doctor-external-clean");
    std::fs::write(lab.root.join("config.toml"), "min_free_space = \"0\"\n").unwrap();
    let live = lab.build_dir("aa0001", MB);
    let gone = lab.build_dir("bb0002", MB);
    for (bd, bytes) in [(&live, MB), (&gone, 120 * MB)] {
        let mut record = nostromo::store::Record::new(bd.clone());
        record.footprint = Some(bytes);
        lab.store.save_record(&record).unwrap();
    }
    std::fs::remove_dir_all(&gone).unwrap();
    assert!(lab.store.load_record(&gone).is_some()); // No metadata deletion/race with a new build.

    let cargo_home = lab.root.join("cargo-home");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::write(
        cargo_home.join("config.toml"),
        format!("[build]\nbuild-dir = {:?}\nrustc-wrapper = {:?}\n", lab.store.template(), env!("CARGO_BIN_EXE_nostromo")),
    ).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nostromo"))
        .arg("doctor")
        .env("CARGO_HOME", &cargo_home)
        .env("NOSTROMO_STORE", &lab.store.root)
        .env("NOSTROMO_CONFIG", lab.root.join("config.toml"))
        .output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(text.contains("store size 1.0 MB in 1 build-dir(s)"), "{text}");
    assert_eq!(lab.store.cached_footprint(), MB);
}

fn skeleton(lab: &Lab, hash: &str, age: u64) -> std::path::PathBuf {
    let bd = lab.store.build_root().join(&hash[..2]).join(hash);
    std::fs::create_dir_all(bd.join("debug/deps")).unwrap();
    std::fs::write(bd.join("debug/.cargo-build-lock"), b"").unwrap();
    std::fs::write(bd.join(".rustc_info.json"), b"{}").unwrap();
    age_tree(&bd, age);
    bd
}

fn age_tree(path: &std::path::Path, age: u64) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            age_tree(&entry.unwrap().path(), age);
        }
    }
    let modified = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
    std::fs::File::open(path).unwrap().set_modified(modified).unwrap();
}

#[test]
fn status_hides_only_idle_skeletons_without_deleting_them() {
    use nostromo::locks::{self, Mode};
    let lab = Lab::new("status-skeletons");
    let idle = skeleton(&lab, "aa0001", 8 * 86400);
    let recent = skeleton(&lab, "aa0002", 0);
    let busy = skeleton(&lab, "bb0001", 0);
    let ws = lab.workspace("busy");
    lab.record(&busy, ago(60), Some(&ws));
    let _held = locks::try_lock(&busy.join("debug/.cargo-build-lock"), Mode::Exclusive, false).unwrap().unwrap();
    let unknown = lab.build_dir("cc0001", MB);
    // Even a whitelisted filename is not trivial if it is a symlink.
    let linked = lab.store.build_root().join("dd/dd0001");
    std::fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(unknown.join("debug/deps/blob.rlib"), linked.join(".rustc_info.json")).unwrap();

    for args in [&["status"][..], &["status", "--scan"][..]] {
        let (code, text) = nostromo(&lab, args);
        assert_eq!(code, 0, "{text}");
        let rows: Vec<_> = text.lines().skip_while(|line| !line.contains("WORKSPACE")).skip(1).collect();
        assert_eq!(rows.len(), 3, "{text}");
        assert!(rows.iter().any(|line| line.contains("in use") && line.contains(&ws.display().to_string())), "{text}");
        assert_eq!(rows.iter().filter(|line| line.contains("(unknown)")).count(), 2, "{text}");
        assert!(idle.exists() && recent.exists(), "status must not delete hidden skeletons");
        assert_eq!(lab.store.build_dirs().len(), 5);
    }
}

#[test]
fn auto_cleans_old_idle_skeletons_even_below_limits() {
    use nostromo::locks::{self, Mode};
    let lab = Lab::new("auto-skeletons");
    std::fs::write(lab.root.join("config.toml"), "max_size = \"100%\"\nmin_free_space = \"0\"\n").unwrap();
    let old_age = 8 * 86400;
    let old = skeleton(&lab, "aa0001", old_age);
    lab.record(&old, ago(old_age), None);
    std::fs::write(nostromo::store::used_path(&old), b"").unwrap();
    age_tree(&nostromo::store::used_path(&old), old_age);
    let recent = skeleton(&lab, "aa0002", 0);
    let busy = skeleton(&lab, "aa0003", old_age);
    let marker = skeleton(&lab, "aa0004", old_age);
    std::fs::write(nostromo::store::used_path(&marker), b"").unwrap();
    let seeding = skeleton(&lab, "aa0005", old_age);
    let recorded = skeleton(&lab, "aa0006", old_age);
    lab.record(&recorded, ago(60), None);
    let deep = skeleton(&lab, "aa0007", old_age);
    // Updating an existing nested file does not update its parent directory's mtime.
    std::fs::write(deep.join("debug/.cargo-build-lock"), b"").unwrap();
    let real = lab.build_dir("bb0001", MB);
    age_tree(&real, old_age);
    let linked = skeleton(&lab, "cc0001", old_age);
    std::fs::remove_file(linked.join(".rustc_info.json")).unwrap();
    std::os::unix::fs::symlink(real.join("debug/deps/blob.rlib"), linked.join(".rustc_info.json")).unwrap();
    let _busy = locks::try_lock(&busy.join("debug/.cargo-build-lock"), Mode::Exclusive, false).unwrap().unwrap();
    let _seed = locks::try_lock(&lab.store.meta_dir().join(format!("seed-{}.lock", nostromo::util::path_id(&seeding))), Mode::Exclusive, true).unwrap().unwrap();

    let (code, text) = nostromo(&lab, &["gc", "--dry-run"]);
    assert_eq!(code, 0, "{text}");
    assert!(old.exists(), "dry run must not sweep skeletons");
    let eviction = locks::try_lock(&lab.store.evict_lock_path(), Mode::Exclusive, true).unwrap().unwrap();
    let (code, text) = nostromo(&lab, &["gc", "--auto"]);
    assert_eq!(code, 0, "{text}");
    assert!(old.exists(), "another cleanup holds the eviction lock");
    drop(eviction);

    let (code, text) = nostromo(&lab, &["gc", "--auto"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.is_empty(), "automatic housekeeping stays silent: {text}");
    assert!(!old.exists(), "old idle skeleton must be removed even with no disk pressure");
    assert!(lab.store.load_record(&old).is_none());
    assert!(!nostromo::store::used_path(&old).exists());
    assert_eq!(std::fs::read_dir(lab.store.trash_dir()).unwrap().count(), 0);
    for kept in [&recent, &busy, &marker, &seeding, &recorded, &deep, &real, &linked] {
        assert!(kept.exists(), "{} must be retained", kept.display());
    }
    assert!(lab.store.load_record(&recorded).is_some());
    assert_eq!(std::fs::metadata(real.join("debug/deps/blob.rlib")).unwrap().len(), MB);
}

#[test]
fn manual_gc_sweeps_skeletons_without_evicting_useful_caches() {
    let lab = Lab::new("manual-skeletons");
    std::fs::write(lab.root.join("config.toml"), "max_size = \"100%\"\nmin_free_space = \"0\"\n").unwrap();
    let old = skeleton(&lab, "aa0001", 8 * 86400);
    let real = lab.build_dir("bb0001", MB);
    let (code, text) = nostromo(&lab, &["gc"]);
    assert_eq!(code, 0, "{text}");
    assert!(!old.exists());
    assert!(real.join("debug/deps/blob.rlib").exists());
    assert_eq!(std::fs::read_dir(lab.store.trash_dir()).unwrap().count(), 0);
}
