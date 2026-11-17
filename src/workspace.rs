//! Workspace discovery: root, git project key, the build-dir Cargo picks, and the active toolchain.

use crate::store::ProjectKey;
use crate::util;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    /// Canonical directory containing the workspace root `Cargo.toml`.
    pub root: PathBuf,
    /// The workspace root manifest.
    pub manifest: PathBuf,
    /// `git rev-parse --show-toplevel` of the worktree containing the workspace.
    pub worktree_top: Option<PathBuf>,
    pub project: Option<ProjectKey>,
}

/// How to invoke the real cargo for helper queries. `toolchain` is the user's `+toolchain`
/// argument (passed as `RUSTUP_TOOLCHAIN`), so Cargo's version matches the user's command.
#[derive(Debug, Clone)]
pub struct RealCargo {
    pub path: PathBuf,
    pub toolchain: Option<String>,
}

impl RealCargo {
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.path);
        // rustup's proxy dispatches on argv[0].
        std::os::unix::process::CommandExt::arg0(&mut c, "cargo");
        if let Some(t) = &self.toolchain {
            c.env("RUSTUP_TOOLCHAIN", t);
        }
        // Nested cargo runs from the wrapper hook: no wrapper (and so no recursion), same config
        // otherwise, so `build_directory` is the one the outer cargo uses.
        c.env("RUSTC_WRAPPER", "").env("NOSTROMO_ACTIVE", "1");
        c.stdin(Stdio::null());
        c
    }

    /// `rustc` that belongs to this cargo (rustup proxy or toolchain bin dir).
    pub fn rustc(&self) -> PathBuf {
        self.path.parent().map(|d| d.join("rustc")).filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("rustc"))
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `cargo locate-project --workspace` from `cwd` (honouring `--manifest-path`). `None` outside a
/// workspace or if Cargo fails (the wrapper then just runs rustc).
pub fn locate(cargo: &RealCargo, cwd: &Path, manifest_path: Option<&Path>) -> Option<Workspace> {
    let mut c = cargo.command();
    c.current_dir(cwd).args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(m) = manifest_path {
        c.arg("--manifest-path").arg(m);
    }
    let out = c.stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let manifest = manifest.canonicalize().unwrap_or(manifest);
    let root = manifest.parent()?.to_path_buf();
    let (worktree_top, project) = project_key(&root);
    Some(Workspace { root, manifest, worktree_top, project })
}

/// `(worktree top level, project key)` for a workspace root; `(None, None)` outside git.
pub fn project_key(root: &Path) -> (Option<PathBuf>, Option<ProjectKey>) {
    let Some(out) = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel"]) else {
        return (None, None);
    };
    let mut lines = out.lines();
    let (Some(common), Some(top)) = (lines.next(), lines.next()) else { return (None, None) };
    let canon = |s: &str| {
        let p = PathBuf::from(s);
        p.canonicalize().unwrap_or(p)
    };
    let (common, top) = (canon(common), canon(top));
    let rel = root.strip_prefix(&top).map(Path::to_path_buf).unwrap_or_default();
    (Some(top), Some(ProjectKey { common_dir: common, rel }))
}

/// The build-dir Cargo uses for `ws` under the current config and environment, via
/// `cargo metadata --no-deps` (`build_directory`).
pub fn build_dir(cargo: &RealCargo, ws: &Workspace) -> io::Result<PathBuf> {
    let mut c = cargo.command();
    c.current_dir(&ws.root)
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline", "--manifest-path"])
        .arg(&ws.manifest);
    let out = util::output_with_timeout(c, std::time::Duration::from_secs(30))?;
    if !out.status.success() {
        return Err(io::Error::other("cargo metadata failed"));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(io::Error::other)?;
    v.get("build_directory")
        .and_then(|b| b.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("cargo metadata has no build_directory (Cargo too old?)"))
}

/// `<release> <commit-hash> <host>` from `rustc -vV` of the active toolchain, in `cwd` (so
/// directory overrides apply).
pub fn toolchain_id(cargo: &RealCargo, cwd: &Path) -> Option<String> {
    let mut c = Command::new(cargo.rustc());
    c.current_dir(cwd).arg("-vV").stdin(Stdio::null()).stderr(Stdio::null());
    if let Some(t) = &cargo.toolchain {
        c.env("RUSTUP_TOOLCHAIN", t);
    }
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    Some(format!("{} {} {}", field("release:")?, field("commit-hash:").unwrap_or_default(), field("host:").unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_outside_git_is_none() {
        let d = std::env::temp_dir().join(format!("nostromo-ws-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // /tmp is not inside a git repo on a normal machine.
        let (top, key) = project_key(&d);
        if top.is_none() {
            assert!(key.is_none());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
