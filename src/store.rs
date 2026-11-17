//! The store: `<store>/build/<xx>/<hash>` build-dirs plus `<store>/.nostromo/` state.
//!
//! ```text
//! <store>/build/<xx>/<hash>/              build-dir of a workspace (Cargo's {workspace-path-hash})
//! <store>/build/<xx>/<hash>.nostromo-used mtime = last rustc invocation in it (wrapper)
//! <store>/.nostromo/index/<id>.json       one Record per build-dir
//! <store>/.nostromo/trash/                renamed build-dirs awaiting deletion
//! <store>/.nostromo/tmp/                  clones being fixed up before rename into place
//! <store>/.nostromo/cache/                workspace lookup cache (cwd + manifest arg -> build-dir)
//! <store>/.nostromo/evict.lock
//! <store>/.nostromo/warnings
//! ```

use crate::util;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Identifies "the same workspace of the same repo" across worktrees (seeding donors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectKey {
    /// `git rev-parse --path-format=absolute --git-common-dir`, shared by all worktrees of a repo.
    pub common_dir: PathBuf,
    /// Workspace root relative to the worktree top level (`""` when they coincide).
    pub rel: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub build_dir: PathBuf,
    /// Canonical workspace root the build-dir belongs to.
    pub workspace_root: Option<PathBuf>,
    pub project: Option<ProjectKey>,
    /// `rustc -vV` summary of the toolchain last used (`<release> <commit> <host>`).
    pub toolchain: Option<String>,
    /// Unix seconds of the last build start or end.
    pub last_used: u64,
    /// Allocated bytes, deduplicated by clone id within the dir, from the last scan.
    pub footprint: Option<u64>,
    pub scanned_at: Option<u64>,
    /// Donor build-dir this one was seeded from, if any.
    #[serde(default)]
    pub seeded_from: Option<PathBuf>,
}

impl Record {
    pub fn new(build_dir: PathBuf) -> Record {
        Record {
            build_dir,
            workspace_root: None,
            project: None,
            toolchain: None,
            last_used: util::now_secs(),
            footprint: None,
            scanned_at: None,
            seeded_from: None,
        }
    }

    /// The workspace is gone (directory removed, or no longer a git worktree when it had a key).
    pub fn is_orphan(&self) -> bool {
        match &self.workspace_root {
            None => true,
            Some(root) => {
                if !root.join("Cargo.toml").is_file() {
                    return true;
                }
                // A linked worktree that `git worktree remove`d but whose dir lingers has no `.git`.
                self.project.is_some() && !has_git_ancestor(root)
            }
        }
    }
}

fn has_git_ancestor(p: &Path) -> bool {
    p.ancestors().any(|a| a.join(".git").exists())
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    pub fn build_root(&self) -> PathBuf {
        self.root.join("build")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.root.join(".nostromo")
    }
    pub fn index_dir(&self) -> PathBuf {
        self.meta_dir().join("index")
    }
    pub fn trash_dir(&self) -> PathBuf {
        self.meta_dir().join("trash")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.meta_dir().join("tmp")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.meta_dir().join("cache")
    }
    pub fn evict_lock_path(&self) -> PathBuf {
        self.meta_dir().join("evict.lock")
    }
    pub fn warnings_path(&self) -> PathBuf {
        self.meta_dir().join("warnings")
    }

    /// Creates the layout and `.metadata_never_index` (keeps Spotlight out of the store).
    pub fn ensure(&self) -> io::Result<()> {
        for d in [self.build_root(), self.index_dir(), self.trash_dir(), self.tmp_dir(), self.cache_dir()] {
            std::fs::create_dir_all(d)?;
        }
        let marker = self.root.join(".metadata_never_index");
        if !marker.exists() {
            std::fs::write(marker, b"")?;
        }
        Ok(())
    }

    /// The `build.build-dir` value nostromo installs into Cargo's config.
    pub fn template(&self) -> String {
        format!("{}/{{workspace-path-hash}}", self.build_root().display())
    }

    pub fn contains(&self, p: &Path) -> bool {
        p.starts_with(self.build_root())
    }

    fn record_path(&self, build_dir: &Path) -> PathBuf {
        self.index_dir().join(format!("{}.json", util::path_id(build_dir)))
    }

    /// The record with `last_used` raised to the wrapper's `.nostromo-used` mtime.
    pub fn load_record(&self, build_dir: &Path) -> Option<Record> {
        let s = std::fs::read(self.record_path(build_dir)).ok()?;
        serde_json::from_slice(&s).ok().map(with_used)
    }

    pub fn save_record(&self, r: &Record) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(r).map_err(io::Error::other)?;
        util::write_atomic(&self.record_path(&r.build_dir), &bytes)
    }

    pub fn remove_record(&self, build_dir: &Path) -> io::Result<()> {
        match std::fs::remove_file(self.record_path(build_dir)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Every index record (unreadable records are skipped).
    pub fn records(&self) -> Vec<Record> {
        let Ok(rd) = std::fs::read_dir(self.index_dir()) else { return Vec::new() };
        rd.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
            .map(with_used)
            .collect()
    }

    /// Cached allocated footprint of build-dirs that still exist in this store. Plain Cargo
    /// clean can delete a directory without updating our index. Keep its record available for
    /// reuse/inspection, but do not count it toward disk limits or diagnostics.
    pub fn cached_footprint(&self) -> u64 {
        self.build_dirs().iter().filter_map(|d| self.load_record(d)).filter_map(|r| r.footprint).sum()
    }

    /// Every build-dir on disk: directories at `build/<xx>/<hash>`.
    pub fn build_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let dirs = |p: &Path| -> Vec<PathBuf> {
            std::fs::read_dir(p)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default()
        };
        for xx in dirs(&self.build_root()) {
            out.extend(dirs(&xx));
        }
        out.sort();
        out
    }

    /// Record for a build-dir, synthesizing one for dirs without a record (the wrapper's hook did
    /// not run, e.g. `RUSTC_WRAPPER` overridden): `last_used` = newest of the dir's mtimes and
    /// its `.nostromo-used` marker.
    pub fn record_or_synth(&self, build_dir: &Path) -> Record {
        if let Some(r) = self.load_record(build_dir) {
            return r;
        }
        let mut r = Record::new(build_dir.to_path_buf());
        r.last_used = last_mtime(build_dir);
        with_used(r)
    }

    pub fn push_warning(&self, msg: &str) -> io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(self.meta_dir())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.warnings_path())?;
        writeln!(f, "{msg}")
    }

    /// Returns and clears stored warnings for explicit acknowledgement.
    pub fn take_warnings(&self) -> Vec<String> {
        let p = self.warnings_path();
        let Ok(s) = std::fs::read_to_string(&p) else { return Vec::new() };
        let _ = std::fs::remove_file(&p);
        // The watchdog may record the same condition after every build: show each message once.
        let mut out: Vec<String> = Vec::new();
        for l in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !out.iter().any(|o| o == l) {
                out.push(l.to_string());
            }
        }
        out
    }
}

/// `<build-dir>.nostromo-used`: touched by the wrapper (rate-limited) on every rustc invocation.
pub fn used_path(build_dir: &Path) -> PathBuf {
    let mut s = build_dir.as_os_str().to_owned();
    s.push(".nostromo-used");
    PathBuf::from(s)
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn with_used(mut r: Record) -> Record {
    if let Some(t) = mtime_secs(&used_path(&r.build_dir)) {
        r.last_used = r.last_used.max(t);
    }
    r
}

/// Files Cargo or nostromo leave in a build-dir that hold no build state.
const SKELETON_FILES: &[&str] = &[".cargo-build-lock", ".cargo-lock", ".rustc_info.json", "CACHEDIR.TAG"];

/// True if `build_dir` is missing or holds no build state: only directories, Cargo lock files,
/// `.rustc_info.json` and `CACHEDIR.TAG`. Eviction leaves such a skeleton (the lock files stay
/// in place so a cargo waiting on one is never split from the dir), and seeding fills it.
pub fn is_unpopulated(build_dir: &Path) -> bool {
    fn walk(d: &Path, depth: u32) -> bool {
        let Ok(rd) = std::fs::read_dir(d) else { return true };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { return false };
            if t.is_dir() {
                if depth >= 4 || !walk(&e.path(), depth + 1) {
                    return false;
                }
            } else if !SKELETON_FILES.iter().any(|n| e.file_name() == *n) {
                return false;
            }
        }
        true
    }
    walk(build_dir, 0)
}

/// Newest mtime among `dir` and its immediate children (profile dirs get touched by builds).
pub fn last_mtime(dir: &Path) -> u64 {
    let m = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    let mut best = m(dir);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            best = best.max(m(&e.path()));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_records() {
        let root = std::env::temp_dir().join(format!("nostromo-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let s = Store::new(&root);
        s.ensure().unwrap();
        assert!(root.join(".metadata_never_index").exists());
        assert_eq!(s.template(), format!("{}/build/{{workspace-path-hash}}", root.display()));
        let bd = root.join("build/ab/cdef0123456789");
        std::fs::create_dir_all(bd.join("debug")).unwrap();
        std::fs::write(used_path(&bd), b"").unwrap();
        assert_eq!(s.build_dirs(), vec![bd.clone()]);
        assert!(is_unpopulated(&bd));
        std::fs::write(bd.join("debug/.cargo-build-lock"), b"").unwrap();
        std::fs::write(bd.join(".rustc_info.json"), b"{}").unwrap();
        assert!(is_unpopulated(&bd));
        std::fs::create_dir_all(bd.join("debug/.fingerprint/x-0123")).unwrap();
        std::fs::write(bd.join("debug/.fingerprint/x-0123/lib-x"), b"h").unwrap();
        assert!(!is_unpopulated(&bd));
        let mut r = Record::new(bd.clone());
        r.footprint = Some(42);
        s.save_record(&r).unwrap();
        assert_eq!(s.load_record(&bd).unwrap().footprint, Some(42));
        assert_eq!(s.records().len(), 1);
        assert!(r.is_orphan());
        s.push_warning("low disk").unwrap();
        s.push_warning("low disk").unwrap();
        assert_eq!(s.take_warnings(), vec!["low disk".to_string()]);
        assert!(s.take_warnings().is_empty());
        s.remove_record(&bd).unwrap();
        assert!(s.records().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
