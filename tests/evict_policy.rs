//! Eviction policy on synthetic stores: order, protection, targets, dry run, locking.

#[path = "common_evict/mod.rs"]
mod common;

use common::{Lab, MB, ago};
use nostromo::evict::{self, Options, Outcome, Trigger};
use nostromo::locks::{self, Mode};
use nostromo::util::SizeSpec;
#[cfg(target_os = "macos")]
use nostromo::util;
use std::path::PathBuf;
use std::time::Duration;

fn opts() -> Options {
    Options { min_freed: 512 * 1024, threads: 2 }
}

fn gc(lab: &Lab, cfg: &nostromo::config::Config, target: u64) -> Outcome {
    evict::run_with(&lab.store, cfg, Trigger::Manual { target: Some(target) }, false, &opts()).unwrap()
}

fn evicted(o: &Outcome) -> Vec<PathBuf> {
    o.evicted.iter().map(|e| e.0.clone()).collect()
}

/// Fresh lock files might still be held by a forked child of a parallel test for a moment.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    (0..50).any(|_| {
        f() || {
            std::thread::sleep(Duration::from_millis(20));
            false
        }
    })
}

#[test]
fn order_trash_orphans_then_lru() {
    let lab = Lab::new("order");
    let ws1 = lab.workspace("one");
    let ws2 = lab.workspace("two");
    let trash_entry = lab.store.trash_dir().join("p-xx-old.1.0");
    std::fs::create_dir_all(trash_entry.join("debug")).unwrap();
    common::write_file(&trash_entry.join("debug/f"), MB);

    let orphan = lab.build_dir("0a0001", MB); // newest of all, but its workspace is gone
    lab.record(&orphan, ago(100), Some(&lab.root.join("ws/deleted")));
    let synth = lab.build_dir("0b0001", MB); // no record at all: synthesized, orphan
    let slot = lab.build_dir("1a0001", MB);
    lab.record(&slot, ago(5000), Some(&ws1));
    let p_old = lab.build_dir("2a0001", MB);
    lab.record(&p_old, ago(4000), Some(&ws1));
    let p_new = lab.build_dir("2b0001", MB);
    lab.record(&p_new, ago(3000), Some(&ws2));
    std::fs::write(nostromo::store::used_path(&slot), b"").unwrap();
    // The marker's mtime counts as use: make it old.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(5000);
    std::fs::File::options().write(true).open(nostromo::store::used_path(&slot)).unwrap().set_modified(old).unwrap();

    let o = gc(&lab, &lab.cfg(), 0);
    assert!(!o.lock_busy);
    assert_eq!(o.trash_freed, MB);
    // The synthesized record's last_used is the dir's mtime (now), so it comes after `orphan`.
    assert_eq!(evicted(&o), vec![orphan.clone(), synth.clone(), slot.clone(), p_old.clone(), p_new.clone()]);
    assert!(o.target_met);
    assert_eq!(o.freed, 6 * MB);
    for d in [&orphan, &synth, &slot, &p_old, &p_new] {
        assert!(common::gone(d), "{} still populated", d.display());
        assert!(d.join("debug/.cargo-build-lock").exists(), "lock files stay in place");
        assert!(lab.store.load_record(d).is_none());
    }
    assert!(!nostromo::store::used_path(&slot).exists());
    assert_eq!(std::fs::read_dir(lab.store.trash_dir()).unwrap().count(), 0);
    assert!(lab.store.records().is_empty());
    // Workspaces are untouched.
    assert!(ws1.join("Cargo.toml").exists() && ws2.join("Cargo.toml").exists());
}

#[test]
fn dir_with_busy_debug_lock_is_never_evicted() {
    let lab = Lab::new("busylock");
    let ws = lab.workspace("w");
    let a = lab.build_dir("aa0001", MB);
    lab.record(&a, ago(9000), Some(&ws));
    let b = lab.build_dir("bb0001", MB);
    lab.record(&b, ago(10), Some(&ws));
    let _held = locks::try_lock(&a.join("debug/.cargo-build-lock"), Mode::Exclusive, false).unwrap().unwrap();

    for trigger in [Trigger::Manual { target: Some(0) }, Trigger::MaxSize, Trigger::MinFree] {
        let mut cfg = lab.cfg();
        cfg.max_size = SizeSpec::Bytes(0);
        cfg.min_free_space = Some(SizeSpec::Percent(100.0));
        let o = evict::run_with(&lab.store, &cfg, trigger, false, &opts()).unwrap();
        assert!(a.join("debug/deps/blob.rlib").exists(), "{trigger:?} evicted a busy dir");
        assert_eq!(o.skipped_busy, 1);
        assert!(!o.target_met);
    }
    assert!(common::gone(&b));
}

#[test]
fn dir_with_busy_cargo_lock_is_never_evicted() {
    let lab = Lab::new("cargolock");
    let ws = lab.workspace("w");
    let a = lab.build_dir("aa0001", MB);
    lab.record(&a, ago(9000), Some(&ws));
    // A second profile under a target triple, held by "cargo".
    std::fs::create_dir_all(a.join("aarch64-apple-darwin/release")).unwrap();
    let l = a.join("aarch64-apple-darwin/release/.cargo-build-lock");
    std::fs::write(&l, b"").unwrap();
    let held = locks::try_lock(&l, Mode::Exclusive, false).unwrap().unwrap();
    let o = gc(&lab, &lab.cfg(), 0);
    assert!(o.evicted.is_empty());
    assert_eq!(o.skipped_busy, 1);
    assert!(a.join("debug/deps/blob.rlib").exists());

    drop(held);
    let mut o = gc(&lab, &lab.cfg(), 0);
    assert!(eventually(|| {
        if o.evicted.is_empty() {
            o = gc(&lab, &lab.cfg(), 0);
        }
        !o.evicted.is_empty()
    }));
    assert!(common::gone(&a));
}

#[test]
fn keep_recent_protects_newest_under_both_triggers() {
    let lab = Lab::new("keep");
    let ws = lab.workspace("w");
    let p1 = lab.build_dir("aa0001", MB);
    lab.record(&p1, ago(30_000), Some(&ws));
    let p2 = lab.build_dir("aa0002", MB);
    lab.record(&p2, ago(20_000), Some(&ws));
    let p3 = lab.build_dir("aa0003", MB);
    lab.record(&p3, ago(10_000), Some(&ws));
    let p4 = lab.build_dir("aa0004", MB);
    lab.record(&p4, ago(5), Some(&ws));
    let mut cfg = lab.cfg();
    cfg.keep_recent = 2;

    let o = evict::run_with(&lab.store, &cfg, Trigger::Manual { target: Some(0) }, true, &opts()).unwrap();
    assert_eq!(evicted(&o), vec![p1.clone(), p2.clone()]);
    assert_eq!(o.protected, 2);

    cfg.min_free_space = Some(SizeSpec::Percent(100.0)); // never satisfiable
    let o = evict::run_with(&lab.store, &cfg, Trigger::MinFree, false, &opts()).unwrap();
    assert_eq!(evicted(&o), vec![p1.clone(), p2.clone()]);
    assert_eq!(o.protected, 2);
    assert!(!o.target_met);
    assert!(!common::gone(&p3) && !common::gone(&p4));
    let w = lab.store.take_warnings();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("min_free_space") && w[0].contains("2 protected"), "{}", w[0]);
}

#[test]
fn min_idle_applies_to_max_size_and_manual_not_min_free() {
    let lab = Lab::new("idle");
    let ws = lab.workspace("w");
    let old = lab.build_dir("aa0001", MB);
    lab.record(&old, ago(7200), Some(&ws));
    let fresh = lab.build_dir("aa0002", MB);
    lab.record(&fresh, ago(10), Some(&ws));
    let mut cfg = lab.cfg();
    cfg.min_idle = Duration::from_secs(3600);
    cfg.max_size = SizeSpec::Bytes(0);

    for t in [Trigger::Manual { target: Some(0) }, Trigger::MaxSize] {
        let o = evict::run_with(&lab.store, &cfg, t, true, &opts()).unwrap();
        assert_eq!(evicted(&o), vec![old.clone()], "{t:?}");
        assert_eq!(o.protected, 1);
    }
    cfg.min_free_space = Some(SizeSpec::Percent(100.0));
    let o = evict::run_with(&lab.store, &cfg, Trigger::MinFree, false, &opts()).unwrap();
    assert_eq!(evicted(&o), vec![old.clone(), fresh.clone()]);
    assert_eq!(o.protected, 0);
    let _ = lab.store.take_warnings();
}

#[test]
fn dry_run_deletes_nothing() {
    let lab = Lab::new("dry");
    let ws = lab.workspace("w");
    let t = lab.store.trash_dir().join("leftover");
    std::fs::create_dir_all(&t).unwrap();
    common::write_file(&t.join("f"), MB);
    let a = lab.build_dir("aa0001", MB);
    lab.record(&a, ago(9000), Some(&ws));
    let b = lab.build_dir("aa0002", MB);
    lab.record(&b, ago(9000), Some(&ws));
    let mut cfg = lab.cfg();
    cfg.min_free_space = Some(SizeSpec::Percent(100.0));
    for trig in [Trigger::Manual { target: Some(0) }, Trigger::MinFree] {
        let o = evict::run_with(&lab.store, &cfg, trig, true, &opts()).unwrap();
        assert_eq!(evicted(&o), vec![a.clone(), b.clone()]);
        assert_eq!(o.trash_freed, MB);
        assert_eq!(o.freed, 3 * MB);
    }
    assert!(t.join("f").exists());
    assert!(a.join("debug/deps/blob.rlib").exists() && b.join("debug/deps/blob.rlib").exists());
    assert_eq!(lab.store.records().len(), 2);
    assert!(lab.store.take_warnings().is_empty());
}

#[test]
fn evict_lock_busy() {
    let lab = Lab::new("busy");
    let a = lab.build_dir("aa0001", MB);
    let _l = locks::try_lock(&lab.store.evict_lock_path(), Mode::Exclusive, true).unwrap().unwrap();
    let o = gc(&lab, &lab.cfg(), 0);
    assert!(o.lock_busy);
    assert!(o.evicted.is_empty());
    assert!(a.exists());
}

#[test]
fn stops_when_target_reached() {
    let lab = Lab::new("target");
    let ws = lab.workspace("w");
    let mut dirs = Vec::new();
    for (i, age) in [(1, 300), (2, 200), (3, 100)] {
        let d = lab.build_dir(&format!("aa000{i}"), MB);
        lab.record(&d, ago(age), Some(&ws));
        dirs.push(d);
    }
    let o = gc(&lab, &lab.cfg(), 2 * MB + MB / 2);
    assert_eq!(evicted(&o), vec![dirs[0].clone()]);
    assert_eq!(o.footprint_before, 3 * MB);
    assert!(o.target_met);
    assert!(!common::gone(&dirs[1]) && !common::gone(&dirs[2]));
    // Already under target: nothing happens.
    let o = gc(&lab, &lab.cfg(), 2 * MB);
    assert!(o.evicted.is_empty() && o.target_met);
    // max_size trigger uses the config's max_size.
    let mut cfg = lab.cfg();
    cfg.max_size = SizeSpec::Bytes(MB);
    let o = evict::run_with(&lab.store, &cfg, Trigger::MaxSize, false, &opts()).unwrap();
    assert_eq!(evicted(&o), vec![dirs[1].clone()]);
    assert!(o.target_met);
}

#[cfg(target_os = "macos")]
#[test]
fn small_and_shared_candidates() {
    let lab = Lab::new("small");
    if !util::cow_available(&lab.root).unwrap() { return; }
    let ws = lab.workspace("w");
    let tiny = lab.build_dir("aa0001", 16 * 1024);
    lab.record(&tiny, ago(9000), Some(&ws));
    // `seed` is a fresh clone of `main`, which keep_recent protects: evicting seed frees ~0.
    let main = lab.build_dir("bb0001", MB);
    lab.record(&main, ago(10), Some(&ws));
    let seed = lab.store.build_root().join("bb/bb0002");
    std::fs::create_dir_all(seed.parent().unwrap()).unwrap();
    util::clonefile(&main, &seed).unwrap();
    lab.record(&seed, ago(8000), Some(&ws));
    // Two idle clones of each other: neither frees anything alone, both together free 1 MB.
    let c1 = lab.build_dir("cc0001", MB);
    lab.record(&c1, ago(7000), Some(&ws));
    let c2 = lab.store.build_root().join("cc/cc0002");
    util::clonefile(&c1, &c2).unwrap();
    lab.record(&c2, ago(6000), Some(&ws));

    let mut cfg = lab.cfg();
    cfg.keep_recent = 1;
    let o = gc(&lab, &cfg, 0);
    assert_eq!(evicted(&o), vec![c1.clone(), c2.clone()]);
    assert_eq!(o.evicted[0].1 + o.evicted[1].1, MB);
    assert_eq!(o.skipped_small, 2); // tiny, seed
    assert!(tiny.exists() && seed.exists() && main.exists());
    assert!(!o.target_met);
}

#[test]
fn needed_reports_trigger() {
    let lab = Lab::new("needed");
    let a = lab.build_dir("aa0001", MB);
    let mut r = nostromo::store::Record::new(a.clone());
    r.footprint = Some(10 * MB);
    lab.store.save_record(&r).unwrap();
    let b = lab.build_dir("aa0002", MB);
    lab.record(&b, ago(1), None); // no footprint: counts 0
    let mut cfg = lab.cfg();
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), None);
    cfg.max_size = SizeSpec::Bytes(10 * MB);
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), None);
    cfg.max_size = SizeSpec::Bytes(10 * MB - 1);
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), Some(Trigger::MaxSize));
    cfg.min_free_space = Some(SizeSpec::Percent(100.0));
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), Some(Trigger::MinFree));
}

#[test]
fn purge_trash_removes_everything() {
    let lab = Lab::new("purge");
    let t = lab.store.trash_dir();
    std::fs::create_dir_all(t.join("a/b/c")).unwrap();
    common::write_file(&t.join("a/b/c/f"), MB);
    std::fs::write(t.join("x.nostromo-used"), b"").unwrap();
    let freed = evict::purge_trash(&lab.store).unwrap();
    assert_eq!(freed, MB);
    assert_eq!(std::fs::read_dir(&t).unwrap().count(), 0);
    assert!(lab.store.build_root().exists());
}

#[test]
fn external_clean_does_not_keep_a_missing_directory_over_max_size() {
    let lab = Lab::new("external-clean-accounting");
    let bd = lab.build_dir("aa0001", MB);
    let mut record = nostromo::store::Record::new(bd.clone());
    record.footprint = Some(120 * MB);
    lab.store.save_record(&record).unwrap();
    let mut cfg = lab.cfg();
    cfg.max_size = SizeSpec::Bytes(100 * MB);
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), Some(Trigger::MaxSize));

    // Plain cargo clean removes the directory without calling the rustc wrapper or updating
    // Nostromo's index. Preserve that old record, but it must no longer count as occupied space.
    std::fs::remove_dir_all(&bd).unwrap();
    assert!(lab.store.load_record(&bd).is_some());
    assert_eq!(evict::needed(&lab.store, &cfg).unwrap(), None);
}
