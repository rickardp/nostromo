//! nostromo: managed Cargo build-dirs, LRU-evicted with optional APFS CoW reuse.
//!
//! One binary. Invoked by Cargo as `build.rustc-wrapper` (`nostromo <rustc> ...`) it is the
//! wrapper; otherwise the CLI.

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if nostromo::wrapper::is_wrapper_invocation(&args) {
        nostromo::wrapper::main(args);
    }
    std::process::exit(nostromo::cli::main());
}
