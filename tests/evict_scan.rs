//! Scanner regressions: clone/hardlink dedup and `freed_by` with shared blocks in synthetic stores.

#[path = "common_evict/mod.rs"]
mod common;
#[cfg(target_os = "linux")]
use common::Lab;

#[cfg(target_os = "macos")]
use common::{Lab, MB, write_file};
use nostromo::scan::{dir_footprint, scan_dirs};
#[cfg(target_os = "macos")]
use nostromo::util;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(target_os = "macos")]
#[test]
fn footprint_dedups_clones_and_hardlinks() {
    let lab = Lab::new("dedup");
    if !util::cow_available(&lab.root).unwrap() { return; }
    let d = lab.root.join("d");
    std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
    write_file(&d.join("a"), MB);
    util::clonefile(&d.join("a"), &d.join("sub/a-clone")).unwrap();
    std::fs::hard_link(d.join("a"), d.join("sub/deeper/a-link")).unwrap();
    write_file(&d.join("sub/b"), 2 * MB);
    // A symlink to a big file outside must not be followed.
    let outside = lab.root.join("outside");
    write_file(&outside, 3 * MB);
    std::os::unix::fs::symlink(&outside, d.join("link-out")).unwrap();
    std::os::unix::fs::symlink(&lab.root, d.join("dirlink-out")).unwrap();

    let f = dir_footprint(&d).unwrap();
    assert_eq!(f.files, 4);
    assert_eq!(f.bytes, 3 * MB);

    // First write gives the clone its own id: now counted separately.
    std::fs::OpenOptions::new().write(true).open(d.join("sub/a-clone")).and_then(|mut fh| {
        use std::io::Write;
        fh.write_all(b"x")
    }).unwrap();
    assert_eq!(dir_footprint(&d).unwrap().bytes, 4 * MB);
}

#[test]
fn many_files_and_large_dirs() {
    // More entries than fit in one 256 KB getattrlistbulk buffer, across nested levels.
    let lab = Lab::new("many");
    let d = lab.root.join("d");
    for sub in 0..3 {
        let s = d.join(format!("s{sub}/inner"));
        std::fs::create_dir_all(&s).unwrap();
        for i in 0..2500 {
            std::fs::write(s.join(format!("file-with-a-longish-name-{i:05}.o")), b"x").unwrap();
        }
    }
    let f = dir_footprint(&d).unwrap();
    assert_eq!(f.files, 7500);
    assert!(f.bytes >= 7500 * 4096, "each 1-byte file allocates a block: {}", f.bytes);
}

#[cfg(target_os = "macos")]
#[test]
fn freed_by_with_shared_blocks() {
    let lab = Lab::new("freed");
    if !util::cow_available(&lab.root).unwrap() { return; }
    let a = lab.build_dir("aa0001", MB);
    let b = lab.store.build_root().join("aa/aa0002");
    util::clonefile(&a, &b).unwrap();
    let c = lab.build_dir("cc0001", 2 * MB);
    let dirs = vec![a.clone(), b.clone(), c.clone()];

    let s = scan_dirs(&dirs, 2).unwrap();
    assert_eq!(s.union_bytes(), 3 * MB);
    assert_eq!(s.footprint(&a), MB);
    assert_eq!(s.footprint(&b), MB);
    let none = HashSet::new();
    assert_eq!(s.freed_by(&a, &none), 0);
    assert_eq!(s.freed_by(&b, &none), 0);
    assert_eq!(s.freed_by(&c, &none), 2 * MB);
    let gone_b: HashSet<PathBuf> = [b.clone()].into();
    assert_eq!(s.freed_by(&a, &gone_b), MB);

    // B diverges (rewritten file): A's blocks become private.
    write_file(&b.join("debug/deps/blob.rlib"), MB);
    let s = scan_dirs(&dirs, 3).unwrap();
    assert_eq!(s.union_bytes(), 4 * MB);
    assert_eq!(s.freed_by(&a, &none), MB);

    // A missing dir counts as empty.
    let s = scan_dirs(&[a.clone(), lab.root.join("nope")], 2).unwrap();
    assert_eq!(s.union_bytes(), MB);
}
