#![cfg(target_os = "macos")]
//! Seeding correctness against the synthetic workspace (older-source, forward, busy donor).
//! Requires APFS; the optional layout test also requires a nightly toolchain.

#[path = "common/fixture.rs"]
mod fixture;

use nostromo::locks::{self, Mode};
use nostromo::seed;
use nostromo::store::{Record, Store};
use nostromo::workspace::{self, RealCargo, Workspace};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const WS_CRATES: [&str; 5] = ["app", "consumer", "gen", "native", "pm"];

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=nostromo-test", "-c", "user.email=test@invalid"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Everything a test creates; removed on drop, even after a panic.
struct Lab {
    dir: PathBuf,
    fixture: PathBuf,
    worktrees: Vec<PathBuf>,
    branches: Vec<String>,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let dir = fixture::test_root(&format!("seeding-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = fixture::create(&dir.join("fixture"));
        Lab { dir, fixture, worktrees: Vec::new(), branches: Vec::new() }
    }

    fn worktree(&mut self, name: &str, branch: Option<&str>) -> PathBuf {
        let wt = self.dir.join(name);
        let ws = wt.to_str().unwrap();
        match branch {
            Some(b) => {
                git(&self.fixture, &["worktree", "add", "-q", "-b", b, ws, "HEAD"]);
                self.branches.push(b.to_string());
            }
            None => {
                git(&self.fixture, &["worktree", "add", "-q", "--detach", ws, "HEAD"]);
            }
        }
        self.worktrees.push(wt.clone());
        wt.canonicalize().unwrap()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let fx = &self.fixture;
        for wt in &self.worktrees {
            let _ = Command::new("git").arg("-C").arg(fx).args(["worktree", "remove", "--force"]).arg(wt).output();
        }
        for b in &self.branches {
            let _ = Command::new("git").arg("-C").arg(fx).args(["branch", "-D", b]).output();
        }
        let _ = Command::new("git").arg("-C").arg(fx).args(["worktree", "prune"]).output();
        let _ = nostromo::util::remove_dir_all_retry(&self.dir);
    }
}

fn proxy() -> PathBuf {
    let p = nostromo::util::home().join(".cargo/bin/cargo");
    if p.exists() { p } else { PathBuf::from("cargo") }
}

fn real_cargo(tc: Option<&str>) -> RealCargo {
    RealCargo { path: proxy(), toolchain: tc.map(str::to_string) }
}

/// `cargo [+tc] build --offline -v` with an explicit build-dir and target-dir; returns stderr.
fn build(tc: Option<&str>, ws: &Path, bd: &Path, td: &Path) -> String {
    let mut c = Command::new(proxy());
    if let Some(t) = tc {
        c.arg(format!("+{t}"));
    }
    let out = c
        .args(["build", "--offline", "-v"])
        .current_dir(ws)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("CARGO_BUILD_BUILD_DIR", bd)
        .env("CARGO_TARGET_DIR", td)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "build of {} failed:\n{err}", ws.display());
    err
}

fn compiling(stderr: &str) -> Vec<String> {
    stderr.lines().filter_map(|l| l.trim_start().strip_prefix("Compiling ")).map(|s| s.split_whitespace().next().unwrap().to_string()).collect()
}

fn run_app(td: &Path) -> String {
    let out = Command::new(td.join("debug/app")).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn locate(cargo: &RealCargo, root: &Path) -> Workspace {
    let ws = workspace::locate(cargo, root, None).expect("workspace");
    assert!(ws.project.is_some());
    ws
}

fn save_record(store: &Store, cargo: &RealCargo, bd: &Path, root: &Path, last_used: u64) {
    let mut r = Record::new(bd.to_path_buf());
    r.workspace_root = Some(root.to_path_buf());
    r.project = workspace::project_key(root).1;
    r.toolchain = workspace::toolchain_id(cargo, root);
    r.last_used = last_used;
    store.save_record(&r).unwrap();
}

fn tmp_is_empty(store: &Store) -> bool {
    std::fs::read_dir(store.tmp_dir()).map(|mut rd| rd.next().is_none()).unwrap_or(true)
}

fn edit(p: &Path, from: &str, to: &str) {
    let s = std::fs::read_to_string(p).unwrap();
    assert!(s.contains(from), "{} lacks {from}", p.display());
    std::fs::write(p, s.replace(from, to)).unwrap();
}

struct EditableSourceCase {
    store: Store,
    donor_b: PathBuf,
    main_bd: PathBuf,
    cold_out: String,
}

/// Case 1: a worktree at commit B (changed proc-macro and lib) is the donor; the main checkout
/// (older files, commit A) is seeded from it and must build A's code, not B's.
fn older_source_case(lab: &mut Lab, tc: Option<&str>) -> EditableSourceCase {
    let cargo = real_cargo(tc);
    let store = Store::new(lab.dir.join("store"));
    store.ensure().unwrap();
    let fx = lab.fixture.clone();
    let lock = fx.join("Cargo.lock");

    let branch = format!("nostromo-seed-b-{}-{}", std::process::id(), tc.unwrap_or("stable"));
    let wt_b = lab.worktree("wt-b", Some(&branch));
    edit(&wt_b.join("crates/pm/src/lib.rs"), "stringify!({name})", r#"concat!(stringify!({name}), \"-B\")"#);
    edit(&wt_b.join("crates/app/src/lib.rs"), "{ 1 }", "{ 1000 }");
    git(&wt_b, &["commit", "-q", "-am", "B"]);
    std::fs::copy(&lock, wt_b.join("Cargo.lock")).unwrap();

    let donor_b = store.build_root().join("aa/donor-b");
    let td_b = lab.dir.join("t-b");
    build(tc, &wt_b, &donor_b, &td_b);
    let out_b = run_app(&td_b);
    println!("[{tc:?}] donor (B) output: {out_b}");
    assert!(out_b.contains("hello=Thing-B"));
    save_record(&store, &cargo, &donor_b, &wt_b, 1000);

    let td_cold = lab.dir.join("t-cold");
    build(tc, &fx, &lab.dir.join("cold-bd"), &td_cold);
    let cold_out = run_app(&td_cold);
    println!("[{tc:?}] cold main output: {cold_out}");
    assert!(cold_out.contains("hello=Thing "));

    // Control: the same clone with every fix-up rule except editable-source invalidation is silently stale,
    // so this fixture really exercises the bug.
    let ctl = lab.dir.join("control-bd");
    nostromo::util::clonefile(&donor_b, &ctl).unwrap();
    seed::fixup(&ctl, &donor_b, &ctl, &[PathBuf::from("debug/.cargo-build-lock")], &Default::default()).unwrap();
    let td_ctl = lab.dir.join("t-control");
    build(tc, &fx, &ctl, &td_ctl);
    let ctl_out = run_app(&td_ctl);
    println!("[{tc:?}] control (no editable-source invalidation) output: {ctl_out}");
    assert!(ctl_out.contains("hello=Thing-B"), "control expected to be stale");
    let _ = nostromo::util::remove_dir_all_retry(&ctl);
    let _ = nostromo::util::remove_dir_all_retry(&td_ctl);

    let main_bd = store.build_root().join("bb/main");
    let ws_main = locate(&cargo, &fx);
    let s = seed::seed(&store, &cargo, &ws_main, &main_bd, false).unwrap().expect("seeded from B");
    println!("[{tc:?}] seeded main from {} in {:.3}s: {:?}", s.donor.display(), s.secs, s.fixup);
    assert_eq!(s.donor, donor_b);
    assert_eq!(s.donor_workspace.as_deref(), Some(wt_b.as_path()));
    assert!(s.fixup.fingerprints_invalidated > 0);
    assert!(main_bd.is_dir() && tmp_is_empty(&store));

    let td_main = lab.dir.join("t-main");
    let err = build(tc, &fx, &main_bd, &td_main);
    println!("[{tc:?}] seeded main build compiled: {:?}", compiling(&err));
    let out = run_app(&td_main);
    println!("[{tc:?}] seeded main output: {out}");
    assert_eq!(out, cold_out, "seeded build must equal cold build");
    let noop = compiling(&build(tc, &fx, &main_bd, &td_main));
    assert!(noop.is_empty(), "no-op rebuild compiled {noop:?}");
    println!("[{tc:?}] no-op rebuild compiled 0 crates");
    EditableSourceCase { store, donor_b, main_bd, cold_out }
}

#[test]
#[ignore = "slower APFS seeding correctness oracle; run with --ignored"]
fn seed_lab_stable() {
    let mut lab = Lab::new("stable");
    let cargo = real_cargo(None);
    let EditableSourceCase { store, donor_b, main_bd, cold_out } = older_source_case(&mut lab, None);
    let fx = lab.fixture.clone();

    // Case 2: forward. A fresh worktree at main's commit, seeded from main's build-dir.
    save_record(&store, &cargo, &main_bd, &fx, 2000);
    let wt_c = lab.worktree("wt-c", None);
    let ws_c = locate(&cargo, &wt_c);
    let c_bd = store.build_root().join("cc/wt-c");
    let s = seed::seed(&store, &cargo, &ws_c, &c_bd, false).unwrap().expect("seeded from main");
    println!("[forward] seeded from {} in {:.3}s: {:?}", s.donor.display(), s.secs, s.fixup);
    assert_eq!(s.donor, main_bd, "most recently used donor wins");
    assert!(!wt_c.join("Cargo.lock").exists(), "metadata fallback must not leave a Cargo.lock behind");
    std::fs::copy(fx.join("Cargo.lock"), wt_c.join("Cargo.lock")).unwrap();
    let td_c = lab.dir.join("t-c");
    let comp = compiling(&build(None, &wt_c, &c_bd, &td_c));
    println!("[forward] compiled {} crates: {comp:?}", comp.len());
    assert!(comp.iter().all(|c| WS_CRATES.contains(&c.as_str())), "only workspace crates rebuild: {comp:?}");
    assert!(comp.len() <= WS_CRATES.len());
    let out = run_app(&td_c);
    assert_eq!(out, cold_out);
    let noop = compiling(&build(None, &wt_c, &c_bd, &td_c));
    assert!(noop.is_empty(), "no-op rebuild compiled {noop:?}");

    // Case 3: busy donors. Hold their Cargo locks, as running builds do.
    let wt_d = lab.worktree("wt-d", None);
    let ws_d = locate(&cargo, &wt_d);
    let d_bd = store.build_root().join("dd/wt-d");
    let hold = |bd: &Path| locks::try_lock(&bd.join("debug/.cargo-build-lock"), Mode::Exclusive, false).unwrap().unwrap();
    let (held_main, held_b, held_c) = (hold(&main_bd), hold(&donor_b), hold(&c_bd));
    assert!(seed::seed(&store, &cargo, &ws_d, &d_bd, false).unwrap().is_none());
    assert!(!d_bd.exists() && tmp_is_empty(&store));
    println!("[busy] all donors busy -> Ok(None)");
    drop(held_b);
    // Free B only: it becomes the donor even though main is more recent.
    let s = seed::seed(&store, &cargo, &ws_d, &d_bd, false).unwrap().expect("seeded from B");
    assert_eq!(s.donor, donor_b);
    println!("[busy] main busy, B idle -> seeded from B");
    drop((held_main, held_c));

    // Target already populated: never overwritten.
    assert!(seed::seed(&store, &cargo, &ws_d, &d_bd, false).unwrap().is_none());
}

#[test]
#[ignore = "requires a nightly toolchain; run with --ignored"]
fn seed_lab_nightly_new_layout() {
    let mut lab = Lab::new("nightly");
    let EditableSourceCase { main_bd, .. } = older_source_case(&mut lab, Some("nightly"));
    // Confirm this really exercised the new layout.
    assert!(!main_bd.join("debug/.fingerprint").exists());
    assert!(std::fs::read_dir(main_bd.join("debug/build/app")).unwrap().flatten().any(|e| e.path().join("fingerprint").is_dir()));
}
