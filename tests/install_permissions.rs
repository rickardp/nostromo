//! Installation must not introduce a new privacy permission context.
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

struct Install {
    root: PathBuf,
}
impl Install {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nostromo-permissions-{}-{name}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for tool in ["launchctl", "tmutil"] {
            let p = root.join("bin").join(tool);
            std::fs::write(&p, "#!/bin/sh\nprintf '%s %s\\n' \"${0##*/}\" \"$*\" >> \"$HOME/calls\"\nexit 1\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { root }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nostromo"));
        c.env_clear().env("HOME", &self.root)
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .args(["init", "--store"]).arg(self.root.join("store"));
        c
    }
}
impl Drop for Install {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
}

#[test]
fn fresh_install_never_starts_system_permission_contexts() {
    let h = Install::new("fresh");
    let o = h.command().output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!h.root.join("Library/LaunchAgents").exists());
    assert!(std::fs::read_to_string(h.root.join(".cargo/config.toml")).unwrap().contains("rustc-wrapper"));
    // Reinstall and uninstall must remain config-only too.
    assert!(h.command().status().unwrap().success());
    assert!(h.command().arg("--uninstall").status().unwrap().success());
    assert!(!h.root.join("calls").exists(), "must not invoke launchctl or tmutil");
}
