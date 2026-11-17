use std::{env, fs, path::Path};
fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let object = Path::new(&out).join("foo.o");
    let archive = Path::new(&out).join("libnativefoo.a");
    assert!(std::process::Command::new("cc").args(["-c", "c/foo.c", "-o"]).arg(&object).status().unwrap().success());
    assert!(std::process::Command::new("ar").arg("crs").arg(&archive).arg(&object).status().unwrap().success());
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=nativefoo");
    let inc = Path::new(&out).join("include");
    fs::create_dir_all(&inc).unwrap();
    fs::write(inc.join("foo.h"), "int native_foo(int);\n").unwrap();
    println!("cargo:include={}", inc.display());
    println!("cargo:rerun-if-changed=c/foo.c");
    println!("cargo:rerun-if-changed=build.rs");
}
