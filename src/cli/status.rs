//! `nostromo status`.

use crate::config::Config;
use crate::locks;
use crate::scan;
use crate::store::Store;
use crate::util;

const USAGE: &str = "usage: nostromo status [--scan]\n  --scan  measure footprints with a full scan instead of cached figures\n";

pub fn main(args: &[String]) -> i32 {
    let mut full_scan = false;
    for a in args {
        match a.as_str() {
            "--scan" => full_scan = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("nostromo status: unknown argument `{other}`\n{USAGE}");
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
    let st = match util::statfs(&store.root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nostromo: statfs {}: {e}", store.root.display());
            return 1;
        }
    };
    let limits = cfg.limits_for(st.total);
    let dirs = store.build_dirs();
    let mut recs: Vec<_> = dirs.iter().map(|d| store.record_or_synth(d)).collect();
    recs.sort_by_key(|r| std::cmp::Reverse(r.last_used));

    let scanned = if full_scan {
        match scan::scan_dirs(&dirs, 4) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("nostromo: scan failed: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let fp_of = |r: &crate::store::Record| -> Option<u64> {
        match &scanned {
            Some(s) => Some(s.footprint(&r.build_dir)),
            None => r.footprint,
        }
    };
    let (total, total_label) = match &scanned {
        Some(s) => (s.union_bytes(), "union, full scan"),
        None => (recs.iter().filter_map(fp_of).sum(), "sum of cached per-dir figures"),
    };

    println!("store      {}", store.root.display());
    println!("mode       {}", if util::cow_available(&store.root).unwrap_or(false) { "CoW cache reuse and disk cleanup" } else { "basic (disk cleanup; no CoW cache reuse)" });
    println!(
        "footprint  {} of max_size {} ({total_label}){}",
        util::human(total),
        util::human(limits.max_size),
        if total > limits.max_size { "  OVER" } else { "" }
    );
    println!(
        "free       {} (min_free_space {}){}",
        util::human(st.avail),
        util::human(limits.min_free),
        if st.avail < limits.min_free { "  BELOW" } else { "" }
    );
    let trash = std::fs::read_dir(store.trash_dir()).map(|rd| rd.count()).unwrap_or(0);
    if trash > 0 {
        println!("trash      {trash} entr{} awaiting deletion", if trash == 1 { "y" } else { "ies" });
    }
    // Explicit inspection only: ordinary builds never print or consume these warnings.
    if let Ok(w) = std::fs::read_to_string(store.warnings_path()) {
        let mut seen: Vec<&str> = Vec::new();
        for l in w.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !seen.contains(&l) {
                seen.push(l);
                println!("warning    {l}");
            }
        }
    }
    println!();
    if recs.is_empty() {
        println!("no build-dirs");
        return 0;
    }
    println!("{:>10} {:>6}  {:<6}  WORKSPACE", "FOOTPRINT", "AGE", "STATE");
    let now = util::now_secs();
    for r in &recs {
        let state = if matches!(locks::try_lock_idle(&r.build_dir), Ok(None)) {
            "in use"
        } else if r.is_orphan() {
            "orphan"
        } else {
            ""
        };
        let fp = fp_of(r).map(util::human).unwrap_or_else(|| "?".into());
        let ws = r.workspace_root.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(unknown)".into());
        println!("{fp:>10} {:>6}  {state:<6}  {ws}", age(now.saturating_sub(r.last_used)));
    }
    0
}

fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}
