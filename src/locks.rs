//! `flock(2)` helpers and interop with Cargo's own build-dir locks.
//!
//! Cargo holds `<build-dir>/<profile>/.cargo-build-lock` (and `<triple>/<profile>/...`) for the
//! whole build. A build-dir is idle when `LOCK_EX|LOCK_NB` succeeds on every one of them;
//! eviction and seeding (of a donor) require that.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

/// A held lock. Dropping it closes the file, which releases the lock.
#[derive(Debug)]
pub struct Lock {
    file: File,
    path: PathBuf,
}

fn open(path: &Path, create: bool) -> io::Result<File> {
    if create && let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    OpenOptions::new().read(true).write(true).create(create).truncate(false).open(path)
}

fn flock(fd: RawFd, op: libc::c_int) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(fd, op) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn op(mode: Mode) -> libc::c_int {
    match mode {
        Mode::Shared => libc::LOCK_SH,
        Mode::Exclusive => libc::LOCK_EX,
    }
}

/// Non-blocking. `Ok(None)` if someone else holds a conflicting lock. With `create = false`, a
/// missing file is `Err(NotFound)`.
pub fn try_lock(path: &Path, mode: Mode, create: bool) -> io::Result<Option<Lock>> {
    let file = open(path, create)?;
    match flock(file.as_raw_fd(), op(mode) | libc::LOCK_NB) {
        Ok(()) => Ok(Some(Lock { file, path: path.to_path_buf() })),
        Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Blocking.
pub fn lock(path: &Path, mode: Mode, create: bool) -> io::Result<Lock> {
    let file = open(path, create)?;
    flock(file.as_raw_fd(), op(mode))?;
    Ok(Lock { file, path: path.to_path_buf() })
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Every Cargo build-dir lock: `<bd>/<profile>/.cargo-build-lock` and
/// `<bd>/<triple>/<profile>/.cargo-build-lock`. Same names in the nightly layout.
pub fn cargo_lock_files(build_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(build_dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let l = p.join(".cargo-build-lock");
        if l.is_file() {
            out.push(l);
        }
        if let Ok(rd2) = std::fs::read_dir(&p) {
            for e2 in rd2.flatten() {
                if e2.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let l2 = e2.path().join(".cargo-build-lock");
                    if l2.is_file() {
                        out.push(l2);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// All-or-nothing `LOCK_EX|LOCK_NB` on every Cargo lock of the build-dir.
pub fn try_lock_idle(build_dir: &Path) -> io::Result<Option<IdleLocks>> {
    let mut cargo = Vec::new();
    for p in cargo_lock_files(build_dir) {
        match try_lock(&p, Mode::Exclusive, false) {
            Ok(Some(l)) => cargo.push(l),
            Ok(None) => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Some(IdleLocks { cargo }))
}

#[derive(Debug)]
pub struct IdleLocks {
    pub cargo: Vec<Lock>,
}

impl IdleLocks {
    /// Cargo lock paths held, relative to the build-dir (e.g. `debug/.cargo-build-lock`).
    pub fn cargo_rel(&self, build_dir: &Path) -> Vec<PathBuf> {
        self.cargo.iter().filter_map(|l| l.path().strip_prefix(build_dir).ok().map(Path::to_path_buf)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Other tests spawn processes; a forked child briefly holds copies of our fds until it
    /// execs, so "free" checks may need a moment.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        (0..50).any(|_| f() || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            false
        })
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nostromo-locks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn shared_blocks_exclusive() {
        let d = tmp("sh");
        let p = d.join("l");
        let a = try_lock(&p, Mode::Shared, true).unwrap().unwrap();
        let _b = try_lock(&p, Mode::Shared, true).unwrap().unwrap();
        assert!(try_lock(&p, Mode::Exclusive, true).unwrap().is_none());
        drop(a);
        assert!(try_lock(&p, Mode::Exclusive, true).unwrap().is_none());
        drop(_b);
        assert!(eventually(|| try_lock(&p, Mode::Exclusive, true).unwrap().is_some()));
    }

    #[test]
    fn idle_requires_all_cargo_locks() {
        let d = tmp("idle");
        let bd = d.join("bd");
        std::fs::create_dir_all(bd.join("debug")).unwrap();
        std::fs::create_dir_all(bd.join("aarch64-apple-darwin/release")).unwrap();
        std::fs::write(bd.join("debug/.cargo-build-lock"), b"").unwrap();
        std::fs::write(bd.join("aarch64-apple-darwin/release/.cargo-build-lock"), b"").unwrap();
        assert_eq!(cargo_lock_files(&bd).len(), 2);
        let held = try_lock(&bd.join("aarch64-apple-darwin/release/.cargo-build-lock"), Mode::Exclusive, false).unwrap().unwrap();
        assert!(try_lock_idle(&bd).unwrap().is_none());
        drop(held);
        let mut idle = None;
        assert!(eventually(|| {
            idle = try_lock_idle(&bd).unwrap();
            idle.is_some()
        }));
        let idle = idle.unwrap();
        assert_eq!(idle.cargo_rel(&bd).len(), 2);
    }
}
