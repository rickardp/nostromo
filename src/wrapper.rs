//! `build.rustc-wrapper`: Cargo runs `nostromo <rustc> <args...>` for every rustc invocation,
//! including its target-info probe. Everything here is best effort and ends in `exec rustc`: a
//! bug in nostromo must never fail a build.
//!
//! - Probe (`rustc - --crate-name ___ --print=...`): Cargo runs it only when the build-dir has no
//!   `.rustc_info.json` cache, i.e. for a new or evicted build-dir or a new toolchain, and before
//!   it takes the build-dir lock or computes freshness. The hook resolves the workspace (from the
//!   parent cargo's argv and cwd), records it, and seeds an unpopulated build-dir from a sibling.
//! - Compile (`--out-dir <bd>/...`): touches `<bd>.nostromo-used` (LRU), starts a detached
//!   `nostromo gc --auto` at most every 30 s (free-space watchdog). Nostromo diagnostics are
//!   silent unless `NOSTROMO_DEBUG=1` is explicitly enabled.

use crate::cargo_args;
use crate::config::Config;
use crate::locks::{self, Mode};
use crate::seed;
use crate::store::{self, Record, Store};
use crate::util;
use crate::workspace::{self, RealCargo, Workspace};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Rate limit for touching `.nostromo-used` and for starting `gc --auto`.
const EVERY_SECS: u64 = 30;

/// `nostromo <path-to-rustc-or-driver> ...`: argv[1] is a path or a rustc/clippy driver name.
pub fn is_wrapper_invocation(args: &[OsString]) -> bool {
    let Some(a) = args.get(1).and_then(|a| a.to_str()) else { return args.len() > 1 };
    let name = Path::new(a).file_name().and_then(|n| n.to_str()).unwrap_or(a);
    a.contains('/') || name.starts_with("rustc") || name.starts_with("clippy-driver")
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn main(args: Vec<OsString>) -> ! {
    let tool = args[1].clone();
    let rest: Vec<OsString> = args[2..].to_vec();
    {
        // panic = "abort": on a bug, exec rustc from the panic hook instead of failing the build.
        let (tool, rest) = (tool.clone(), rest.clone());
        std::panic::set_hook(Box::new(move |info| {
            debug(&format!("internal error ({info}); continuing without it"));
            let e = Command::new(&tool).args(&rest).exec();
            eprintln!("nostromo: cannot run {}: {e}", tool.to_string_lossy());
            std::process::exit(127);
        }));
    }
    if !env_flag("NOSTROMO_DISABLE") && !env_flag("NOSTROMO_ACTIVE") {
        hook(&rest);
    }
    let e = Command::new(&tool).args(&rest).exec();
    eprintln!("nostromo: cannot run {}: {e}", tool.to_string_lossy());
    std::process::exit(127);
}

fn hook(rest: &[OsString]) {
    let Ok(cfg) = Config::load() else { return };
    let store = Store::new(cfg.store.clone());
    if is_probe(rest) {
        if let Err(e) = probe_hook(&store) {
            debug(&format!("probe hook: {e}"));
        }
    } else if let Some(out_dir) = arg_value(rest, "--out-dir") {
        compile_hook(&store, Path::new(&out_dir));
    }
}

fn debug(msg: &str) {
    if env_flag("NOSTROMO_DEBUG") {
        eprintln!("nostromo: {msg}");
    }
}

fn is_probe(rest: &[OsString]) -> bool {
    rest.windows(2).any(|w| w[0] == "--crate-name" && w[1] == "___")
}

fn arg_value(rest: &[OsString], flag: &str) -> Option<OsString> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
    }
    None
}

/// `<store>/build/<xx>/<hash>` containing `p`, if `p` is inside the store's build root.
fn build_dir_of(store: &Store, p: &Path) -> Option<PathBuf> {
    let rel = p.strip_prefix(store.build_root()).ok()?;
    let mut c = rel.components();
    let (a, b) = (c.next()?, c.next()?);
    Some(store.build_root().join(a).join(b))
}

fn compile_hook(store: &Store, out_dir: &Path) {
    let Some(bd) = build_dir_of(store, out_dir) else { return };
    let used = store::used_path(&bd);
    if util::age_secs(&used).is_none_or(|a| a >= EVERY_SECS) {
        let _ = util::touch(&used);
    }
    maybe_spawn_gc(store);
    show_warnings(store);
}

/// Starts `nostromo gc --auto` detached, at most once per [`EVERY_SECS`] across all processes.
fn maybe_spawn_gc(store: &Store) {
    let stamp = store.meta_dir().join("auto.stamp");
    if util::age_secs(&stamp).is_some_and(|a| a < EVERY_SECS) {
        return;
    }
    let Ok(Some(_l)) = locks::try_lock(&store.meta_dir().join("auto.lock"), Mode::Exclusive, true) else { return };
    if util::age_secs(&stamp).is_some_and(|a| a < EVERY_SECS) {
        return;
    }
    let _ = util::touch(&stamp);
    let _ = spawn_detached(&["gc", "--auto"]);
}

/// Canonical path of the running nostromo binary.
pub fn self_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

/// Runs `nostromo <args>` in a new session with no stdio and no inherited fds (jobserver pipes,
/// cargo's pipes), so it can outlive the build without holding anything of it.
/// Spawn from Cargo's existing context, preserving its permission responsibility.
pub fn spawn_detached(args: &[&str]) -> std::io::Result<()> {
    let exe = self_exe().ok_or_else(|| std::io::Error::other("cannot resolve own executable"))?;
    let mut c = Command::new(&exe);
    c.arg0("nostromo").args(args).env_remove("NOSTROMO_ACTIVE");
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls (setsid, getrlimit, fcntl) between fork and exec.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            let mut rl: libc::rlimit = std::mem::zeroed();
            let max = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 { rl.rlim_cur.min(10240) as i32 } else { 1024 };
            for fd in 3..max {
                let fl = libc::fcntl(fd, libc::F_GETFD);
                if fl >= 0 && fl & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, fl | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    drop(c.spawn()?);
    Ok(())
}

/// Diagnostics are opt-in and never consume warnings needed by `nostromo status`.
fn show_warnings(store: &Store) {
    if env_flag("NOSTROMO_DEBUG") {
        if let Ok(warnings) = std::fs::read_to_string(store.warnings_path()) {
            for warning in warnings.lines().filter(|w| !w.is_empty()) {
                debug(warning);
            }
        }
    }
}

/// Cached workspace resolution for a (cwd, manifest arg, config) key.
#[derive(Serialize, Deserialize)]
struct Resolved {
    ws: Workspace,
    build_dir: PathBuf,
}

fn probe_hook(store: &Store) -> Result<(), String> {
    let parent = util::proc_args(unsafe { libc::getppid() }).unwrap_or_default();
    let inv = cargo_args::parse_args(parent.get(1..).unwrap_or_default());
    if inv.subcommand.as_deref() == Some("clean") {
        return Ok(());
    }
    // Cargo's cwd (after any `-C`): the probe runs in it.
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let manifest = inv.manifest_path.as_ref().map(|m| cwd.join(m));

    let cargo_path = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .or_else(|| parent.first().map(PathBuf::from).filter(|p| p.is_absolute()))
        .unwrap_or_else(|| PathBuf::from("cargo"));
    let real = RealCargo { path: cargo_path, toolchain: std::env::var("RUSTUP_TOOLCHAIN").ok() };

    let resolved = resolve(store, &real, &cwd, manifest.as_deref())?;
    let (ws, bd) = (resolved.ws, resolved.build_dir);
    if !store.contains(&bd) {
        return Ok(()); // the build-dir is configured elsewhere (project config or env override)
    }
    store.ensure().map_err(|e| e.to_string())?;

    // One hook per build-dir at a time: a second cold cargo in the same worktree waits here and
    // then finds the build-dir populated.
    let _seed_lock = locks::lock(&store.meta_dir().join(format!("seed-{}.lock", util::path_id(&bd))), Mode::Exclusive, true)
        .map_err(|e| e.to_string())?;

    let mut rec = store.load_record(&bd).unwrap_or_else(|| Record::new(bd.clone()));
    rec.workspace_root = Some(ws.root.clone());
    rec.project = ws.project.clone();
    if let Some(t) = workspace::toolchain_id(&real, &ws.root) {
        rec.toolchain = Some(t);
    }
    if store::is_unpopulated(&bd) {
        match seed::seed(store, &real, &ws, &bd, false) {
            Ok(Some(s)) => {
                let from = s.donor_workspace.as_deref().unwrap_or(&s.donor).display().to_string();
                debug(&format!("seeded build-dir from {from} ({:.1} s)", s.secs));
                rec.seeded_from = Some(s.donor);
                rec.footprint = None;
                rec.scanned_at = None;
            }
            Ok(None) => {}
            Err(e) => debug(&format!("seeding failed, building cold: {e}")),
        }
    }
    rec.last_used = util::now_secs();
    store.save_record(&rec).map_err(|e| e.to_string())?;
    let _ = util::touch(&store::used_path(&bd));
    Ok(())
}

fn resolve(store: &Store, real: &RealCargo, cwd: &Path, manifest: Option<&Path>) -> Result<Resolved, String> {
    let key = format!(
        "{}\0{}\0{}\0{}",
        cwd.display(),
        manifest.map(|m| m.display().to_string()).unwrap_or_default(),
        std::env::var("CARGO_BUILD_BUILD_DIR").unwrap_or_default(),
        std::env::var("CARGO_HOME").unwrap_or_default(),
    );
    let cache = store.cache_dir().join(format!("{:016x}.json", util::fnv64(key.as_bytes())));
    if let Ok(b) = std::fs::read(&cache)
        && let Ok(r) = serde_json::from_slice::<Resolved>(&b)
        && r.ws.manifest.is_file()
    {
        return Ok(r);
    }
    let ws = workspace::locate(real, cwd, manifest).ok_or("not in a cargo workspace")?;
    let build_dir = workspace::build_dir(real, &ws).map_err(|e| e.to_string())?;
    let r = Resolved { ws, build_dir };
    if let Ok(b) = serde_json::to_vec(&r) {
        let _ = std::fs::create_dir_all(store.cache_dir());
        let _ = util::write_atomic(&cache, &b);
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn detects_wrapper_mode_and_probe() {
        assert!(is_wrapper_invocation(&os(&["nostromo", "/r/bin/rustc", "-vV"])));
        assert!(is_wrapper_invocation(&os(&["nostromo", "rustc", "-"])));
        assert!(is_wrapper_invocation(&os(&["nostromo", "clippy-driver", "rustc"])));
        for cli in ["init", "doctor", "status", "gc", "--version", "help"] {
            assert!(!is_wrapper_invocation(&os(&["nostromo", cli])), "{cli}");
        }
        assert!(!is_wrapper_invocation(&os(&["nostromo"])));
        assert!(is_probe(&os(&["-", "--crate-name", "___", "--print=file-names"])));
        assert!(!is_probe(&os(&["-vV"])));
        assert_eq!(arg_value(&os(&["--crate-name", "x", "--out-dir", "/s/b/ab/cd/debug/deps"]), "--out-dir"), Some(OsString::from("/s/b/ab/cd/debug/deps")));
        let st = Store::new("/s");
        assert_eq!(build_dir_of(&st, Path::new("/s/build/ab/cdef/debug/deps")), Some(PathBuf::from("/s/build/ab/cdef")));
        assert_eq!(build_dir_of(&st, Path::new("/elsewhere/debug/deps")), None);
    }
}
