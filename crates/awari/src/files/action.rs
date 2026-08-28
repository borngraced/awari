//! Spawning external actions: desktop open, file-manager reveal, and terminal
//! emulator scripts. All children are reaped on background threads so the UI
//! thread never blocks.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

/// Open via the desktop default handler. No shell.
pub fn activate(path: &Path) {
    match Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            thread::Builder::new()
                .name("awari-xdg-open".into())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => tracing::warn!(%e, path = %path.display(), "xdg-open failed"),
    }
}

/// Reveal a path in the file manager by opening its parent directory.
pub fn reveal(path: &Path) {
    let parent = path.parent().unwrap_or(path);
    activate(parent);
}

/// Resolve the user's preferred terminal emulator: `$TERMINAL`, then probing
/// `$PATH` directly (no `which` fork) so this is safe to call on the UI thread.
/// The result is cached for the process lifetime.
pub(crate) fn resolve_terminal() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Ok(term) = std::env::var("TERMINAL")
                && !term.trim().is_empty()
            {
                return Some(term.trim().to_string());
            }
            let paths = std::env::var("PATH").ok()?;
            for candidate in [
                "alacritty",
                "kitty",
                "ghostty",
                "wezterm",
                "foot",
                "gnome-terminal",
                "konsole",
                "st",
            ] {
                for dir in paths.split(':') {
                    if dir.is_empty() {
                        continue;
                    }
                    let p = Path::new(dir).join(candidate);
                    if p.is_file() {
                        return Some(candidate.to_string());
                    }
                }
            }
            None
        })
        .clone()
}

fn terminal_args(term: &str, script: &str) -> Vec<String> {
    let name = Path::new(term)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(term);
    match name {
        "gnome-terminal" | "kitty" => vec![
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        "wezterm" | "wezterm-gui" => vec![
            "start".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        _ => vec![
            "-e".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
    }
}

/// Spawn `script` inside the user's terminal emulator. The child is reaped on
/// a background thread (like `activate`) so the UI thread never blocks.
pub(crate) fn run_script(script: &str) {
    let Some(term) = resolve_terminal() else {
        tracing::warn!("no terminal emulator found; set $TERMINAL");
        return;
    };

    match Command::new(&term)
        .args(terminal_args(&term, script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            thread::Builder::new()
                .name("awari-terminal".into())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => tracing::warn!(%e, term, "failed to spawn terminal"),
    }
}

/// Open a terminal emulator rooted at `dir`.
pub fn run_in_terminal(dir: &Path) {
    let dir = dir.display().to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
    run_script(&format!("cd {dir:?} && exec {shell}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_argv_matches_emulator() {
        assert_eq!(
            terminal_args("wezterm", "ls"),
            vec!["start", "--", "sh", "-c", "ls"]
        );
        assert_eq!(
            terminal_args("/usr/bin/kitty", "ls"),
            vec!["--", "sh", "-c", "ls"]
        );
        assert_eq!(
            terminal_args("alacritty", "ls"),
            vec!["-e", "sh", "-c", "ls"]
        );
    }
}