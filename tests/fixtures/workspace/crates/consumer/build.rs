fn main() {
    let inc = std::env::var("DEP_NATIVEFOO_INCLUDE").unwrap();
    println!("cargo:rustc-env=NATIVE_INCLUDE={inc}");
    println!("cargo:rerun-if-changed=build.rs");
}
