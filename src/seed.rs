//! Seeding a new build-dir by cloning a donor and fixing it up (SPEC "Seeding").
//!
//! Both Cargo build-dir layouts are handled, per profile dir (`<bd>/<profile>/` and
//! `<bd>/<triple>/<profile>/`):
//! - old (stable): `.fingerprint/<pkg>-<16hex>/`, build scripts in `build/<pkg>-<16hex>/{output,root-output}`;
//! - new (nightly): units in `build/<pkg>/<16hex>/` with `fingerprint/`, `out/`, `run/{stdout,root-output}`.

use crate::locks;
use crate::store::{Record, Store};
use crate::util;
use crate::workspace::{self, RealCargo, Workspace};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const CARGO_LOCK: &str = ".cargo-build-lock";

#[derive(Debug)]
pub struct Seeded {
    pub donor: PathBuf,
    pub donor_workspace: Option<PathBuf>,
    pub secs: f64,
    /// What the fix-up did to the clone.
    pub fixup: Fixup,
}

/// Counters from [`fixup`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Fixup {
    /// Profile dirs dropped because their Cargo lock was not held during the clone.
    pub profiles_removed: usize,
    /// Build-script output files whose donor paths were rewritten (mtimes kept).
    pub outputs_rewritten: usize,
    /// `bin-*`/`test-*`/`bench-*`/`example-*` fingerprint files deleted (forces relink).
    pub link_fps_removed: usize,
    /// Fingerprint dirs of editable packages and their reverse-dep closure deleted.
    pub fingerprints_invalidated: usize,
}

/// Seeds `target` (missing, or an unpopulated skeleton left by eviction) for `ws`. Donors: index
/// records with the same project key, populated and idle (all Cargo locks free), preferring `prefer_workspace` matches, then same
/// toolchain, then most recent. `Ok(None)` when there is no usable donor (caller builds cold).
/// On any failure after cloning, the partial clone is removed and the error returned; the
/// caller then builds cold. Never leaves a half-fixed build-dir at `target`.
pub fn seed(store: &Store, cargo: &RealCargo, ws: &Workspace, target: &Path, prefer_workspace: bool) -> io::Result<Option<Seeded>> {
    // ADR-002: no copying or hardlink fallback when CoW is unavailable.
    if !util::cow_available(&store.root)? { return Ok(None); }
    let start = Instant::now();
    let Some(project) = &ws.project else { return Ok(None) };
    if !crate::store::is_unpopulated(target) {
        return Ok(None);
    }
    let mut cands: Vec<Record> = store
        .records()
        .into_iter()
        .filter(|r| {
            r.project.as_ref() == Some(project)
                && r.build_dir != target
                && r.build_dir.is_dir()
                && !crate::store::is_unpopulated(&r.build_dir)
        })
        .collect();
    if cands.is_empty() {
        return Ok(None);
    }
    let toolchain = workspace::toolchain_id(cargo, &ws.root);
    order_candidates(&mut cands, prefer_workspace.then_some(ws.root.as_path()), toolchain.as_deref());

    // Before touching any donor: without this set the seeded build could be silently stale,
    // so failing to compute it aborts seeding.
    let names = local_closure(&cargo_metadata(cargo, ws, Some(&store.tmp_dir().join("metadata")))?)?;

    for rec in cands {
        let idle = match locks::try_lock_idle(&rec.build_dir) {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => continue,
        };
        let tmp_root = store.tmp_dir();
        std::fs::create_dir_all(&tmp_root)?;
        let tmp = tmp_root.join(unique_name());
        let held = idle.cargo_rel(&rec.build_dir);
        let cloned = util::clonefile(&rec.build_dir, &tmp);
        drop(idle);
        let result = cloned
            .and_then(|()| fixup(&tmp, &rec.build_dir, target, &held, &names))
            .and_then(|fx| install(&tmp, target).map(|won| won.then_some(fx)));
        return match result {
            Ok(Some(fixup)) => Ok(Some(Seeded {
                donor: rec.build_dir,
                donor_workspace: rec.workspace_root,
                secs: start.elapsed().as_secs_f64(),
                fixup,
            })),
            Ok(None) => {
                let _ = util::remove_dir_all_retry(&tmp);
                Ok(None)
            }
            Err(e) => {
                let _ = util::remove_dir_all_retry(&tmp);
                Err(e)
            }
        };
    }
    Ok(None)
}

/// Puts the fixed-up clone at `target`. A missing target is a single rename. An unpopulated
/// skeleton is filled entry by entry under its Cargo locks, keeping its lock files (and their
/// inodes) in place. `Ok(false)` if another process got there first or the skeleton is in use.
fn install(tmp: &Path, target: &Path) -> io::Result<bool> {
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    if !target.exists() {
        return match std::fs::rename(tmp, target) {
            Ok(()) => Ok(true),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ENOTEMPTY)) => Ok(false),
            Err(e) => Err(e),
        };
    }
    let Some(_held) = locks::try_lock_idle(target)? else { return Ok(false) };
    if !crate::store::is_unpopulated(target) {
        return Ok(false);
    }
    merge_into(tmp, target)?;
    Ok(true)
}

/// Moves every entry of `src` into `dst`, descending into directories both have; files that
/// already exist in `dst` (skeleton lock files, `.rustc_info.json`) are kept.
fn merge_into(src: &Path, dst: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        match std::fs::symlink_metadata(&to) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => std::fs::rename(e.path(), &to)?,
            Err(err) => return Err(err),
            Ok(m) if m.is_dir() && e.file_type()?.is_dir() => merge_into(&e.path(), &to)?,
            Ok(_) => {}
        }
    }
    Ok(())
}

fn unique_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("seed-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// Donor order: `prefer_root` matches first, then same toolchain, then most recently used.
pub fn order_candidates(cands: &mut [Record], prefer_root: Option<&Path>, toolchain: Option<&str>) {
    cands.sort_by_key(|r| {
        let ws_match = prefer_root.is_some() && r.workspace_root.as_deref() == prefer_root;
        let tc_match = toolchain.is_some() && r.toolchain.as_deref() == toolchain;
        (std::cmp::Reverse(ws_match), std::cmp::Reverse(tc_match), std::cmp::Reverse(r.last_used))
    });
}

/// `cargo metadata --format-version 1` for the workspace, offline first. Uses `--locked` when a
/// `Cargo.lock` exists (never modifies it). Without one, the lock file Cargo generates is removed
/// again so a later `cargo build --locked` behaves exactly as without nostromo.
/// `scratch_build_dir` keeps Cargo's `.rustc_info.json` out of the build-dir being seeded.
pub fn cargo_metadata(cargo: &RealCargo, ws: &Workspace, scratch_build_dir: Option<&Path>) -> io::Result<serde_json::Value> {
    let lock = ws.root.join("Cargo.lock");
    let had_lock = lock.exists();
    let attempts: [&[&str]; 2] = if had_lock { [&["--locked", "--offline"], &["--locked"]] } else { [&["--offline"], &[]] };
    let mut last_err = String::new();
    let mut found = None;
    for extra in attempts {
        let mut c = cargo.command();
        c.current_dir(&ws.root)
            .args(["metadata", "--format-version", "1"])
            .args(extra)
            .arg("--manifest-path")
            .arg(&ws.manifest)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = scratch_build_dir {
            c.env("CARGO_BUILD_BUILD_DIR", d);
        }
        let out = util::output_with_timeout(c, std::time::Duration::from_secs(120))?;
        if out.status.success() {
            found = Some(out.stdout);
            break;
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    if !had_lock && lock.exists() {
        let _ = std::fs::remove_file(&lock);
    }
    let stdout = found.ok_or_else(|| io::Error::other(format!("cargo metadata failed: {last_err}")))?;
    serde_json::from_slice(&stdout).map_err(io::Error::other)
}

/// Names of every package whose fingerprints must go: packages with editable sources
/// (`source == null`), closed over reverse dependencies and over packages sharing a name with a
/// member (fingerprint dirs are keyed by name), to a fixpoint. Closed under reverse deps, so no
/// kept unit depends on a rebuilt one (no E0460).
pub fn local_closure(md: &serde_json::Value) -> io::Result<HashSet<String>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("cargo metadata: {m}"));
    let pkgs = md.get("packages").and_then(|p| p.as_array()).ok_or_else(|| bad("no packages"))?;
    let nodes = md.pointer("/resolve/nodes").and_then(|n| n.as_array()).ok_or_else(|| bad("no resolve"))?;
    let mut name_of: HashMap<&str, &str> = HashMap::new();
    let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut todo: Vec<&str> = Vec::new();
    for p in pkgs {
        let id = p.get("id").and_then(|v| v.as_str()).ok_or_else(|| bad("package without id"))?;
        let name = p.get("name").and_then(|v| v.as_str()).ok_or_else(|| bad("package without name"))?;
        name_of.insert(id, name);
        by_name.entry(name).or_default().push(id);
        if p.get("source").is_none_or(|s| s.is_null()) {
            todo.push(id);
        }
    }
    let mut rdeps: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes {
        let id = n.get("id").and_then(|v| v.as_str()).ok_or_else(|| bad("node without id"))?;
        for d in n.get("deps").and_then(|d| d.as_array()).map(Vec::as_slice).unwrap_or_default() {
            if let Some(pkg) = d.get("pkg").and_then(|v| v.as_str()) {
                rdeps.entry(pkg).or_default().push(id);
            }
        }
        // Older formats only list `dependencies` (ids); treat them the same.
        for d in n.get("dependencies").and_then(|d| d.as_array()).map(Vec::as_slice).unwrap_or_default() {
            if let Some(pkg) = d.as_str() {
                rdeps.entry(pkg).or_default().push(id);
            }
        }
    }
    let mut set: HashSet<&str> = HashSet::new();
    while let Some(id) = todo.pop() {
        if !set.insert(id) {
            continue;
        }
        todo.extend(rdeps.get(id).into_iter().flatten().copied());
        if let Some(name) = name_of.get(id) {
            todo.extend(by_name.get(name).into_iter().flatten().copied());
        }
    }
    Ok(set.iter().filter_map(|id| name_of.get(id)).map(|n| n.to_string()).collect())
}

/// Fix-up of a fresh clone of `donor` that will be renamed to `target`. `held` = Cargo lock paths
/// (relative to the build-dir) held while cloning; `invalidate` = [`local_closure`] names.
pub fn fixup(clone: &Path, donor: &Path, target: &Path, held: &[PathBuf], invalidate: &HashSet<String>) -> io::Result<Fixup> {
    let mut fx = Fixup::default();
    let (from, to) = (donor.as_os_str().as_encoded_bytes(), target.as_os_str().as_encoded_bytes());
    for (rel, dir) in profile_dirs(clone) {
        // A profile dir whose lock we did not hold may have been written mid-clone.
        if !held.iter().any(|h| *h == rel.join(CARGO_LOCK)) {
            util::remove_dir_all_retry(&dir)?;
            fx.profiles_removed += 1;
            continue;
        }
        fix_profile(&dir, from, to, invalidate, &mut fx)?;
    }
    Ok(fx)
}

fn is_profile_dir(d: &Path) -> bool {
    [CARGO_LOCK, ".fingerprint", "build", "deps"].iter().any(|n| d.join(n).exists())
}

/// `(relative path, absolute path)` of `<bd>/<profile>` and `<bd>/<triple>/<profile>` dirs.
fn profile_dirs(bd: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for d1 in subdirs(bd) {
        let n1 = PathBuf::from(d1.file_name().unwrap_or_default());
        if is_profile_dir(&d1) {
            out.push((n1, d1));
            continue;
        }
        for d2 in subdirs(&d1) {
            if is_profile_dir(&d2) {
                out.push((n1.join(d2.file_name().unwrap_or_default()), d2));
            }
        }
    }
    out
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    let mut v: Vec<PathBuf> =
        rd.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.path()).collect();
    v.sort();
    v
}

fn is_hash16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `serde_json-0123456789abcdef` -> `serde_json`.
fn split_hashed(s: &str) -> Option<&str> {
    let (name, hash) = s.rsplit_once('-')?;
    is_hash16(hash).then_some(name)
}

fn is_link_fp(file: &str) -> bool {
    ["bin-", "test-", "bench-", "example-"].iter().any(|p| file.starts_with(p))
}

fn file_name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn remove_link_fps(fp_dir: &Path, fx: &mut Fixup) -> io::Result<()> {
    let Ok(rd) = std::fs::read_dir(fp_dir) else { return Ok(()) };
    for e in rd.flatten() {
        let name = e.file_name();
        if is_link_fp(&name.to_string_lossy()) && e.file_type()?.is_file() {
            std::fs::remove_file(e.path())?;
            fx.link_fps_removed += 1;
        }
    }
    Ok(())
}

fn fix_profile(dir: &Path, from: &[u8], to: &[u8], invalidate: &HashSet<String>, fx: &mut Fixup) -> io::Result<()> {
    // Old layout fingerprints.
    for fp in subdirs(&dir.join(".fingerprint")) {
        if split_hashed(file_name(&fp)).is_some_and(|n| invalidate.contains(n)) {
            util::remove_dir_all_retry(&fp)?;
            fx.fingerprints_invalidated += 1;
        } else {
            remove_link_fps(&fp, fx)?;
        }
    }
    for entry in subdirs(&dir.join("build")) {
        // Old layout: build/<pkg>-<hash>/{output,root-output}.
        for f in ["output", "root-output"] {
            if rewrite_keep_times(&entry.join(f), from, to)? {
                fx.outputs_rewritten += 1;
            }
        }
        // New layout: build/<pkg>/<hash>/{fingerprint,out,run}.
        let pkg = file_name(&entry).to_string();
        for unit in subdirs(&entry) {
            if !is_hash16(file_name(&unit)) {
                continue;
            }
            for f in ["run/stdout", "run/root-output"] {
                if rewrite_keep_times(&unit.join(f), from, to)? {
                    fx.outputs_rewritten += 1;
                }
            }
            let fp = unit.join("fingerprint");
            if !fp.is_dir() {
                continue;
            }
            if invalidate.contains(&pkg) {
                util::remove_dir_all_retry(&fp)?;
                fx.fingerprints_invalidated += 1;
            } else {
                remove_link_fps(&fp, fx)?;
            }
        }
    }
    Ok(())
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')
}

/// Replaces whole-path occurrences of `from` (not when embedded in a longer path component).
pub fn replace_path_bytes(data: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    if from.is_empty() || data.len() < from.len() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut i, mut last, mut hit) = (0, 0, false);
    while i + from.len() <= data.len() {
        if &data[i..i + from.len()] == from {
            let before_ok = i == 0 || !is_path_byte(data[i - 1]);
            let after = data.get(i + from.len()).copied();
            let after_ok = after.is_none_or(|b| b == b'/' || !is_path_byte(b));
            if before_ok && after_ok {
                out.extend_from_slice(&data[last..i]);
                out.extend_from_slice(to);
                i += from.len();
                last = i;
                hit = true;
                continue;
            }
        }
        i += 1;
    }
    if !hit {
        return None;
    }
    out.extend_from_slice(&data[last..]);
    Some(out)
}

/// Rewrites donor paths in `p`, restoring atime/mtime afterwards: a changed mtime makes Cargo
/// rerun the build script, and `-sys` crates then rebuild. `Ok(false)` if absent or unchanged.
fn rewrite_keep_times(p: &Path, from: &[u8], to: &[u8]) -> io::Result<bool> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let data = std::fs::read(p)?;
    let Some(new) = replace_path_bytes(&data, from, to) else { return Ok(false) };
    let times = std::fs::FileTimes::new().set_accessed(meta.accessed()?).set_modified(meta.modified()?);
    let f = std::fs::OpenOptions::new().write(true).truncate(true).open(p)?;
    io::Write::write_all(&mut &f, &new)?;
    f.set_times(times)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nostromo-seed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn set_old_times(p: &Path) -> (SystemTime, SystemTime) {
        let a = SystemTime::UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789);
        let m = SystemTime::UNIX_EPOCH + Duration::new(1_650_000_000, 987_654_321);
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(a).set_modified(m)).unwrap();
        (a, m)
    }

    fn names(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const H1: &str = "0123456789abcdef";
    const H2: &str = "fedcba9876543210";

    #[test]
    fn replace_respects_path_boundaries() {
        let r = |d: &str| replace_path_bytes(d.as_bytes(), b"/s/p/ab", b"/s/p/cd").map(|v| String::from_utf8(v).unwrap());
        assert_eq!(r("cargo:x=/s/p/ab/debug/out\n").as_deref(), Some("cargo:x=/s/p/cd/debug/out\n"));
        assert_eq!(r("/s/p/ab").as_deref(), Some("/s/p/cd"));
        assert_eq!(r("a=/s/p/ab:/s/p/ab/x").as_deref(), Some("a=/s/p/cd:/s/p/cd/x"));
        assert_eq!(r("/s/p/abc/x"), None);
        assert_eq!(r("/s/p/ab.nostromo-used"), None);
        assert_eq!(r("/x/s/p/ab/y"), None);
        assert_eq!(r("nothing"), None);
    }

    #[test]
    fn closure_over_rdeps_and_names() {
        // ws members: app, mylib (path). Registry: serde, log (two versions), uses-mylib depends on
        // mylib via [patch]-like edge. Registry `log` shares a name with a path package `log`.
        let md = serde_json::json!({
            "packages": [
                {"id": "app", "name": "app", "source": null},
                {"id": "mylib", "name": "mylib", "source": null},
                {"id": "log-path", "name": "log", "source": null},
                {"id": "log-reg", "name": "log", "source": "registry+https://github.com/rust-lang/crates.io-index"},
                {"id": "env_logger", "name": "env_logger", "source": "registry+x"},
                {"id": "uses-mylib", "name": "uses-mylib", "source": "registry+x"},
                {"id": "top", "name": "top", "source": "registry+x"},
                {"id": "serde", "name": "serde", "source": "registry+x"},
                {"id": "regex", "name": "regex", "source": "registry+x"}
            ],
            "resolve": {"nodes": [
                {"id": "app", "deps": [{"pkg": "mylib"}, {"pkg": "serde"}, {"pkg": "regex"}]},
                {"id": "mylib", "deps": [{"pkg": "serde"}]},
                {"id": "uses-mylib", "deps": [{"pkg": "mylib"}]},
                {"id": "top", "deps": [{"pkg": "uses-mylib"}]},
                {"id": "env_logger", "deps": [{"pkg": "log-reg"}]},
                {"id": "log-reg", "deps": []},
                {"id": "log-path", "deps": []},
                {"id": "serde", "deps": []},
                {"id": "regex", "deps": []}
            ]}
        });
        let got = local_closure(&md).unwrap();
        // log-path -> name "log" pulls log-reg -> its rdep env_logger.
        assert_eq!(got, names(&["app", "mylib", "log", "uses-mylib", "top", "env_logger"]));
    }

    /// Old-layout synthetic build-dir.
    fn old_tree(bd: &Path, donor: &Path) {
        let p = bd.join("debug");
        put(&p.join(CARGO_LOCK), "");
        for pkg in ["app", "serde", "log", "env_logger", "regex", "native"] {
            put(&p.join(format!(".fingerprint/{pkg}-{H1}/lib-{pkg}")), "x");
            put(&p.join(format!(".fingerprint/{pkg}-{H1}/dep-lib-{pkg}")), "x");
        }
        put(&p.join(format!(".fingerprint/app-{H2}/bin-app")), "x");
        put(&p.join(format!(".fingerprint/app-{H2}/bin-app.json")), "x");
        put(&p.join(format!(".fingerprint/app-{H2}/dep-bin-app")), "x");
        put(&p.join(format!(".fingerprint/regex-{H2}/test-lib-regex")), "x");
        put(&p.join(format!(".fingerprint/regex-{H2}/dep-test-lib-regex")), "x");
        put(&p.join(format!(".fingerprint/regex-{H2}/example-foo")), "x");
        put(&p.join(format!(".fingerprint/regex-{H2}/bench-bar")), "x");
        let d = donor.display();
        put(&p.join(format!("build/native-{H1}/output")), &format!("cargo:rustc-link-search=native={d}/debug/build/native-{H1}/out\n"));
        put(&p.join(format!("build/native-{H1}/root-output")), &format!("{d}/debug/build/native-{H1}/out"));
        put(&p.join(format!("build/serde-{H1}/output")), "cargo:rustc-cfg=foo\n");
        put(&p.join("deps/libserde.rlib"), "rlib");
    }

    /// New-layout synthetic build-dir.
    fn new_tree(bd: &Path, donor: &Path) {
        let p = bd.join("aarch64-apple-darwin/release");
        put(&p.join(CARGO_LOCK), "");
        for pkg in ["app", "serde", "log", "env_logger", "regex", "native"] {
            put(&p.join(format!("build/{pkg}/{H1}/fingerprint/lib-{pkg}.json")), "x");
            put(&p.join(format!("build/{pkg}/{H1}/fingerprint/dep-lib-{pkg}")), "x");
            put(&p.join(format!("build/{pkg}/{H1}/out/lib{pkg}.rlib")), "x");
        }
        put(&p.join(format!("build/app/{H2}/fingerprint/bin-app")), "x");
        put(&p.join(format!("build/app/{H2}/fingerprint/dep-bin-app")), "x");
        put(&p.join(format!("build/regex/{H2}/fingerprint/test-lib-regex")), "x");
        put(&p.join(format!("build/regex/{H2}/fingerprint/dep-test-lib-regex")), "x");
        let d = donor.display();
        let out = format!("{d}/aarch64-apple-darwin/release/build/native/{H2}/out");
        put(&p.join(format!("build/native/{H2}/run/stdout")), &format!("cargo:include={out}\n"));
        put(&p.join(format!("build/native/{H2}/run/root-output")), &out);
        put(&p.join(format!("build/native/{H2}/fingerprint/run-build-script-build-script-build")), "x");
    }

    #[test]
    fn fixup_both_layouts() {
        let root = tmp("both");
        let donor = root.join("store/build/p/aa/donorhash0000000");
        let target = root.join("store/build/p/bb/targethash000000");
        let clone = root.join("clone");
        old_tree(&clone, &donor);
        new_tree(&clone, &donor);
        // Old-layout output file gets an old mtime; it must survive the rewrite.
        let out_old = clone.join(format!("debug/build/native-{H1}/output"));
        let (a, m) = set_old_times(&out_old);
        let out_new = clone.join(format!("aarch64-apple-darwin/release/build/native/{H2}/run/stdout"));
        let (_, m2) = set_old_times(&out_new);
        // Profile dir created mid-clone (lock not held), plus one without any lock file.
        put(&clone.join(format!("release/.fingerprint/app-{H1}/lib-app")), "x");
        put(&clone.join("release").join(CARGO_LOCK), "");
        put(&clone.join("x86_64-apple-darwin/debug/build/foo/x"), "x");
        // Non-profile entries survive.
        put(&clone.join(".rustc_info.json"), "{}");
        put(&clone.join("CACHEDIR.TAG"), "x");

        let held = vec![PathBuf::from("debug").join(CARGO_LOCK), PathBuf::from("aarch64-apple-darwin/release").join(CARGO_LOCK)];
        // Closure contains app + "log" (path package colliding with registry log) + env_logger.
        let inv = names(&["app", "log", "env_logger"]);
        let fx = fixup(&clone, &donor, &target, &held, &inv).unwrap();

        // Unlocked profile dirs removed.
        assert!(!clone.join("release").exists());
        assert!(!clone.join("x86_64-apple-darwin/debug").exists());
        assert!(clone.join(".rustc_info.json").exists() && clone.join("CACHEDIR.TAG").exists());
        assert_eq!(fx.profiles_removed, 2);

        // Old layout.
        let p = clone.join("debug");
        let t = target.display();
        // Check times before reading (a read may bump atime).
        let md = std::fs::metadata(&out_old).unwrap();
        assert_eq!(md.modified().unwrap(), m);
        assert_eq!(md.accessed().unwrap(), a);
        let s = std::fs::read_to_string(&out_old).unwrap();
        assert_eq!(s, format!("cargo:rustc-link-search=native={t}/debug/build/native-{H1}/out\n"));
        assert_eq!(std::fs::read_to_string(p.join(format!("build/native-{H1}/root-output"))).unwrap(), format!("{t}/debug/build/native-{H1}/out"));
        assert_eq!(std::fs::read_to_string(p.join(format!("build/serde-{H1}/output"))).unwrap(), "cargo:rustc-cfg=foo\n");
        let fps: HashSet<String> =
            std::fs::read_dir(p.join(".fingerprint")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        let want: HashSet<String> =
            [format!("serde-{H1}"), format!("regex-{H1}"), format!("native-{H1}"), format!("regex-{H2}")].into_iter().collect();
        assert_eq!(fps, want, "only closure packages invalidated (incl. registry name collision)");
        let rg2: HashSet<String> = std::fs::read_dir(p.join(format!(".fingerprint/regex-{H2}")))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(rg2, names(&["dep-test-lib-regex"]), "link fps removed, dep- kept");
        assert!(p.join(format!(".fingerprint/serde-{H1}/lib-serde")).exists());
        assert!(p.join("deps/libserde.rlib").exists());

        // New layout.
        let q = clone.join("aarch64-apple-darwin/release");
        let s = std::fs::read_to_string(&out_new).unwrap();
        assert_eq!(s, format!("cargo:include={t}/aarch64-apple-darwin/release/build/native/{H2}/out\n"));
        assert_eq!(std::fs::metadata(&out_new).unwrap().modified().unwrap(), m2);
        assert!(std::fs::read_to_string(q.join(format!("build/native/{H2}/run/root-output"))).unwrap().starts_with(&t.to_string()));
        for gone in ["app", "log", "env_logger"] {
            assert!(!q.join(format!("build/{gone}/{H1}/fingerprint")).exists(), "{gone}");
            assert!(q.join(format!("build/{gone}/{H1}/out/lib{gone}.rlib")).exists(), "artifacts stay");
        }
        assert!(!q.join(format!("build/app/{H2}/fingerprint")).exists());
        for kept in ["serde", "regex", "native"] {
            assert!(q.join(format!("build/{kept}/{H1}/fingerprint/lib-{kept}.json")).exists(), "{kept}");
        }
        assert!(!q.join(format!("build/regex/{H2}/fingerprint/test-lib-regex")).exists());
        assert!(q.join(format!("build/regex/{H2}/fingerprint/dep-test-lib-regex")).exists());
        assert!(q.join(format!("build/native/{H2}/fingerprint/run-build-script-build-script-build")).exists());

        assert_eq!(fx.outputs_rewritten, 4);
        // old: app-H1, log-H1, env_logger-H1, app-H2 dirs; new: same four fingerprint dirs.
        assert_eq!(fx.fingerprints_invalidated, 8);
        // old: test-lib-regex, example-foo, bench-bar; new: test-lib-regex.
        assert_eq!(fx.link_fps_removed, 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn link_fps_removed_outside_closure() {
        let root = tmp("link");
        let donor = root.join("d");
        let clone = root.join("c");
        old_tree(&clone, &donor);
        let fx = fixup(&clone, &donor, &root.join("t"), &[PathBuf::from("debug").join(CARGO_LOCK)], &HashSet::new()).unwrap();
        let fp = clone.join(format!("debug/.fingerprint/app-{H2}"));
        assert!(!fp.join("bin-app").exists() && !fp.join("bin-app.json").exists());
        assert!(fp.join("dep-bin-app").exists());
        assert_eq!(fx.fingerprints_invalidated, 0);
        assert_eq!(fx.link_fps_removed, 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn donor_order() {
        let rec = |name: &str, ws: &str, tc: &str, last: u64| {
            let mut r = Record::new(PathBuf::from(name));
            r.workspace_root = Some(PathBuf::from(ws));
            r.toolchain = Some(tc.to_string());
            r.last_used = last;
            r
        };
        let mut v = vec![rec("a", "/w1", "old", 50), rec("b", "/w2", "cur", 10), rec("c", "/w2", "cur", 20), rec("d", "/me", "old", 1)];
        order_candidates(&mut v, None, Some("cur"));
        let order: Vec<_> = v.iter().map(|r| r.build_dir.display().to_string()).collect();
        assert_eq!(order, ["c", "b", "a", "d"]);
        order_candidates(&mut v, Some(Path::new("/me")), Some("cur"));
        assert_eq!(v[0].build_dir, PathBuf::from("d"));
        order_candidates(&mut v, None, None);
        assert_eq!(v[0].build_dir, PathBuf::from("a"));
    }
}
