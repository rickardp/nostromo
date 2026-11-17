//! `nostromo gc`.

use crate::config::Config;
use crate::evict::{self, Trigger};
use crate::store::Store;
use crate::util;
use std::collections::HashMap;

const USAGE: &str = "usage: nostromo gc [--dry-run] [--to <size>] | --auto\n  --auto       build-triggered tick: free-space watchdog, account finished builds, enforce max_size\n  --dry-run    show what would be evicted, delete nothing\n  --to <size>  evict until the store footprint is at most <size> (e.g. 50GB, 10%); default max_size\n";

pub fn main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--auto") {
        return auto();
    }
    let mut dry_run = false;
    let mut to: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            "--to" => match it.next() {
                Some(v) => to = Some(v.clone()),
                None => {
                    eprintln!("nostromo gc: --to needs a size\n{USAGE}");
                    return 2;
                }
            },
            s if s.starts_with("--to=") => to = Some(s["--to=".len()..].to_string()),
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("nostromo gc: unknown argument `{other}`\n{USAGE}");
                return 2;
            }
        }
    }
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nostromo: {e}");
            return 1;
        }
    };
    let store = Store::new(&cfg.store);
    let target = match to.as_deref().map(util::parse_size).transpose() {
        Ok(None) => None,
        Ok(Some(spec)) => match util::statfs(&store.root) {
            Ok(st) => Some(spec.resolve(st.total)),
            Err(e) => {
                eprintln!("nostromo: statfs {}: {e}", store.root.display());
                return 1;
            }
        },
        Err(e) => {
            eprintln!("nostromo gc: {e}");
            return 2;
        }
    };
    // Workspace names for the report (records are removed as dirs are evicted).
    let names: HashMap<_, _> = store
        .build_dirs()
        .into_iter()
        .map(|d| {
            let r = store.record_or_synth(&d);
            (d, r.workspace_root.map(|p| p.display().to_string()).unwrap_or_else(|| "(unknown)".into()))
        })
        .collect();

    let out = match evict::run(&store, &cfg, Trigger::Manual { target }, dry_run) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nostromo gc: {e}");
            return 1;
        }
    };
    if out.lock_busy {
        eprintln!("nostromo gc: another nostromo process is evicting; try again later");
        return 1;
    }
    let verb = if dry_run { "would evict" } else { "evicted" };
    if out.trash_freed > 0 {
        println!("{} leftover trash: {}", if dry_run { "would delete" } else { "deleted" }, util::human(out.trash_freed));
    }
    for (bd, freed) in &out.evicted {
        let ws = names.get(bd).map(String::as_str).unwrap_or("(unknown)");
        println!("{verb} {} {ws}  ({})", util::human(*freed), bd.display());
    }
    let target_s = target.map(util::human).unwrap_or_else(|| "max_size".into());
    println!(
        "{}: {} freed; footprint was {}, target {target_s}{}",
        if dry_run { "dry run" } else { "done" },
        util::human(out.freed),
        util::human(out.footprint_before),
        if out.target_met { " (met)" } else { " (NOT met)" }
    );
    if out.skipped_busy + out.protected + out.skipped_small > 0 {
        println!(
            "kept: {} in use, {} protected (keep_recent / min_idle), {} would free < 100 MB",
            out.skipped_busy, out.protected, out.skipped_small
        );
    }
    0
}

/// One maintenance tick (the wrapper every 30 s during builds). Quiet.
/// 1. free space below `min_free_space` -> evict (MinFree);
/// 2. account build-dirs used since their last scan, once idle (the post-build size);
/// 3. footprint above `max_size` -> evict (MaxSize).
fn auto() -> i32 {
    let Ok(cfg) = Config::load() else { return 1 };
    let store = Store::new(&cfg.store);
    if !store.build_root().is_dir() {
        return 0;
    }
    if let (Ok(st), Ok(l)) = (util::statfs(&store.root), cfg.limits())
        && st.avail < l.min_free
    {
        let _ = evict::run(&store, &cfg, Trigger::MinFree, false);
    }
    for d in store.build_dirs() {
        let mut r = store.record_or_synth(&d);
        let dirty = r.footprint.is_none() || r.scanned_at.is_none_or(|t| t < r.last_used);
        if !dirty || crate::store::is_unpopulated(&d) {
            continue;
        }
        match crate::locks::try_lock_idle(&d) {
            Ok(Some(idle)) => drop(idle),
            _ => continue, // still building; account on a later tick
        }
        if let Ok(fp) = crate::scan::dir_footprint(&d) {
            r.footprint = Some(fp.bytes);
            r.scanned_at = Some(util::now_secs());
            let _ = store.save_record(&r);
        }
    }
    if let Ok(Some(t)) = evict::needed(&store, &cfg) {
        let _ = evict::run(&store, &cfg, t, false);
    }
    0
}
