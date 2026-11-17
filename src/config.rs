//! `$XDG_CONFIG_HOME/nostromo/config.toml` (default `~/.config/nostromo/config.toml`).
//!
//! Test/override hooks: `NOSTROMO_CONFIG` (config file path), `NOSTROMO_STORE` (store root).

use crate::util::{self, SizeSpec};
use serde::Deserialize;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    store: Option<String>,
    allow_no_cow: Option<bool>,
    max_size: Option<String>,
    min_free_space: Option<String>,
    min_idle: Option<String>,
    keep_recent: Option<usize>,
    min_eviction_gain: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the config was (or would be) read from.
    pub path: PathBuf,
    pub store: PathBuf,
    /// Explicit macOS consent to install on a store without CoW.
    pub allow_no_cow: bool,
    /// Soft cap on the store footprint. Default 25% of the store volume.
    pub max_size: SizeSpec,
    /// Floor on free space. `None` = default max(20 GB, 5% of the volume).
    pub min_free_space: Option<SizeSpec>,
    /// Grace before a `max_size` eviction may take a build-dir.
    pub min_idle: Duration,
    /// Most recently used primary build-dirs that are never evicted.
    pub keep_recent: usize,
    /// Skip evicting a build-dir that would free less than this (clones share blocks). 100 MB.
    pub min_eviction_gain: u64,
}

/// Limits resolved against the store volume.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_size: u64,
    pub min_free: u64,
}

pub const DEFAULT_MIN_FREE_FLOOR: u64 = 20_000_000_000;

impl Config {
    pub fn config_path() -> PathBuf {
        if let Some(p) = std::env::var_os("NOSTROMO_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| util::home().join(".config"));
        base.join("nostromo").join("config.toml")
    }

    pub fn load() -> Result<Config, String> {
        let path = Self::config_path();
        let raw: RawConfig = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => RawConfig::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Self::from_raw(path, raw)
    }

    fn from_raw(path: PathBuf, raw: RawConfig) -> Result<Config, String> {
        let store = std::env::var_os("NOSTROMO_STORE")
            .map(PathBuf::from)
            .or_else(|| raw.store.as_deref().map(expand_tilde))
            .unwrap_or_else(default_store);
        if !store.is_absolute() {
            return Err(format!("store must be an absolute path: {}", store.display()));
        }
        Ok(Config {
            path,
            store,
            allow_no_cow: raw.allow_no_cow.unwrap_or(false),
            max_size: raw.max_size.as_deref().map(util::parse_size).transpose()?.unwrap_or(SizeSpec::Percent(25.0)),
            min_free_space: raw.min_free_space.as_deref().map(util::parse_size).transpose()?,
            min_idle: raw.min_idle.as_deref().map(util::parse_duration).transpose()?.unwrap_or(Duration::from_secs(600)),
            keep_recent: raw.keep_recent.unwrap_or(2),
            min_eviction_gain: match raw.min_eviction_gain.as_deref().map(util::parse_size).transpose()? {
                None => 100_000_000,
                Some(SizeSpec::Bytes(b)) => b,
                Some(SizeSpec::Percent(_)) => return Err("min_eviction_gain must be a size, not a percentage".into()),
            },
        })
    }

    pub fn limits(&self) -> io::Result<Limits> {
        let st = util::statfs(&self.store)?;
        Ok(self.limits_for(st.total))
    }

    pub fn limits_for(&self, volume_total: u64) -> Limits {
        let min_free = match self.min_free_space {
            Some(s) => s.resolve(volume_total),
            None => DEFAULT_MIN_FREE_FLOOR.max(volume_total / 20),
        };
        Limits { max_size: self.max_size.resolve(volume_total), min_free }
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => util::home().join(rest),
        None if s == "~" => util::home(),
        None => Path::new(s).to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, String> {
        let raw: RawConfig = toml::from_str(s).map_err(|e| e.to_string())?;
        Config::from_raw(PathBuf::from("/x/config.toml"), raw)
    }

    #[test]
    fn defaults() {
        let c = parse("store = \"/s\"").unwrap();
        assert_eq!(c.store, PathBuf::from("/s"));
        assert_eq!(c.keep_recent, 2);
        assert_eq!(c.min_idle, Duration::from_secs(600));
        let l = c.limits_for(2_000_000_000_000);
        assert_eq!(l.max_size, 500_000_000_000);
        assert_eq!(l.min_free, 100_000_000_000);
        let small = c.limits_for(250_000_000_000);
        assert_eq!(small.min_free, DEFAULT_MIN_FREE_FLOOR);
    }

    #[test]
    fn explicit_values_and_unknown_keys() {
        let c = parse("store = \"/s\"\nmax_size = \"100GB\"\nmin_free_space = \"10%\"\nmin_idle = \"1m\"\nkeep_recent = 1").unwrap();
        let l = c.limits_for(1_000_000_000_000);
        assert_eq!(l.max_size, 100_000_000_000);
        assert_eq!(l.min_free, 100_000_000_000);
        assert_eq!(c.keep_recent, 1);
        assert!(parse("stor = \"/s\"").is_err());
        assert!(parse("store = \"relative\"").is_err());
    }
}

fn default_store() -> PathBuf {
    #[cfg(target_os = "macos")]
    { util::home().join("Library/Caches/nostromo") }
    #[cfg(target_os = "linux")]
    { std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .unwrap_or_else(|| util::home().join(".cache")).join("nostromo") }
}
