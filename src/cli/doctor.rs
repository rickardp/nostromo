//! `nostromo doctor`.
//!
//! One line per check: `ok`, `warn` or `fail`, plus an explanation. Exit 1 if anything failed.

use super::init::cargo_config_path;
use crate::config::Config;
use crate::store::Store;
use crate::util;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report {
    failed: bool,
}

impl Report {
    fn line(&mut self, level: Level, msg: impl AsRef<str>) {
        let tag = match level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                self.failed = true;
                "fail"
            }
        };
        println!("{tag}  {}", msg.as_ref());
    }
}

/// Runs a command with null stdin and a timeout; stdout (trimmed) if it exited successfully.
fn run(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn main(args: &[String]) -> i32 {
    if let Some(a) = args.first() {
        if a == "-h" || a == "--help" {
            println!("usage: nostromo doctor");
            return 0;
        }
        eprintln!("nostromo doctor: unknown argument `{a}`");
        return 2;
    }
    let mut r = Report { failed: false };
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            r.line(Level::Fail, format!("config: {e}"));
            return 1;
        }
    };
    let store = Store::new(cfg.store.clone());
    check_cargo(&mut r);
    check_cargo_config(&mut r, &store);
    check_effective(&mut r, &store);
    check_store(&mut r, &cfg, &store);
    if r.failed { 1 } else { 0 }
}

fn check_cargo(r: &mut Report) {
    match run(Command::new("cargo").arg("--version"), Duration::from_secs(30)) {
        Some(v) => {
            // build.build-dir and {workspace-path-hash} are stable since Cargo 1.91.
            let minor = v.split_whitespace().nth(1).and_then(|ver| ver.split('.').nth(1)).and_then(|m| m.parse::<u32>().ok());
            match minor {
                Some(m) if m >= 91 => r.line(Level::Ok, v),
                Some(_) => r.line(Level::Fail, format!("{v}: nostromo needs Cargo 1.91 or newer (build.build-dir)")),
                None => r.line(Level::Warn, format!("cannot parse `{v}`")),
            }
        }
        None => r.line(Level::Fail, "cargo not found on PATH (nostromo itself needs no PATH entry; this check does)"),
    }
}

fn check_cargo_config(r: &mut Report, store: &Store) {
    let p = cargo_config_path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        r.line(Level::Fail, format!("{} not found; run `nostromo init`", p.display()));
        return;
    };
    let t = match toml::from_str::<toml::Table>(&text) {
        Ok(t) => t,
        Err(e) => {
            r.line(Level::Fail, format!("{} is not valid TOML: {e}", p.display()));
            return;
        }
    };
    let get = |k: &str| t.get("build").and_then(|b| b.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    match get("build-dir") {
        Some(v) if v == store.template() => r.line(Level::Ok, format!("{}: build-dir = {v}", p.display())),
        Some(v) => r.line(Level::Fail, format!("{}: build-dir = {v}, expected {} (re-run `nostromo init`)", p.display(), store.template())),
        None => r.line(Level::Fail, format!("{}: no build.build-dir; run `nostromo init`", p.display())),
    }
    match get("rustc-wrapper") {
        Some(w) => {
            let me = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok());
            match Path::new(&w).canonicalize() {
                Ok(c) if Some(&c) == me.as_ref() => r.line(Level::Ok, format!("rustc-wrapper = {w}")),
                Ok(c) => r.line(Level::Warn, format!("rustc-wrapper = {w} resolves to {}, not this nostromo", c.display())),
                Err(_) => r.line(
                    Level::Fail,
                    format!("rustc-wrapper = {w} does not exist: every cargo build fails until you reinstall nostromo or run `nostromo init --uninstall`"),
                ),
            }
        }
        None => r.line(Level::Fail, format!("{}: no build.rustc-wrapper; run `nostromo init`", p.display())),
    }
    for (var, what) in [
        ("CARGO_BUILD_BUILD_DIR", "overrides build-dir: builds bypass the store"),
        ("RUSTC_WRAPPER", "overrides rustc-wrapper: builds still use the store but are not seeded or tracked"),
        ("CARGO_BUILD_RUSTC_WRAPPER", "overrides rustc-wrapper: builds still use the store but are not seeded or tracked"),
        ("NOSTROMO_DISABLE", "disables seeding and tracking"),
    ] {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            r.line(Level::Warn, format!("{var} is set in this environment and {what}"));
        }
    }
}

/// What cargo actually resolves, in a throwaway package inside the store.
fn check_effective(r: &mut Report, store: &Store) {
    let dir = store.tmp_dir().join("doctor");
    let _ = std::fs::create_dir_all(dir.join("src"));
    let _ = std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"nostromo-doctor\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n");
    let _ = std::fs::write(dir.join("src/lib.rs"), "");
    let out = run(Command::new("cargo").current_dir(&dir).args(["metadata", "--no-deps", "--format-version", "1", "--offline"]), Duration::from_secs(30));
    let bd = out
        .and_then(|o| serde_json::from_str::<serde_json::Value>(&o).ok())
        .and_then(|v| v.get("build_directory")?.as_str().map(PathBuf::from));
    match bd {
        Some(b) if store.contains(&b) => r.line(Level::Ok, format!("cargo resolves build-dirs into the store ({})", b.display())),
        Some(b) => r.line(Level::Fail, format!("cargo resolves the build-dir to {}, outside the store", b.display())),
        None => r.line(Level::Warn, "could not run `cargo metadata` to check the effective build-dir"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn check_store(r: &mut Report, cfg: &Config, store: &Store) {
    if !store.root.is_dir() {
        r.line(Level::Warn, format!("store {} does not exist yet (created on first build or by `nostromo init`)", store.root.display()));
    }
    let st = match util::statfs(&store.root) {
        Ok(s) => s,
        Err(e) => {
            r.line(Level::Fail, format!("cannot statfs the store {}: {e}", store.root.display()));
            return;
        }
    };
    match util::cow_available(&store.root) {
        Ok(true) => r.line(Level::Ok, format!("store {} is on {} ({}); CoW cache reuse enabled", store.root.display(), st.fstype, st.mount.display())),
        Ok(false) if cfg!(target_os = "macos") && !cfg.allow_no_cow => r.line(Level::Fail, "CoW unavailable; explicitly run `nostromo init --allow-no-cow` to enable basic mode"),
        Ok(false) => r.line(Level::Ok, format!("store {} is on {}; basic mode (disk cleanup; no CoW cache reuse)", store.root.display(), st.fstype)),
        Err(e) => r.line(Level::Fail, format!("cannot check CoW capability: {e}")),
    }
    let limits = cfg.limits_for(st.total);
    if st.avail < limits.min_free {
        r.line(
            Level::Warn,
            format!("free space {} is below min_free_space {}: idle build-dirs are evicted down to keep_recent", util::human(st.avail), util::human(limits.min_free)),
        );
    } else {
        r.line(Level::Ok, format!("free space {} (min_free_space {})", util::human(st.avail), util::human(limits.min_free)));
    }
    let dirs = store.build_dirs();
    let total = store.cached_footprint();
    let level = if total > limits.max_size { Level::Warn } else { Level::Ok };
    r.line(level, format!("store size {} in {} build-dir(s) (max_size {}, from cached scans)", util::human(total), dirs.len(), util::human(limits.max_size)));
}
