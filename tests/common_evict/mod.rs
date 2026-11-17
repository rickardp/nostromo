//! Synthetic stores for eviction/accounting tests. Data lives in the system temp dir (APFS) and is
//! removed when the `Lab` is dropped.
#![allow(dead_code)]

use nostromo::config::Config;
use nostromo::store::{Record, Store};
use nostromo::util::{self, SizeSpec};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MB: u64 = 1024 * 1024;

pub struct Lab {
    pub root: PathBuf,
    pub store: Store,
}

impl Lab {
    pub fn new(name: &str) -> Lab {
        let base = std::env::var_os("NOSTROMO_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!("nostromo-evict-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Canonical, so paths match what the store derives (/var -> /private/var).
        let root = root.canonicalize().unwrap();
        let store = Store::new(root.join("store"));
        store.ensure().unwrap();
        Lab { root, store }
    }

    /// A live workspace (has Cargo.toml, no project key so no git check).
    pub fn workspace(&self, name: &str) -> PathBuf {
        let ws = self.root.join("ws").join(name);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), b"[package]\n").unwrap();
        ws
    }

    /// `build/<xx>/<hash>` with `debug/.cargo-build-lock` and one file of `bytes` bytes.
    pub fn build_dir(&self, hash: &str, bytes: u64) -> PathBuf {
        let bd = self.store.build_root().join(&hash[..2]).join(hash);
        std::fs::create_dir_all(bd.join("debug/deps")).unwrap();
        std::fs::write(bd.join("debug/.cargo-build-lock"), b"").unwrap();
        write_file(&bd.join("debug/deps/blob.rlib"), bytes);
        bd
    }

    pub fn record(&self, bd: &Path, last_used: u64, ws: Option<&Path>) {
        let mut r = Record::new(bd.to_path_buf());
        r.last_used = last_used;
        r.workspace_root = ws.map(Path::to_path_buf);
        self.store.save_record(&r).unwrap();
    }

    /// Config with no limits in the way: huge max_size, no min_free, no min_idle, keep_recent 0.
    pub fn cfg(&self) -> Config {
        Config {
            path: self.root.join("config.toml"),
            store: self.store.root.clone(),
            allow_no_cow: false,
            max_size: SizeSpec::Bytes(u64::MAX / 2),
            min_free_space: Some(SizeSpec::Bytes(0)),
            min_idle: Duration::from_secs(0),
            keep_recent: 0,
            min_eviction_gain: 100_000_000,
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = util::remove_dir_all_retry(&self.root);
    }
}

/// Writes `bytes` of non-zero data (real allocation, no sparse holes).
pub fn write_file(p: &Path, bytes: u64) {
    let chunk = vec![0xa5u8; MB as usize];
    let mut left = bytes as usize;
    let mut data = Vec::with_capacity(left);
    while left > 0 {
        let n = left.min(chunk.len());
        data.extend_from_slice(&chunk[..n]);
        left -= n;
    }
    std::fs::write(p, data).unwrap();
}

pub fn ago(secs: u64) -> u64 {
    util::now_secs() - secs
}

/// Evicted: only the skeleton (dirs + lock files) is left.
pub fn gone(bd: &Path) -> bool {
    nostromo::store::is_unpopulated(bd)
}
