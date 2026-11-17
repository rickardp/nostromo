//! Capture pipes AND the controlling terminal: /dev/tty output bypasses pipe-only tests.
#[path = "common_evict/mod.rs"]
mod common;

use common::Lab;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::{Command, Stdio};

#[test]
fn wrapped_output_is_unchanged_unless_debug_is_enabled() {
    let lab = Lab::new("wrapper-output");
    let config = lab.root.join("config.toml");
    std::fs::write(&config, "min_free_space = \"0\"\n").unwrap();
    let bd = lab.build_dir("aa0001", 0);
    // Keep detached maintenance out of this output test.
    nostromo::util::touch(&lab.store.meta_dir().join("auto.stamp")).unwrap();
    let warning = "disk pressure should not distract the agent";
    lab.store.push_warning(warning).unwrap();
    let tool = lab.root.join("rustc-fixture");
    std::fs::write(&tool, "#!/bin/sh\nprintf 'compiler stdout\\n'\nprintf 'compiler diagnostic\\n' >&2\nexit 17\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    for debug in [None, Some("0"), Some("1")] {
        let (mut master_fd, mut slave_fd) = (-1, -1);
        // SAFETY: valid output pointers; default terminal settings and window size.
        assert_eq!(unsafe {
            libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        }, 0);
        // SAFETY: openpty returned two owned descriptors.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
        // Nonblocking reads avoid hanging if any inherited terminal descriptor stays open.
        assert_ne!(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) }, -1);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nostromo"));
        cmd.arg(&tool).arg("--out-dir").arg(bd.join("debug/deps"))
            .env("NOSTROMO_CONFIG", &config).env("NOSTROMO_STORE", &lab.store.root)
            .env_remove("NOSTROMO_DEBUG").env_remove("NOSTROMO_DISABLE").env_remove("NOSTROMO_ACTIVE")
            .stdin(Stdio::from(slave)).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(value) = debug {
            cmd.env("NOSTROMO_DEBUG", value);
        }
        // SAFETY: only async-signal-safe calls in the child before exec. stdin is the PTY slave.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = cmd.output().unwrap();
        assert_eq!(output.status.code(), Some(17));
        assert_eq!(output.stdout, b"compiler stdout\n");
        let expected = if debug == Some("1") {
            format!("nostromo: {warning}\ncompiler diagnostic\n")
        } else {
            "compiler diagnostic\n".into()
        };
        assert_eq!(output.stderr, expected.as_bytes());
        let mut terminal = Vec::new();
        let _ = master.read_to_end(&mut terminal); // EOF may be reported as EIO by the PTY.
        assert!(terminal.is_empty(), "unexpected terminal output: {:?}", String::from_utf8_lossy(&terminal));
        assert_eq!(std::fs::read_to_string(lab.store.warnings_path()).unwrap(), format!("{warning}\n"));
    }

    let status = Command::new(env!("CARGO_BIN_EXE_nostromo"))
        .arg("status").env("NOSTROMO_CONFIG", &config).env("NOSTROMO_STORE", &lab.store.root)
        .output().unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains(warning));
}
