//! CLI dispatch: `nostromo <init|doctor|status|gc|help|--version>`.

pub mod doctor;
pub mod gc;
pub mod init;
pub mod status;

pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = if args.len() > 1 { &args[1..] } else { &[][..] };
    match args.first().map(String::as_str) {
        Some("init") => init::main(rest),
        Some("doctor") => doctor::main(rest),
        Some("status") => status::main(rest),
        Some("gc") => gc::main(rest),
        Some("--version" | "-V") => {
            println!("nostromo {}", env!("CARGO_PKG_VERSION"));
            0
        }
        None | Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("nostromo: unknown command `{other}`\n\n{USAGE}");
            2
        }
    }
}

const USAGE: &str = "\
nostromo: per-worktree Cargo build-dirs, CoW-seeded and evicted before the disk fills

Usage:
  nostromo init [--store <dir>] [--uninstall]
                            add nostromo to ~/.cargo/config.toml, create the store,
                            cleanup is triggered by Cargo builds
  nostromo doctor           check the installation
  nostromo status           store size, free space, build-dirs by last use
  nostromo gc [--dry-run] [--to <size>]
                            evict idle build-dirs now
  nostromo gc --auto        build-triggered maintenance: account, enforce limits

Config: ~/.config/nostromo/config.toml (store, max_size, min_free_space, min_idle, keep_recent,\n        min_eviction_gain)
No seeding/tracking for one command: NOSTROMO_DISABLE=1 cargo ...
";
