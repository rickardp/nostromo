//! Eviction policy and execution (SPEC "Eviction").
//!
//! Order: leftover trash, orphans (LRU), live build-dirs (LRU). A build-dir is evicted only while
//! `locks::try_lock_idle` holds every Cargo lock in it. Under those locks its contents, except the
//! lock files themselves, are moved into the trash; the remaining skeleton keeps the lock inodes,
//! so a cargo already waiting on a lock simply builds cold. The trash is deleted after the locks
//! are released. Skeletons idle for a week are removed. Nothing outside `<store>/build/` and
//! `<store>/.nostromo/trash/` is ever renamed or deleted.

use crate::config::Config;
use crate::locks::{self, Mode};
use crate::scan;
use crate::store::{Record, Store};
use crate::util;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trigger {
    /// Store footprint above `max_size`: respects `min_idle` and `keep_recent`.
    MaxSize,
    /// Free space below `min_free_space`: ignores `min_idle`, respects `keep_recent`.
    MinFree,
    /// `nostromo gc [--to <size>]`: evict until footprint <= target (default `max_size`);
    /// respects `min_idle` and `keep_recent`.
    Manual { target: Option<u64> },
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// (build-dir, bytes freed estimate)
    pub evicted: Vec<(PathBuf, u64)>,
    pub skipped_busy: usize,
    pub skipped_small: usize,
    pub protected: usize,
    pub freed: u64,
    /// Whether the trigger's target is satisfied afterwards.
    pub target_met: bool,
    /// Another process held the evict lock; nothing was done.
    pub lock_busy: bool,
    /// Bytes of leftover trash deleted (included in `freed`); in a dry run, what would be.
    pub trash_freed: u64,
    /// Union footprint of the store before eviction (from the full scan).
    pub footprint_before: u64,
}

/// Tunables; [`run`] uses [`Options::default`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Skip a candidate whose eviction would free less than this (SPEC: 100 MB).
    pub min_freed: u64,
    /// Scanner threads for the full scan.
    pub threads: usize,
}

impl Default for Options {
    fn default() -> Options {
        Options { min_freed: 100_000_000, threads: 4 }
    }
}

/// Which trigger applies now, if any (cheap: cached footprints + statfs).
pub fn needed(store: &Store, cfg: &Config) -> io::Result<Option<Trigger>> {
    let st = util::statfs(&store.root)?;
    let limits = cfg.limits_for(st.total);
    if st.avail < limits.min_free {
        return Ok(Some(Trigger::MinFree));
    }
    let cached = store.cached_footprint();
    Ok((cached > limits.max_size).then_some(Trigger::MaxSize))
}

/// Runs one eviction pass under `<store>/.nostromo/evict.lock`. `dry_run` plans without deleting.
pub fn run(store: &Store, cfg: &Config, trigger: Trigger, dry_run: bool) -> io::Result<Outcome> {
    let opts = Options { min_freed: cfg.min_eviction_gain, ..Options::default() };
    run_with(store, cfg, trigger, dry_run, &opts)
}

struct Candidate {
    rec: Record,
    orphan: bool,
}

/// What the run is trying to reach.
enum Goal {
    /// Union footprint ≤ target; `current` is tracked from the scan.
    Footprint { target: u64, current: u64 },
    /// `statfs.avail ≥ min_free`. In a dry run `avail` is simulated from freed estimates.
    Free { min_free: u64, avail: u64 },
}

impl Goal {
    fn met(&self) -> bool {
        match *self {
            Goal::Footprint { target, current } => current <= target,
            Goal::Free { min_free, avail } => avail >= min_free,
        }
    }
}

/// [`run`] with explicit [`Options`].
pub fn run_with(store: &Store, cfg: &Config, trigger: Trigger, dry_run: bool, opts: &Options) -> io::Result<Outcome> {
    let mut out = Outcome::default();
    // A dry run takes no destructive action, so it does not need (or block) the evict lock.
    let _evict_lock = if dry_run {
        None
    } else {
        match locks::try_lock(&store.evict_lock_path(), Mode::Exclusive, true)? {
            Some(l) => Some(l),
            None => {
                out.lock_busy = true;
                return Ok(out);
            }
        }
    };

    if !dry_run {
        sweep_skeletons(store);
    }

    // 1. Leftover trash, including skeletons retired in this pass.
    // A trash entry that cannot be deleted must not stop eviction of build-dirs.
    out.trash_freed = if dry_run { trash_bytes(store) } else { purge_trash(store).unwrap_or(0) };
    out.freed += out.trash_freed;

    let st = util::statfs(&store.root)?;
    let limits = cfg.limits_for(st.total);
    let now = util::now_secs();
    let min_idle = cfg.min_idle.as_secs();
    let ignore_idle = trigger == Trigger::MinFree;

    // Candidates and protection. Skeletons hold nothing worth evicting.
    let dirs: Vec<PathBuf> = store.build_dirs().into_iter().filter(|d| !crate::store::is_unpopulated(d)).collect();
    let mut cands: Vec<Candidate> = dirs
        .iter()
        .map(|d| {
            let rec = store.record_or_synth(d);
            let orphan = rec.is_orphan();
            Candidate { rec, orphan }
        })
        .collect();
    // keep_recent: the most recently used build-dirs of live workspaces. An orphan can never be
    // built again, so protecting it would only take protection away from a live worktree.
    let mut live: Vec<&Candidate> = cands.iter().filter(|c| !c.orphan).collect();
    live.sort_by_key(|c| std::cmp::Reverse(c.rec.last_used));
    let keep: HashSet<PathBuf> = live.iter().take(cfg.keep_recent).map(|c| c.rec.build_dir.clone()).collect();

    // Order: orphans, then live build-dirs; least recently used first within each group.
    cands.sort_by_key(|c| (!c.orphan, c.rec.last_used, c.rec.build_dir.clone()));

    let mut eligible = Vec::new();
    for c in cands {
        let too_recent = !ignore_idle && now.saturating_sub(c.rec.last_used) < min_idle;
        if keep.contains(&c.rec.build_dir) || too_recent {
            out.protected += 1;
        } else {
            eligible.push(c);
        }
    }

    // Full scan: union footprint and per-dir freed estimates.
    let sc = scan::scan_dirs(&dirs, opts.threads)?;
    out.footprint_before = sc.union_bytes();
    let mut goal = match trigger {
        Trigger::MinFree => {
            Goal::Free { min_free: limits.min_free, avail: if dry_run { st.avail + out.trash_freed } else { st.avail } }
        }
        Trigger::MaxSize => Goal::Footprint { target: limits.max_size, current: sc.union_bytes() },
        Trigger::Manual { target } => {
            Goal::Footprint { target: target.unwrap_or(limits.max_size), current: sc.union_bytes() }
        }
    };

    let mut gone: HashSet<PathBuf> = HashSet::new();
    let mut busy: HashSet<PathBuf> = HashSet::new();
    let mut small: HashSet<PathBuf> = HashSet::new();

    // Pass 1: the SPEC rule, freed estimate against the dirs that remain.
    // Pass 2 (only if the goal is still unmet): candidates that pass 1 found too small are judged
    // by what they free if every other remaining candidate goes too. Two idle clones of one
    // build-dir each free ~0 alone, but together free their whole footprint; without pass 2 they
    // would block eviction forever.
    for pass in 0..2 {
        if goal.met() || (pass == 1 && small.is_empty()) {
            break;
        }
        let mut i = 0;
        while i < eligible.len() && !goal.met() {
            let bd = eligible[i].rec.build_dir.clone();
            i += 1;
            if gone.contains(&bd) || busy.contains(&bd) || (pass == 1 && !small.contains(&bd)) {
                continue;
            }
            let freed = sc.freed_by(&bd, &gone);
            if freed < opts.min_freed {
                let optimistic = pass == 1 && {
                    let mut all_gone = gone.clone();
                    all_gone.extend(
                        eligible.iter().map(|c| c.rec.build_dir.clone()).filter(|p| *p != bd && !busy.contains(p)),
                    );
                    sc.freed_by(&bd, &all_gone) >= opts.min_freed
                };
                if !optimistic {
                    small.insert(bd);
                    continue;
                }
            }
            match evict_one(store, &bd, dry_run)? {
                Evicted::Busy => {
                    busy.insert(bd);
                }
                Evicted::Done => {
                    small.remove(&bd);
                    gone.insert(bd.clone());
                    out.evicted.push((bd, freed));
                    out.freed += freed;
                    match &mut goal {
                        Goal::Footprint { current, .. } => *current = current.saturating_sub(freed),
                        Goal::Free { avail, .. } => {
                            if dry_run {
                                *avail += freed;
                            } else {
                                *avail = util::statfs(&store.root)?.avail;
                            }
                        }
                    }
                }
            }
        }
    }
    out.skipped_busy = busy.len();
    out.skipped_small = small.len();
    out.target_met = goal.met();

    if !out.target_met && !dry_run && let Goal::Free { min_free, avail } = goal {
        let _ = store.push_warning(&format!(
            "free space on the store volume is {} (below min_free_space {}) and nothing more can be \
             evicted: {} build-dir(s) in use, {} protected (keep_recent), {} too small to matter. \
             Free disk space, or lower min_free_space / keep_recent in {}.",
            util::human(avail),
            util::human(min_free),
            out.skipped_busy,
            out.protected,
            out.skipped_small,
            cfg.path.display(),
        ));
    }
    Ok(out)
}

enum Evicted {
    Done,
    Busy,
}

/// A build-dir is `<store>/build/<xx>/<hash>`: exactly two components below `build/`.
fn is_build_dir(store: &Store, bd: &Path) -> bool {
    bd.strip_prefix(store.build_root()).is_ok_and(|rel| {
        rel.components().count() == 2 && rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
    })
}

fn new_trash_entry(store: &Store, bd: &Path) -> io::Result<PathBuf> {
    let trash = store.trash_dir();
    std::fs::create_dir_all(&trash)?;
    let rel = bd.strip_prefix(store.build_root()).unwrap_or(bd);
    let name = rel.iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join("-");
    let ts = util::now_secs();
    for n in 0.. {
        let dst = trash.join(format!("{name}.{ts}.{n}"));
        match std::fs::create_dir(&dst) {
            Ok(()) => return Ok(dst),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Moves everything under `src` into `dst` except the paths in `keep` (relative), descending
/// into directories that lead to a kept path. Returns the moved top-level-relative paths.
fn move_except(src: &Path, dst: &Path, rel: &Path, keep: &HashSet<PathBuf>, moved: &mut Vec<PathBuf>) -> io::Result<()> {
    for e in std::fs::read_dir(src.join(rel))? {
        let r = rel.join(e?.file_name());
        if keep.contains(&r) {
            continue;
        }
        if keep.iter().any(|k| k.starts_with(&r)) {
            std::fs::create_dir_all(dst.join(&r))?;
            move_except(src, dst, &r, keep, moved)?;
        } else {
            std::fs::rename(src.join(&r), dst.join(&r))?;
            moved.push(r);
        }
    }
    Ok(())
}

fn evict_one(store: &Store, bd: &Path, dry_run: bool) -> io::Result<Evicted> {
    if !is_build_dir(store, bd) {
        return Err(io::Error::other(format!("refusing to evict {}: not a build-dir of the store", bd.display())));
    }
    let Some(idle) = locks::try_lock_idle(bd)? else { return Ok(Evicted::Busy) };
    if dry_run {
        return Ok(Evicted::Done);
    }
    let dst = new_trash_entry(store, bd)?;
    let held: HashSet<PathBuf> = idle.cargo_rel(bd).into_iter().collect();
    let mut moved = Vec::new();
    let r = move_except(bd, &dst, Path::new(""), &held, &mut moved);

    // A cargo that started after try_lock_idle listed the locks may have created a new profile
    // dir (with its own lock). If any lock we do not hold is busy, put its entry back.
    let mut busy = false;
    for l in locks::cargo_lock_files(&dst) {
        if let Ok(None) = locks::try_lock(&l, Mode::Exclusive, false) {
            busy = true;
            let r = l.strip_prefix(&dst).unwrap();
            if let Some(top) = moved.iter().find(|m| r.starts_with(m)) {
                let _ = std::fs::rename(dst.join(top), bd.join(top));
            }
        }
    }
    r?;
    store.remove_record(bd)?;
    let _ = std::fs::remove_file(crate::store::used_path(bd));
    drop(idle);
    util::remove_dir_all_retry(&dst)?;
    Ok(if busy && moved.is_empty() { Evicted::Busy } else { Evicted::Done })
}

/// Light housekeeping on every automatic maintenance tick, even below the disk limits.
/// Uses the same eviction lock as a full eviction pass. Does not evict populated caches.
pub fn cleanup_skeletons(store: &Store) -> io::Result<()> {
    let Some(_evict_lock) = locks::try_lock(&store.evict_lock_path(), Mode::Exclusive, true)? else {
        return Ok(());
    };
    sweep_skeletons(store);
    purge_trash(store)?;
    Ok(())
}

/// Retire trivial skeletons untouched for seven days, with no active Cargo build or seeding.
/// Recheck contents and all use timestamps under the locks before moving to trash.
fn sweep_skeletons(store: &Store) {
    const WEEK: u64 = 7 * 24 * 3600;
    let cutoff = util::now_secs().saturating_sub(WEEK);
    for bd in store.build_dirs() {
        let old_skeleton = || crate::store::skeleton_last_modified(&bd).is_some_and(|t| t <= cutoff)
            && store.record_or_synth(&bd).last_used <= cutoff;
        if !old_skeleton() { continue; }
        let seed_path = store.meta_dir().join(format!("seed-{}.lock", util::path_id(&bd)));
        let Ok(Some(_seed)) = locks::try_lock(&seed_path, Mode::Exclusive, true) else { continue };
        let Ok(Some(idle)) = locks::try_lock_idle(&bd) else { continue };
        if !old_skeleton() { continue; }
        // A newly added Cargo profile must be included in the lock set too.
        let held: Vec<_> = idle.cargo_rel(&bd).into_iter().map(|p| bd.join(p)).collect();
        if locks::cargo_lock_files(&bd) != held { continue; }
        if let Ok(dst) = new_trash_entry(store, &bd) {
            if std::fs::rename(&bd, dst.join("bd")).is_ok() {
                let _ = std::fs::remove_file(crate::store::used_path(&bd));
                let _ = store.remove_record(&bd);
            } else {
                // A failed move must leave the live directory's tracking metadata intact.
                let _ = std::fs::remove_dir(dst);
            }
        }
    }
}

fn trash_entries(store: &Store) -> Vec<PathBuf> {
    let trash = store.trash_dir();
    std::fs::read_dir(&trash)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.starts_with(&trash)).collect())
        .unwrap_or_default()
}

fn entry_bytes(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => scan::dir_footprint(p).map(|f| f.bytes).unwrap_or(0),
        Ok(m) => {
            use std::os::unix::fs::MetadataExt;
            m.blocks() * 512
        }
        Err(_) => 0,
    }
}

fn trash_bytes(store: &Store) -> u64 {
    trash_entries(store).iter().map(|p| entry_bytes(p)).sum()
}

/// Deletes everything in `<store>/.nostromo/trash/`. Returns bytes freed (approximate).
pub fn purge_trash(store: &Store) -> io::Result<u64> {
    let mut freed = 0;
    let mut first_err = None;
    for p in trash_entries(store) {
        let bytes = entry_bytes(&p);
        let r = match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => util::remove_dir_all_retry(&p),
            Ok(_) => std::fs::remove_file(&p).or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) }),
            Err(_) => Ok(()),
        };
        match r {
            Ok(()) => freed += bytes,
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) if freed == 0 => Err(e),
        _ => Ok(freed),
    }
}
