//! Small helpers shared by every module: time, hashing, sizes, statfs, atomic writes, deletion.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// FNV-1a 64. Stable across Rust versions (unlike `DefaultHasher`), so ids survive upgrades.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn path_id(p: &Path) -> String {
    format!("{:016x}", fnv64(p.as_os_str().as_bytes()))
}

pub fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[derive(Debug, Clone)]
pub struct FsStat {
    /// Bytes available to unprivileged users (`f_bavail * f_bsize`).
    pub avail: u64,
    /// Volume capacity in bytes.
    pub total: u64,
    pub fstype: String,
    /// Mount point of the volume (`f_mntonname`).
    pub mount: PathBuf,
}

/// `statfs(2)` on `p`, or on its nearest existing ancestor if `p` does not exist yet.
#[cfg(target_os = "macos")]
pub fn statfs(p: &Path) -> io::Result<FsStat> {
    let mut probe = p.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no existing ancestor"));
        }
    }
    let c = cstr(&probe)?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bsize = u64::from(s.f_bsize);
    let cs = |a: &[libc::c_char]| {
        let bytes: Vec<u8> = a.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(FsStat {
        avail: s.f_bavail * bsize,
        total: s.f_blocks * bsize,
        fstype: cs(&s.f_fstypename),
        mount: PathBuf::from(cs(&s.f_mntonname)),
    })
}

/// Linux volume statistics; capacity uses statvfs's fragment size.
#[cfg(target_os = "linux")]
pub fn statfs(p: &Path) -> io::Result<FsStat> {
    use std::os::unix::fs::MetadataExt;
    let mut probe = p.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no existing ancestor"));
        }
    }
    probe = probe.canonicalize()?;
    let c = cstr(&probe)?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let dev = std::fs::metadata(&probe)?.dev();
    let mut mount = probe.clone();
    while let Some(parent) = mount.parent() {
        if std::fs::metadata(parent)?.dev() != dev { break; }
        mount = parent.to_path_buf();
    }
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    let fstype = if unsafe { libc::statfs(c.as_ptr(), &mut fs) } == 0 {
        match fs.f_type as u64 {
            0xef53 => "ext4".to_string(),
            0x9123683e => "btrfs".to_string(),
            0x58465342 => "xfs".to_string(),
            0x01021994 => "tmpfs".to_string(),
            other => format!("linux fs 0x{other:x}"),
        }
    } else { "unknown".to_string() };
    Ok(FsStat { avail: s.f_bavail * s.f_frsize, total: s.f_blocks * s.f_frsize, fstype, mount })
}

/// ADR-002: Linux always uses basic mode; macOS seeding requires APFS.
pub fn cow_available(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    { Ok(statfs(path)?.fstype == "apfs") }
    #[cfg(target_os = "linux")]
    { let _ = path; Ok(false) }
}

#[cfg(target_os = "linux")]
pub fn clonefile(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "CoW seeding is not supported on Linux"))
}

#[cfg(target_os = "linux")]
pub fn proc_args(pid: i32) -> Option<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(bytes.strip_suffix(&[0]).unwrap_or(&bytes).split(|b| *b == 0).map(|s| std::ffi::OsString::from_vec(s.to_vec())).collect())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeSpec {
    Bytes(u64),
    /// Percentage of the store volume's capacity.
    Percent(f64),
}

impl SizeSpec {
    pub fn resolve(self, volume_total: u64) -> u64 {
        match self {
            SizeSpec::Bytes(b) => b,
            SizeSpec::Percent(p) => ((volume_total as f64) * p / 100.0) as u64,
        }
    }
}

/// Parses `25%`, `100G`, `100GB`, `512MiB`, `1.5T`, `1000000` (bytes). Decimal units (GB = 1e9),
/// binary with `i` (GiB = 2^30).
pub fn parse_size(s: &str) -> Result<SizeSpec, String> {
    let t = s.trim();
    if let Some(p) = t.strip_suffix('%') {
        let v: f64 = p.trim().parse().map_err(|_| format!("bad percentage: {s}"))?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("percentage out of range: {s}"));
        }
        return Ok(SizeSpec::Percent(v));
    }
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let v: f64 = num.parse().map_err(|_| format!("bad size: {s}"))?;
    let unit = unit.trim().to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("unknown size unit in {s}")),
    };
    Ok(SizeSpec::Bytes((v * mult) as u64))
}

/// Parses `30s`, `10m`, `2h`, `1d`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let v: u64 = num.parse().map_err(|_| format!("bad duration: {s}"))?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("unknown duration unit in {s}")),
    };
    Ok(Duration::from_secs(v * mult))
}

/// Decimal, one decimal place: `12.3 GB`.
pub fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}

/// Writes via a temp file in the same directory and `rename`, so readers never see a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `remove_dir_all`, retried: Finder/Spotlight can drop a `.DS_Store` into a directory while it is
/// being deleted, which makes the final `rmdir` fail with ENOTEMPTY.
pub fn remove_dir_all_retry(p: &Path) -> io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match std::fs::remove_dir_all(p) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(last.unwrap())
}

/// `clonefile(2)`: APFS copy-on-write clone of a file or a whole directory tree. `dst` must not exist.
#[cfg(target_os = "macos")]
pub fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // CLONE_NOFOLLOW = 1: clone a symlink itself rather than its target.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// argv of process `pid` (argv[0] first), via `sysctl(KERN_PROCARGS2)`. Same-user processes only.
#[cfg(target_os = "macos")]
pub fn proc_args(pid: i32) -> Option<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt;
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 2, (&raw mut argmax).cast(), &mut size, std::ptr::null_mut(), 0) };
    if r != 0 || argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if r != 0 || len < 4 {
        return None;
    }
    buf.truncate(len);
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    // Executable path, then NUL padding, then argc NUL-terminated strings.
    let n = rest.iter().position(|&b| b == 0)?;
    rest = &rest[n..];
    let n = rest.iter().position(|&b| b != 0)?;
    rest = &rest[n..];
    let mut out = Vec::with_capacity(argc);
    for _ in 0..argc {
        let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        out.push(std::ffi::OsString::from_vec(rest[..n].to_vec()));
        rest = rest.get(n + 1..).unwrap_or_default();
    }
    Some(out)
}

/// `Command::output` with a deadline: the child is killed after `timeout` (a nested cargo must
/// never hang the build that triggered it).
pub fn output_with_timeout(mut c: std::process::Command, timeout: Duration) -> io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = so.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = se.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(std::process::Output { status, stdout: t_out.join().unwrap_or_default(), stderr: t_err.join().unwrap_or_default() })
}

/// Sets `p`'s mtime to now, creating it if missing.
pub fn touch(p: &Path) -> io::Result<()> {
    let c = cstr(p)?;
    if unsafe { libc::utimes(c.as_ptr(), std::ptr::null()) } == 0 {
        return Ok(());
    }
    std::fs::OpenOptions::new().create(true).append(true).open(p).map(|_| ())
}

/// Seconds since `p` was modified (`None` if missing).
pub fn age_secs(p: &Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(m).map(|d| d.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("25%").unwrap(), SizeSpec::Percent(25.0));
        assert_eq!(parse_size("100GB").unwrap(), SizeSpec::Bytes(100_000_000_000));
        assert_eq!(parse_size("100G").unwrap(), SizeSpec::Bytes(100_000_000_000));
        assert_eq!(parse_size("1.5T").unwrap(), SizeSpec::Bytes(1_500_000_000_000));
        assert_eq!(parse_size("2GiB").unwrap(), SizeSpec::Bytes(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("123").unwrap(), SizeSpec::Bytes(123));
        assert!(parse_size("12 parsecs").is_err());
        assert!(parse_size("120%").is_err());
        assert_eq!(SizeSpec::Percent(25.0).resolve(1000), 250);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert!(parse_duration("1w").is_err());
    }

    #[test]
    fn human_readable() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_500_000), "1.5 MB");
        assert_eq!(human(149_000_000_000), "149.0 GB");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
