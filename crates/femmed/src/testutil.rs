//! Test-only helpers for the modules that run fake external tools (`git`, `gh`, `claude`, ...).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The one executable behind every fake tool, made once per process and never written again: a
/// script written and run in the same instant can fail with "Text file busy" while another test's
/// fork still holds its descriptor.
fn wrapper() -> &'static Path {
    static WRAPPER: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &WRAPPER
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fake-tool");
            fs::write(&path, "#!/bin/sh\nexec /bin/sh \"$0.sh\" \"$@\"\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            std::thread::sleep(Duration::from_millis(100)); // a fork that raced the write is long gone
            (dir, path)
        })
        .1
}

/// Installs the fake tool `name` in `bin`: `<name>.sh` holds `script` (shell), and `<name>` is a
/// symlink to the shared wrapper, which runs it. Calling it again replaces the script.
pub(crate) fn install_tool(bin: &Path, name: &str, script: &str) {
    fs::create_dir_all(bin).unwrap();
    fs::write(bin.join(format!("{name}.sh")), script).unwrap();
    let link = bin.join(name);
    if !link.exists() {
        std::os::unix::fs::symlink(wrapper(), &link).unwrap();
    }
}
