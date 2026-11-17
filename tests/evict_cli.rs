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
