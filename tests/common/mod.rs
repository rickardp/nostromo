//! Helpers for wrapper integration tests. Synthetic data lives under a temporary dir; the real home
//! directory and Cargo config are never touched: every command runs with a fake HOME and a fake
//! CARGO_HOME whose registry and git caches are symlinks to the real ones (offline builds).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};

mod fixture;
use std::time::{Duration, Instant};

static GIT: Mutex<()> = Mutex::new(());

pub fn lab() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| fixture::test_root("wrapper")).clone()
}

pub fn fixture() -> PathBuf {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| fixture::create(&lab().join("fixture"))).clone()
}

pub fn nostromo() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nostromo"))
}

/// A fresh fake HOME (with `.cargo` linking the real caches) and its store.
pub struct Home {
    pub home: PathBuf,
    pub store: PathBuf,
}

impl Home {
    pub fn new(name: &str) -> Home {
        let home = lab().join(format!("home-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        let ch = home.join(".cargo");
        std::fs::create_dir_all(&ch).unwrap();
        for d in ["registry", "git"] {
            std::os::unix::fs::symlink(fixture::cargo_home().join(d), ch.join(d)).unwrap();
        }
        let store = lab().join(format!("store-{name}"));
        let _ = std::fs::remove_dir_all(&store);
        let h = Home { home, store };
        h.write_config("min_free_space = \"1GB\"\n");
        h
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join(".config/nostromo/config.toml")
    }

    /// nostromo config (the store line is kept by `init --store`; tests rewrite it fully).
    pub fn write_config(&self, extra: &str) {
        let p = self.config_path();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("store = \"{}\"\n{extra}", self.store.display())).unwrap();
    }

    /// A clean environment like a harness that reads no shell profile: no nostromo on PATH.
    pub fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}/bin:{}", fixture::cargo_home().display(), std::env::var("PATH").unwrap_or_default()))
            .env("RUSTUP_HOME", fixture::rustup_home())
            .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()))
            .env("USER", std::env::var_os("USER").unwrap_or_default());
    }

    pub fn cargo(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new("cargo");
        self.env(&mut c);
        c.current_dir(cwd).args(args);
        c
    }

    pub fn nostromo(&self, args: &[&str]) -> Command {
        let mut c = Command::new(nostromo());
        self.env(&mut c);
        c.args(args);
        c
    }

    pub fn store(&self) -> nostromo::store::Store {
        nostromo::store::Store::new(&self.store)
    }
}

impl Drop for Home {
    fn drop(&mut self) { let _ = nostromo::util::remove_dir_all_retry(&lab()); }
}

/// A detached `git worktree` of the fixture with its lockfile (so `--offline` builds work).
pub fn worktree(name: &str) -> PathBuf {
    let _g = GIT.lock().unwrap_or_else(|e| e.into_inner());
    let wt = lab().join(format!("wt-{name}"));
    if !wt.join("Cargo.toml").exists() {
        let st = Command::new("git").arg("-C").arg(fixture()).args(["worktree", "add", "--detach", "--quiet"]).arg(&wt).arg("HEAD").status().unwrap();
        assert!(st.success(), "git worktree add failed");
        std::fs::copy(fixture().join("Cargo.lock"), wt.join("Cargo.lock")).unwrap();
    }
    wt.canonicalize().unwrap()
}

pub fn remove_worktree(wt: &Path) {
    let _g = GIT.lock().unwrap_or_else(|e| e.into_inner());
    let _ = Command::new("git").arg("-C").arg(fixture()).args(["worktree", "remove", "--force"]).arg(wt).status();
    let _ = std::fs::remove_dir_all(wt);
}

pub fn run_ok(mut c: Command) -> Output {
    let o = c.output().unwrap();
    assert!(
        o.status.success(),
        "command failed: {:?}\nstdout: {}\nstderr: {}",
        c,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    o
}

pub fn compiling(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("Compiling "))
        .map(|s| s.split_whitespace().next().unwrap().to_string())
        .collect()
}

pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}
