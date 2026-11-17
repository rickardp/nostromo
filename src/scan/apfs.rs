use super::RawScan;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// `getattrlistbulk` result buffer per directory level. Must be heap-allocated: the recursion
/// keeps one live buffer per depth.
const BUF_SIZE: usize = 256 * 1024;
// `fsobj_type_t` values from <sys/vnode.h>.
const VREG: u32 = 1;
const VDIR: u32 = 2;

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = crate::util::cstr(path)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_dir(dfd: &OwnedFd, name: &CStr) -> Option<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dfd.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap())
}

/// Recursive walk. `pool[depth]` is this level's result buffer (allocated on first use).
fn walk(dfd: &OwnedFd, depth: usize, pool: &mut Vec<Vec<u8>>, out: &mut RawScan) {
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
    al.fileattr = libc::ATTR_FILE_ALLOCSIZE;
    // With FSOPT_ATTR_CMN_EXTENDED, `forkattr` holds ATTR_CMNEXT_* attributes.
    al.forkattr = libc::ATTR_CMNEXT_CLONEID;
    let opts = (libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW) as u64;

    if pool.len() <= depth {
        pool.resize_with(depth + 1, Vec::new);
    }
    let mut buf = std::mem::take(&mut pool[depth]);
    if buf.len() < BUF_SIZE {
        buf = vec![0u8; BUF_SIZE];
    }
    let set_size = std::mem::size_of::<libc::attribute_set_t>();
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                dfd.as_raw_fd(),
                (&raw mut al).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                opts,
            )
        };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            break;
        }
        let mut p = 0usize;
        for _ in 0..n {
            let entry = p;
            let len = read_u32(&buf, p) as usize;
            if len == 0 || entry + len > buf.len() {
                break; // malformed; never seen, but don't loop or read out of bounds
            }
            p += 4;
            // attribute_set_t: commonattr, volattr, dirattr, fileattr, forkattr (u32 each).
            let ret_file = read_u32(&buf, p + 12);
            let ret_fork = read_u32(&buf, p + 16);
            p += set_size;
            // ATTR_CMN_NAME: attrreference_t { i32 offset (from the reference itself), u32 len }.
            let name_ref = p;
            let name_off = read_u32(&buf, p) as i32 as isize;
            p += 8;
            let objtype = read_u32(&buf, p);
            p += 4;
            let mut alloc = 0u64;
            let mut clone = 0u64;
            if ret_file & libc::ATTR_FILE_ALLOCSIZE != 0 {
                alloc = read_u64(&buf, p);
                p += 8;
            }
            if ret_fork & libc::ATTR_CMNEXT_CLONEID != 0 {
                clone = read_u64(&buf, p);
            }
            match objtype {
                VREG => {
                    out.files += 1;
                    if clone == 0 {
                        out.no_clone_bytes += alloc;
                    } else {
                        out.entries.push(((u64::MAX, clone), alloc));
                    }
                }
                VDIR => {
                    let start = (name_ref as isize + name_off) as usize;
                    if start < entry + len
                        && let Ok(name) = CStr::from_bytes_until_nul(&buf[start..entry + len])
                        && let Some(sub) = openat_dir(dfd, name)
                    {
                        walk(&sub, depth + 1, pool, out);
                    }
                }
                _ => {}
            }
            p = entry + len;
        }
    }
    pool[depth] = buf;
}

pub(super) fn scan_raw(dir: &Path) -> io::Result<RawScan> {
    let fd = open_dir(dir)?;
    let mut out = RawScan::default();
    walk(&fd, 0, &mut Vec::new(), &mut out);
    Ok(out)
}
