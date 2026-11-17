//! Footprint accounting: APFS clone IDs on macOS/APFS, allocated inode blocks in basic mode.
//!
//! Only regular files are counted (directories and symlinks are metadata-sized). Symlinks are
//! never followed and unreadable subdirectories are skipped.
//!
//! An unmodified clone and a hardlink share the original's clone id; a clone gets a new id on its
//! first write. Deduplicating by clone id therefore counts partly shared files in full: the result
//! overcounts partly shared files. Basic mode deduplicates by device/inode and accounts for
//! external hardlinks when estimating reclamation; externally created Linux reflinks are not deduplicated.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "macos")]
mod apfs;
mod basic;

type FileId = (u64, u64);

#[derive(Debug, Clone, Copy, Default)]
pub struct DirFootprint {
    /// Allocated bytes, each clone id counted once.
    pub bytes: u64,
    pub files: u64,
}

/// Raw per-dir scan result: (clone id, allocated bytes) per regular file, plus files with no
/// clone id, which cannot be deduplicated.
#[derive(Debug, Default)]
struct RawScan {
    entries: Vec<(FileId, u64)>,
    /// (links observed, total inode links), for basic-mode hardlink accounting.
    links: HashMap<FileId, (u64, u64)>,
    no_clone_bytes: u64,
    files: u64,
}

impl RawScan {
    /// Sorts by clone id and keeps one entry per id. Returns the deduplicated byte total.
    fn dedup(&mut self) -> u64 {
        self.entries.sort_unstable();
        self.entries.dedup_by_key(|e| e.0);
        self.entries.shrink_to_fit();
        self.entries.iter().map(|e| e.1).sum::<u64>() + self.no_clone_bytes
    }
}

fn scan_raw(dir: &Path) -> io::Result<RawScan> {
    #[cfg(target_os = "macos")]
    if crate::util::cow_available(dir)? { return apfs::scan_raw(dir); }
    basic::scan_raw(dir)
}

/// Footprint of one directory tree.
pub fn dir_footprint(dir: &Path) -> io::Result<DirFootprint> {
    let mut raw = scan_raw(dir)?;
    let bytes = raw.dedup();
    Ok(DirFootprint { bytes, files: raw.files })
}

/// Per-dir data of a [`StoreScan`].
#[derive(Debug, Default)]
struct DirData {
    /// Deduplicated footprint of the dir on its own.
    footprint: u64,
    /// Bytes of clone ids held by this dir only (plus files without a clone id).
    private: u64,
    /// Clone ids this dir shares with other dirs: index into `StoreScan::shared`.
    shared: Vec<u32>,
    files: u64,
}

/// A clone id present in two or more dirs.
#[derive(Debug)]
struct Shared {
    alloc: u64,
    holders: Vec<u32>,
}

/// Full scan of several build-dirs, keeping enough per-file data to answer "how much would
/// evicting X free, given these others are already gone" (blocks shared by two dirs become
/// private to the survivor).
#[derive(Debug, Default)]
pub struct StoreScan {
    index: HashMap<PathBuf, usize>,
    dirs: Vec<DirData>,
    shared: Vec<Shared>,
    union: u64,
}

impl StoreScan {
    pub fn union_bytes(&self) -> u64 {
        self.union
    }

    /// Deduplicated footprint of one scanned dir (0 if it was not scanned).
    pub fn footprint(&self, dir: &Path) -> u64 {
        self.index.get(dir).map_or(0, |&i| self.dirs[i].footprint)
    }

    /// Regular files counted in one scanned dir.
    pub fn files(&self, dir: &Path) -> u64 {
        self.index.get(dir).map_or(0, |&i| self.dirs[i].files)
    }

    /// Total regular files across all scanned dirs.
    pub fn total_files(&self) -> u64 {
        self.dirs.iter().map(|d| d.files).sum()
    }

    /// Bytes freed by evicting `dir` when every dir in `gone` has already been evicted.
    pub fn freed_by(&self, dir: &Path, gone: &HashSet<PathBuf>) -> u64 {
        let Some(&i) = self.index.get(dir) else { return 0 };
        let gone_idx: HashSet<u32> =
            gone.iter().filter_map(|g| self.index.get(g)).map(|&g| g as u32).collect();
        let d = &self.dirs[i];
        let me = i as u32;
        d.private
            + d.shared
                .iter()
                .map(|&s| &self.shared[s as usize])
                .filter(|s| s.holders.iter().all(|h| *h == me || gone_idx.contains(h)))
                .map(|s| s.alloc)
                .sum::<u64>()
    }
}

/// Parallel scan (`threads` workers) of the given build-dirs. A dir that cannot be opened (for
/// example, evicted meanwhile) counts as empty.
pub fn scan_dirs(dirs: &[PathBuf], threads: usize) -> io::Result<StoreScan> {
    let results: Vec<Mutex<RawScan>> = dirs.iter().map(|_| Mutex::new(RawScan::default())).collect();
    let next = AtomicUsize::new(0);
    let threads = threads.clamp(1, dirs.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= dirs.len() {
                        break;
                    }
                    if let Ok(mut raw) = scan_raw(&dirs[i]) {
                        raw.dedup();
                        *results[i].lock().unwrap() = raw;
                    }
                }
            });
        }
    });
    let raws: Vec<RawScan> = results.into_iter().map(|m| m.into_inner().unwrap()).collect();
    Ok(build(dirs, raws))
}

fn build(dirs: &[PathBuf], raws: Vec<RawScan>) -> StoreScan {
    // Every (clone id, dir) pair, sorted by clone id: each run is one clone id.
    let total: usize = raws.iter().map(|r| r.entries.len()).sum();
    let mut all: Vec<(FileId, u32)> = Vec::with_capacity(total);
    for (i, r) in raws.iter().enumerate() {
        all.extend(r.entries.iter().map(|e| (e.0, i as u32)));
    }
    all.sort_unstable();

    // Per-dir allocation of each clone id (dirs may disagree slightly; take the max).
    let alloc_of = |dir: u32, clone: FileId| -> u64 {
        let v = &raws[dir as usize].entries;
        v.binary_search_by_key(&clone, |e| e.0).map_or(0, |k| v[k].1)
    };

    let mut out = StoreScan {
        index: HashMap::new(),
        dirs: raws
            .iter()
            .map(|r| DirData {
                footprint: r.entries.iter().map(|e| e.1).sum::<u64>() + r.no_clone_bytes,
                private: r.no_clone_bytes,
                shared: Vec::new(),
                files: r.files,
            })
            .collect(),
        shared: Vec::new(),
        union: raws.iter().map(|r| r.no_clone_bytes).sum(),
    };

    let mut links: HashMap<FileId, (u64, u64)> = HashMap::new();
    for raw in &raws {
        for (&id, &(seen, total)) in &raw.links {
            let entry = links.entry(id).or_default();
            entry.0 += seen;
            entry.1 = entry.1.max(total);
        }
    }
    let mut k = 0;
    while k < all.len() {
        let clone = all[k].0;
        let mut end = k + 1;
        while end < all.len() && all[end].0 == clone {
            end += 1;
        }
        let alloc = all[k..end].iter().map(|&(_, d)| alloc_of(d, clone)).max().unwrap_or(0);
        out.union += alloc;
        let external = links.get(&clone).is_some_and(|(seen, total)| seen < total);
        if end - k == 1 && !external {
            out.dirs[all[k].1 as usize].private += alloc;
        } else {
            let id = out.shared.len() as u32;
            let mut holders: Vec<u32> = all[k..end].iter().map(|&(_, d)| d).collect();
            for &h in &holders {
                out.dirs[h as usize].shared.push(id);
            }
            if external { holders.push(u32::MAX); } // an unscanned hardlink keeps the blocks alive
            out.shared.push(Shared { alloc, holders });
        }
        k = end;
    }
    for (i, d) in dirs.iter().enumerate() {
        out.index.insert(d.clone(), i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(entries: &[(u64, u64)]) -> RawScan {
        let mut r = RawScan { entries: entries.iter().map(|&(id, bytes)| ((0, id), bytes)).collect(), files: entries.len() as u64, ..Default::default() };
        r.dedup();
        r
    }

    #[test]
    fn freed_by_shared_ids() {
        let dirs: Vec<PathBuf> = ["/a", "/b", "/c"].iter().map(PathBuf::from).collect();
        // a: 1 private (10), 2 shared with b (20), 3 shared with b and c (30)
        let s = build(&dirs, vec![raw(&[(1, 10), (2, 20), (3, 30)]), raw(&[(2, 20), (3, 30)]), raw(&[(3, 30), (4, 5)])]);
        assert_eq!(s.union_bytes(), 65);
        assert_eq!(s.footprint(Path::new("/a")), 60);
        let none = HashSet::new();
        assert_eq!(s.freed_by(Path::new("/a"), &none), 10);
        let b: HashSet<PathBuf> = [PathBuf::from("/b")].into();
        assert_eq!(s.freed_by(Path::new("/a"), &b), 30);
        let bc: HashSet<PathBuf> = [PathBuf::from("/b"), PathBuf::from("/c")].into();
        assert_eq!(s.freed_by(Path::new("/a"), &bc), 60);
        assert_eq!(s.freed_by(Path::new("/nope"), &bc), 0);
    }
}
