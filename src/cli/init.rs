//! `nostromo init [--uninstall] [--store <path>]`.
//!
//! Adds a marked block to Cargo's user config (`$CARGO_HOME/config.toml`, default
//! `~/.cargo/config.toml`), which every cargo reads regardless of shell, PATH or profile:
//!
//! ```toml
//! [build]                 # (inside the markers when nostromo creates the table)
//! # >>> nostromo >>>
//! build-dir = "<store>/build/{workspace-path-hash}"
//! rustc-wrapper = "<path to nostromo>"
//! # <<< nostromo <<<
//! ```
//!
//! An existing `build-dir` line in `[build]` is commented out (and restored by `--uninstall`);
//! an existing `rustc-wrapper` is never replaced. Creates the store; cleanup is started
//! by Cargo wrappers in their existing permission context.

use crate::config::Config;
use crate::evict::{self, Trigger};
use crate::store::Store;
use crate::util;
use std::io;
use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> nostromo >>>";
pub const END: &str = "# <<< nostromo <<<";
/// Prefix of a user line nostromo commented out.
pub const DISABLED: &str = "# (disabled by nostromo) ";

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| util::home().join(".cargo"))
}

/// The user config file Cargo reads: `config.toml`, or the legacy `config` if only it exists.
pub fn cargo_config_path() -> PathBuf {
    let home = cargo_home();
    let (toml, legacy) = (home.join("config.toml"), home.join("config"));
    if !toml.exists() && legacy.is_file() { legacy } else { toml }
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn is_build_header(line: &str) -> bool {
    let t = line.split('#').next().unwrap_or("").trim();
    t == "[build]" || t.replace(' ', "") == "[build]"
}

fn is_table_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

fn key_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, _) = t.split_once('=')?;
    Some(k.trim().trim_matches('"').to_string())
}

/// `text` without nostromo's block, with lines nostromo disabled restored.
pub fn strip_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_end();
        if !inside && t == BEGIN {
            inside = true;
        } else if inside && t == END {
            inside = false;
        } else if !inside {
            match line.strip_prefix(DISABLED) {
                Some(orig) => out.push_str(orig),
                None => out.push_str(line),
            }
        }
    }
    out
}

/// `text` with nostromo's settings installed. Errors (without changing anything) when the
/// result would not set exactly these values, e.g. a foreign `rustc-wrapper`.
pub fn with_block(text: &str, build_dir: &str, wrapper: &str) -> Result<String, String> {
    let base = strip_block(text);
    let body = format!("{BEGIN}\nbuild-dir = {}\nrustc-wrapper = {}\n{END}\n", toml_str(build_dir), toml_str(wrapper));
    let mut out = String::new();
    let mut in_build = false;
    let mut inserted = false;
    for line in base.split_inclusive('\n') {
        if is_table_header(line) {
            in_build = is_build_header(line);
        }
        let root_dotted = !in_build && key_of(line).is_some_and(|k| k == "build.build-dir" || k == "build.rustc-wrapper");
        let in_table = in_build && key_of(line).is_some_and(|k| k == "build-dir" || k == "rustc-wrapper");
        if root_dotted || in_table {
            let k = key_of(line).unwrap_or_default();
            if k.ends_with("rustc-wrapper") {
                return Err(format!(
                    "Cargo's config already sets a rustc wrapper ({}). nostromo needs `build.rustc-wrapper`; \
                     remove that line (or chain the other wrapper yourself) and run `nostromo init` again",
                    line.trim()
                ));
            }
            out.push_str(DISABLED);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
            continue;
        }
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
        if in_build && !inserted && is_build_header(line) {
            out.push_str(&body);
            inserted = true;
        }
    }
    if !inserted {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        // Our own table header goes inside the markers so `--uninstall` removes it too.
        out.push_str(&format!("{BEGIN}\n[build]\n{}", &body[BEGIN.len() + 1..]));
    }
    // Verify with a real parser: the file must stay valid and carry exactly our values.
    let v: toml::Table = toml::from_str(&out).map_err(|e| format!("the edited Cargo config would not be valid TOML: {e}"))?;
    let build = v.get("build").and_then(|b| b.as_table());
    let get = |k: &str| build.and_then(|b| b.get(k)).and_then(|x| x.as_str());
    if get("build-dir") != Some(build_dir) || get("rustc-wrapper") != Some(wrapper) {
        return Err("could not place nostromo's settings in the Cargo config (unusual layout); add them by hand".into());
    }
    Ok(out)
}

fn read_or_empty(p: &Path) -> io::Result<String> {
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Sets `store = "<path>"` in the nostromo config file, keeping every other line (and comments).
fn write_store_config(path: &Path, store: &Path) -> Result<(), String> {
    let text = read_or_empty(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let line = format!("store = {}", toml_str(&store.display().to_string()));
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        let is_store = t.strip_prefix("store").is_some_and(|r| r.trim_start().starts_with('='));
        if is_store && !replaced {
            out.push(line.clone());
            replaced = true;
        } else if !is_store {
            out.push(l.to_string());
        }
    }
    if !replaced {
        out.insert(0, line);
    }
    let mut s = out.join("\n");
    s.push('\n');
    toml::from_str::<toml::Table>(&s).map_err(|e| format!("{} would not be valid TOML: {e}", path.display()))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    util::write_atomic(path, s.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The path written as `rustc-wrapper`: the executable as invoked (a stable symlink such as
/// `~/.cargo/bin/nostromo` stays valid across upgrades, unlike its canonical target).
pub fn wrapper_path() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?;
    if p.is_absolute() { Some(p) } else { std::env::current_dir().ok().map(|c| c.join(p)) }
}

const USAGE: &str = "usage: nostromo init [--uninstall] [--store <path>] [--allow-no-cow]";

pub fn main(args: &[String]) -> i32 {
    let mut uninstall = false;
    let mut allow_no_cow = false;
    let mut store_arg: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--uninstall" => uninstall = true,
            "--allow-no-cow" => allow_no_cow = true,
            "--store" => match it.next() {
                Some(v) => store_arg = Some(PathBuf::from(v)),
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            s if s.starts_with("--store=") => store_arg = Some(PathBuf::from(&s["--store=".len()..])),
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("nostromo init: unknown argument `{other}`\n{USAGE}");
                return 2;
            }
        }
    }
    if uninstall { run_uninstall() } else { run_install(store_arg, allow_no_cow) }
}

fn run_uninstall() -> i32 {
    let mut rc = 0;
    let f = cargo_config_path();
    match read_or_empty(&f) {
        Ok(text) => {
            let stripped = strip_block(&text);
            if stripped != text {
                match util::write_atomic(&f, stripped.as_bytes()) {
                    Ok(()) => println!("removed nostromo's settings from {}", f.display()),
                    Err(e) => {
                        eprintln!("nostromo: cannot update {}: {e}", f.display());
                        rc = 1;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("nostromo: cannot read {}: {e}", f.display());
            rc = 1;
        }
    }
    println!("The store and its build-dirs were left in place; delete them by hand if you like.");
    rc
}

/// macOS requires explicit consent for basic-mode installation (ADR-002).
fn check_install_mode(cow: bool, allow_no_cow: bool, macos: bool) -> Result<(), &'static str> {
    if macos && !cow && !allow_no_cow {
        Err("CoW is unavailable for this store. To install basic mode (disk cleanup without cache reuse), explicitly run `nostromo init --allow-no-cow` with the same --store path")
    } else { Ok(()) }
}

fn run_install(store_arg: Option<PathBuf>, allow_no_cow: bool) -> i32 {
    let Some(exe) = wrapper_path() else {
        eprintln!("nostromo: cannot resolve the nostromo executable");
        return 1;
    };
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => { eprintln!("nostromo: config: {e}"); return 1; }
    };
    let store_arg = store_arg.map(|s| if s.is_absolute() { s } else { std::env::current_dir().map(|c| c.join(&s)).unwrap_or(s) });
    if let Some(s) = &store_arg { cfg.store = s.clone(); }
    let cow = match util::cow_available(&cfg.store) {
        Ok(c) => c,
        Err(e) => { eprintln!("nostromo: cannot inspect store: {e}"); return 1; }
    };
    // Require the flag on this invocation; a previous store's consent must not silently carry over.
    if let Err(e) = check_install_mode(cow, allow_no_cow, cfg!(target_os = "macos")) {
        eprintln!("nostromo: {e}"); return 1;
    }
    let store = Store::new(cfg.store.clone());
    if let Err(e) = store.ensure() {
        eprintln!("nostromo: cannot create store {}: {e}", store.root.display());
        return 1;
    }

    // Cargo config.
    let f = cargo_config_path();
    let text = match read_or_empty(&f) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("nostromo: cannot read {}: {e}", f.display());
            return 1;
        }
    };
    let new = match with_block(&text, &store.template(), &exe.display().to_string()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("nostromo: {}: {e}", f.display());
            return 1;
        }
    };
    if let Some(s) = &store_arg {
        if let Err(e) = write_store_config(&cfg.path, s) { eprintln!("nostromo: {e}"); return 1; }
    }
    if allow_no_cow {
        let text = match read_or_empty(&cfg.path) {
            Ok(t) => t,
            Err(e) => { eprintln!("nostromo: {e}"); return 1; }
        };
        let mut table: toml::Table = match toml::from_str(&text) {
            Ok(t) => t,
            Err(e) => { eprintln!("nostromo: {e}"); return 1; }
        };
        table.insert("allow_no_cow".into(), toml::Value::Boolean(true));
        if let Err(e) = util::write_atomic(&cfg.path, toml::to_string(&table).unwrap().as_bytes()) {
            eprintln!("nostromo: {e}"); return 1;
        }
    }
    if new != text {
        if let Some(d) = f.parent()
            && let Err(e) = std::fs::create_dir_all(d)
        {
            eprintln!("nostromo: cannot create {}: {e}", d.display());
            return 1;
        }
        if let Err(e) = util::write_atomic(&f, new.as_bytes()) {
            eprintln!("nostromo: cannot write {}: {e}", f.display());
            return 1;
        }
    }
    println!("cargo config: {} (build-dir in the store, rustc-wrapper = {})", f.display(), exe.display());
    if new.contains(DISABLED) {
        println!("  an existing build-dir setting was commented out; `nostromo init --uninstall` restores it");
    }

    println!("store: {}", store.root.display());
    println!("mode: {}", if cow { "CoW cache reuse and disk cleanup" } else { "basic (disk cleanup; no CoW cache reuse)" });
    // Maintenance stays in Cargo's existing permission context.
    println!("cleanup: triggered by Cargo builds in their existing permission context");

    match evict::run(&store, &cfg, Trigger::Manual { target: None }, true) {
        Ok(o) if o.evicted.is_empty() => {}
        Ok(o) => println!("gc: {} build-dir(s) over the limits would be evicted ({})", o.evicted.len(), util::human(o.freed)),
        Err(e) => println!("gc: dry run failed ({e})"),
    }
    println!("\nDone. Every cargo now builds through nostromo; `nostromo doctor` checks the setup.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_mode_requires_explicit_macos_consent() {
        assert!(check_install_mode(false, false, true).is_err());
        assert!(check_install_mode(false, true, true).is_ok());
        assert!(check_install_mode(true, false, true).is_ok());
        assert!(check_install_mode(false, false, false).is_ok());
    }

    const BD: &str = "/s/build/{workspace-path-hash}";
    const W: &str = "/h/.cargo/bin/nostromo";

    #[test]
    fn empty_and_roundtrip() {
        let a = with_block("", BD, W).unwrap();
        assert!(a.starts_with("# >>> nostromo >>>\n[build]\nbuild-dir = \"/s/build/{workspace-path-hash}\"\n"));
        assert_eq!(strip_block(&a), "");
        assert_eq!(with_block(&a, BD, W).unwrap(), with_block(&strip_block(&a), BD, W).unwrap());
    }

    #[test]
    fn existing_build_table_and_build_dir() {
        let user = "[net]\noffline = true\n\n[build]\njobs = 4\nbuild-dir = \"/old\"\n\n[alias]\nb = \"build\"\n";
        let a = with_block(user, BD, W).unwrap();
        assert!(a.contains("[build]\n# >>> nostromo >>>\n"));
        assert!(a.contains("# (disabled by nostromo) build-dir = \"/old\"\n"));
        assert!(a.contains("jobs = 4\n"));
        assert_eq!(strip_block(&a), user);
        // Reinstall is idempotent.
        assert_eq!(with_block(&a, BD, W).unwrap(), a);
    }

    #[test]
    fn dotted_root_keys_and_foreign_wrapper() {
        let user = "build.build-dir = \"/old\"\n";
        let a = with_block(user, BD, W).unwrap();
        assert_eq!(strip_block(&a).trim_end(), user.trim_end());
        assert!(with_block("[build]\nrustc-wrapper = \"sccache\"\n", BD, W).is_err());
        assert!(with_block("build.rustc-wrapper = \"sccache\"\n", BD, W).is_err());
    }
}
