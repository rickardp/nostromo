//! Parsing the parent cargo's command line (read with `sysctl(KERN_PROCARGS2)` by the wrapper):
//! the parts that decide which workspace a build is for.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Global flags (before the subcommand) that take a value.
const GLOBAL_WITH_VALUE: &[&str] = &["--color", "--config", "-Z", "-C"];
/// Global flags (before the subcommand) without a value.
const GLOBAL_NO_VALUE: &[&str] = &["-v", "-vv", "-vvv", "--verbose", "-q", "--quiet", "--frozen", "--locked", "--offline"];
/// Arguments that make the whole invocation informational.
const INFO_FLAGS: &[&str] = &["--version", "-V", "--list", "--help", "-h"];

/// The parsed parts of a cargo command line that nostromo cares about.
#[derive(Debug, Default, PartialEq)]
pub struct Invocation {
    /// `+toolchain` without the `+`.
    pub toolchain: Option<String>,
    pub subcommand: Option<String>,
    pub manifest_path: Option<PathBuf>,
    /// `-C DIR`.
    pub dir: Option<PathBuf>,
    /// `--version`, `--list`, `--help`, `--explain`, or an unknown global flag.
    pub informational: bool,
}

/// Parses argv (without argv[0]).
pub fn parse_args(args: &[OsString]) -> Invocation {
    let mut inv = Invocation::default();
    let mut i = 0;
    if let Some(t) = args.first().and_then(|a| a.to_str()).and_then(|a| a.strip_prefix('+')) {
        inv.toolchain = Some(t.to_string());
        i = 1;
    }
    // Global flags, up to the subcommand.
    while i < args.len() {
        let a = args[i].to_string_lossy();
        if !a.starts_with('-') || a == "-" {
            inv.subcommand = Some(a.into_owned());
            i += 1;
            break;
        }
        if a == "--" {
            break;
        }
        if INFO_FLAGS.contains(&a.as_ref()) || a == "--explain" || a.starts_with("--explain=") {
            inv.informational = true;
            return inv;
        }
        if GLOBAL_NO_VALUE.contains(&a.as_ref()) {
            i += 1;
            continue;
        }
        if GLOBAL_WITH_VALUE.contains(&a.as_ref()) {
            if a == "-C" {
                inv.dir = args.get(i + 1).map(PathBuf::from);
            }
            i += 2;
            continue;
        }
        if let Some((k, v)) = a.split_once('=')
            && GLOBAL_WITH_VALUE.contains(&k)
        {
            if k == "-C" {
                inv.dir = Some(PathBuf::from(v));
            }
            i += 1;
            continue;
        }
        // Attached short values: `-Zfoo`, `-Cdir`.
        if let Some(v) = a.strip_prefix("-C") {
            inv.dir = Some(PathBuf::from(v));
            i += 1;
            continue;
        }
        if a.starts_with("-Z") {
            i += 1;
            continue;
        }
        // Unknown global flag: do not guess.
        inv.informational = true;
        return inv;
    }
    // Subcommand arguments, up to `--`.
    while i < args.len() {
        let a = &args[i];
        let s = a.to_string_lossy();
        if s == "--" {
            break;
        }
        if s == "--help" || s == "-h" {
            inv.informational = true;
        } else if s == "--manifest-path" {
            inv.manifest_path = args.get(i + 1).map(PathBuf::from);
            i += 1;
        } else if let Some(v) = a.as_bytes().strip_prefix(b"--manifest-path=") {
            inv.manifest_path = Some(PathBuf::from(OsStr::from_bytes(v)));
        }
        i += 1;
    }
    inv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn p(args: &[&str]) -> Invocation {
        parse_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_toolchain_globals_and_manifest() {
        let i = p(&["+nightly", "-v", "--color", "always", "--config=x=1", "-Zunstable-options", "-C", "sub", "build", "--manifest-path", "a/Cargo.toml", "--", "--manifest-path", "b"]);
        assert_eq!(i.toolchain.as_deref(), Some("nightly"));
        assert_eq!(i.subcommand.as_deref(), Some("build"));
        assert_eq!(i.dir.as_deref(), Some(Path::new("sub")));
        assert_eq!(i.manifest_path.as_deref(), Some(Path::new("a/Cargo.toml")));
        assert!(!i.informational);

        let i = p(&["test", "--manifest-path=x/Cargo.toml", "--", "--help"]);
        assert_eq!(i.manifest_path.as_deref(), Some(Path::new("x/Cargo.toml")));
        assert_eq!(p(&["clean"]).subcommand.as_deref(), Some("clean"));
        assert!(p(&["--weird", "build"]).informational);
    }
}
