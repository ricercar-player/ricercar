//! Small desktop helpers: launching other programs, user folders.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Start `cmd` without waiting for it, and reap it from a background
/// thread once it exits so that it never lingers as a zombie.
pub fn spawn_detached(mut cmd: Command) -> std::io::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::Builder::new()
        .name("reap-child".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

/// Open a file, folder or URL with the desktop's default handler.
pub fn xdg_open(target: impl AsRef<OsStr>) {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(target);
    if let Err(e) = spawn_detached(cmd) {
        tracing::warn!("xdg-open: {e}");
    }
}

/// The user's music folder: `XDG_MUSIC_DIR` from `user-dirs.dirs`, else
/// `~/Music`. Unlike the library default, it need not exist yet.
pub fn music_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let dirs = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
    music_dir_from(&dirs, &home)
}

fn music_dir_from(user_dirs: &str, home: &std::path::Path) -> PathBuf {
    user_dirs
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let v = l.strip_prefix("XDG_MUSIC_DIR=")?.trim_matches('"');
            let p = match v.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(v),
            };
            // "$HOME/" alone means the feature is off for this folder.
            (p.is_absolute() && p != home).then_some(p)
        })
        .unwrap_or_else(|| home.join("Music"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn music_dir_from_user_dirs() {
        let home = Path::new("/home/u");
        let dirs =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_MUSIC_DIR=\"$HOME/Musique\"\n";
        assert_eq!(music_dir_from(dirs, home), home.join("Musique"));
        let abs = "XDG_MUSIC_DIR=\"/data/music\"\n";
        assert_eq!(music_dir_from(abs, home), PathBuf::from("/data/music"));
        assert_eq!(music_dir_from("", home), home.join("Music"));
        let off = "XDG_MUSIC_DIR=\"$HOME/\"\n";
        assert_eq!(music_dir_from(off, home), home.join("Music"));
    }

    #[test]
    fn detached_child_is_reaped() {
        let mut cmd = Command::new("true");
        cmd.arg("x");
        assert!(spawn_detached(cmd).is_ok());
        assert!(spawn_detached(Command::new("/nonexistent/ricercar-test")).is_err());
    }
}
