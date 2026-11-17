//! Synthetic workspace used by build/seeding tests; contains no workload snapshots or receipts.
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| nostromo::util::home().join(".cargo"))
}

pub fn rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME").map(PathBuf::from).unwrap_or_else(|| nostromo::util::home().join(".rustup"))
}

pub fn test_root(name: &str) -> PathBuf {
    let base = std::env::var_os("NOSTROMO_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!("nostromo-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

pub fn create(root: &Path) -> PathBuf {
    fn copy(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() { copy(&e.path(), &dst.join(e.file_name())); }
            else { std::fs::copy(e.path(), dst.join(e.file_name())).unwrap(); }
        }
    }
    copy(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace"), root);
    for args in [vec!["git", "init", "--quiet"], vec!["cargo", "generate-lockfile", "--offline"],
                 vec!["git", "add", "Cargo.toml", "crates"],
                 vec!["git", "-c", "user.name=nostromo-test", "-c", "user.email=test@invalid", "commit", "--quiet", "-m", "synthetic fixture"]] {
        let out = Command::new(args[0]).args(&args[1..]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    root.canonicalize().unwrap()
}
