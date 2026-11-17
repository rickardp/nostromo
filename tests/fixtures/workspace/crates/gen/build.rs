use std::{env, fs, path::Path};
fn main() {
    let out = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out).join("gen.rs"), "pub const GENERATED: u32 = 42;\n").unwrap();
    fs::write(Path::new(&out).join("data.txt"), "runtime-data\n").unwrap();
    println!("cargo:rustc-env=GEN_DIR={out}");
    println!("cargo:rerun-if-changed=build.rs");
}
