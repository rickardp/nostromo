//! Basic-mode allocated-space accounting. Never follows symlinks, including during directory races.
use super::RawScan;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;

struct Dir(*mut libc::DIR);
impl Drop for Dir {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

pub(super) fn scan_raw(path: &Path) -> io::Result<RawScan> {
    let c = crate::util::cstr(path)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = RawScan::default();
    walk(unsafe { OwnedFd::from_raw_fd(fd) }, &mut out)?;
    Ok(out)
}

fn walk(fd: OwnedFd, out: &mut RawScan) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let ptr = unsafe { libc::fdopendir(raw) };
    if ptr.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = fd.into_raw_fd(); // fdopendir now owns it
    let dir = Dir(ptr);
    loop {
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(raw, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue;
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                let id = (stat.st_dev as u64, stat.st_ino as u64);
                out.entries
                    .push((id, (stat.st_blocks.max(0) as u64).saturating_mul(512)));
                out.files += 1;
                let links = out.links.entry(id).or_default();
                links.0 += 1;
                links.1 = links.1.max(stat.st_nlink as u64);
            }
            libc::S_IFDIR => {
                let sub = unsafe {
                    libc::openat(
                        raw,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if sub >= 0 {
                    let _ = walk(unsafe { OwnedFd::from_raw_fd(sub) }, out);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::build;
    use std::collections::HashSet;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn allocated_blocks_hardlinks_and_symlinks() {
        let root = std::env::temp_dir().join(format!("nostromo-basic-scan-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/nested")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let a = root.join("a");
        let b = root.join("b");
        let file = a.join("data");
        std::fs::write(&file, vec![0xa5; 1024 * 1024]).unwrap();
        std::fs::hard_link(&file, a.join("nested/link")).unwrap();
        std::fs::hard_link(&file, b.join("link")).unwrap();
        symlink(&root, a.join("cycle")).unwrap();
        symlink(&file, a.join("file-symlink")).unwrap();
        let sparse = a.join("sparse");
        std::fs::File::create(&sparse)
            .unwrap()
            .set_len(100 * 1024 * 1024)
            .unwrap();
        let allocated = std::fs::metadata(&file).unwrap().blocks() * 512;
        let sparse_alloc = std::fs::metadata(&sparse).unwrap().blocks() * 512;
        let scan = || {
            let mut raws = vec![scan_raw(&a).unwrap(), scan_raw(&b).unwrap()];
            for raw in &mut raws {
                raw.dedup();
            }
            build(&[a.clone(), b.clone()], raws)
        };
        let s = scan();
        assert_eq!(s.total_files(), 4);
        assert_eq!(s.union_bytes(), allocated + sparse_alloc);
        assert_eq!(s.freed_by(&a, &HashSet::new()), sparse_alloc);
        assert_eq!(
            s.freed_by(&a, &HashSet::from([b.clone()])),
            allocated + sparse_alloc
        );
        // A link outside all managed build-dirs means eviction cannot reclaim this inode.
        std::fs::hard_link(&file, root.join("outside")).unwrap();
        assert_eq!(
            scan().freed_by(&a, &HashSet::from([b.clone()])),
            sparse_alloc
        );
        assert!(scan_raw(&a.join("cycle")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
