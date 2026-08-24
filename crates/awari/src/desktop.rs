//! Strict `.desktop` Exec parsing (architecture launcher rules). Never shell.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopApp {
    pub name: String,
    pub exec: Vec<String>,
    pub app_id: Option<String>,
    /// Raw `Icon=` value from the entry: an absolute path or a themed name.
    pub icon: Option<String>,
    /// Lowercased `name`, precomputed so app/window matching never allocates.
    pub name_lc: String,
    /// Lowercased `StartupWMClass`/`app_id`, precomputed.
    pub app_id_lc: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    Empty,
    UnclosedQuote,
}

/// Parse `Exec=` per the Desktop Entry Spec. No shell.
/// `%F`/`%U` → reject. `%f`/`%u` omitted (no file picked). `%%` → `%`.
pub fn parse_exec(
    exec: &str,
    name: &str,
    desktop_path: &str,
    icon: Option<&str>,
) -> Result<Vec<String>, ExecError> {
    let exec = exec.trim();
    if exec.is_empty() {
        return Err(ExecError::Empty);
    }

    let tokens = split_exec(exec)?;
    let mut out = Vec::new();
    for tok in tokens {
        if tok == "%f"
            || tok == "%u"
            || tok == "%F"
            || tok == "%U"
            || tok == "%d"
            || tok == "%D"
            || tok == "%n"
            || tok == "%N"
            || tok == "%v"
            || tok == "%m"
        {
            continue;
        }
        if tok == "%c" {
            out.push(name.to_string());
            continue;
        }
        if tok == "%k" {
            out.push(desktop_path.to_string());
            continue;
        }
        if tok == "%i" {
            if let Some(icon) = icon {
                out.push("--icon".into());
                out.push(icon.to_string());
            }
            continue;
        }
        out.push(expand_codes(&tok, name, desktop_path));
    }
    if out.is_empty() {
        return Err(ExecError::Empty);
    }
    Ok(out)
}

fn expand_codes(tok: &str, name: &str, desktop_path: &str) -> String {
    let mut s = String::new();
    let mut chars = tok.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('%') => s.push('%'),
                Some('c') => s.push_str(name),
                Some('k') => s.push_str(desktop_path),
                Some('f' | 'u' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' | 'i') => {}
                Some(other) => {
                    s.push('%');
                    s.push(other);
                }
                None => s.push('%'),
            }
        } else {
            s.push(c);
        }
    }
    s
}

fn split_exec(exec: &str) -> Result<Vec<String>, ExecError> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = exec.chars().peekable();
    let mut in_quote = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            '\\' if in_quote => {
                if let Some(n) = chars.next() {
                    buf.push(n);
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            _ => buf.push(c),
        }
    }
    if in_quote {
        return Err(ExecError::UnclosedQuote);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    Ok(out)
}

pub fn parse_desktop_entry(text: &str, path: &Path) -> Option<DesktopApp> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut terminal = false;
    let mut hidden = false;
    let mut no_display = false;
    let mut try_exec = None;
    let mut ty = None;
    let mut app_id = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "Type" => ty = Some(v.to_string()),
            "Name" => name = Some(unescape_desktop(v)),
            "Exec" => exec = Some(v.to_string()),
            "Icon" => icon = Some(v.to_string()),
            "Terminal" => terminal = v.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = v.eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = v.eq_ignore_ascii_case("true"),
            "TryExec" => try_exec = Some(v.to_string()),
            "StartupWMClass" => app_id = Some(v.to_string()),
            _ => {}
        }
    }

    if hidden || no_display {
        return None;
    }
    if ty.as_deref().is_some_and(|t| t != "Application") {
        return None;
    }
    let name = name?;
    let exec = exec?;
    let path_str = path.to_string_lossy();
    let mut argv = parse_exec(&exec, &name, &path_str, icon.as_deref()).ok()?;
    if terminal {
        let Some(term) = crate::files::resolve_terminal() else {
            return None;
        };
        let mut wrapped = vec![term, "-e".into()];
        wrapped.append(&mut argv);
        argv = wrapped;
    }
    if let Some(te) = try_exec {
        if !try_exec_ok(&te) {
            return None;
        }
    }
    let name_lc = name.to_lowercase();
    let app_id_lc = app_id.as_deref().map(|s| s.to_lowercase());
    Some(DesktopApp {
        name,
        exec: argv,
        app_id,
        icon,
        name_lc,
        app_id_lc,
    })
}

fn unescape_desktop(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\\", "\\")
}

fn try_exec_ok(cmd: &str) -> bool {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return p.is_file();
    }
    let Ok(path) = std::env::var("PATH") else {
        return true;
    };
    path.split(':')
        .any(|dir| Path::new(dir).join(cmd).is_file())
}

pub fn scan_applications() -> Vec<DesktopApp> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':') {
        if !d.is_empty() {
            dirs.push(PathBuf::from(d).join("applications"));
        }
    }

    let mut apps = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let id = path.file_name().map(|s| s.to_os_string());
            if let Some(id) = &id {
                if !seen.insert(id.clone()) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(app) = parse_desktop_entry(&text, &path) {
                apps.push(app);
            }
        }
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_multi_file_codes() {
        assert_eq!(
            parse_exec("foo %F", "Foo", "/x.desktop", None).unwrap(),
            vec!["foo"]
        );
        assert_eq!(
            parse_exec("foo %U", "Foo", "/x.desktop", None).unwrap(),
            vec!["foo"]
        );
        assert_eq!(
            parse_exec("foo %f %F", "Foo", "/x.desktop", None).unwrap(),
            vec!["foo"]
        );
    }

    #[test]
    fn terminal_true_wraps_exec_with_resolved_terminal() {
        unsafe { std::env::set_var("TERMINAL", "xterm") };
        let entry = "[Desktop Entry]\nType=Application\nName=Top\nExec=top\nTerminal=true\n";
        let app = parse_desktop_entry(entry, std::path::Path::new("/x.desktop"));
        assert_eq!(
            app.map(|a| a.exec),
            Some(vec![
                "xterm".to_string(),
                "-e".to_string(),
                "top".to_string()
            ])
        );
    }

    #[test]
    fn strips_single_file_codes() {
        assert_eq!(
            parse_exec("foo %f %u", "Foo", "/x.desktop", None).unwrap(),
            vec!["foo"]
        );
    }

    #[test]
    fn quoted_args_and_percent() {
        assert_eq!(
            parse_exec(r#"bar --title "a b" %%"#, "Bar", "/x.desktop", None).unwrap(),
            vec!["bar", "--title", "a b", "%"]
        );
    }

    #[test]
    fn no_shell_metacharacters_as_shell() {
        let argv = parse_exec("echo hello; rm -rf /", "X", "/x.desktop", None).unwrap();
        assert_eq!(argv[0], "echo");
        assert!(argv.iter().any(|a| a.contains(';')));
    }

    #[test]
    fn skips_hidden_and_nondisplay() {
        let p = Path::new("/tmp/x.desktop");
        assert!(
            parse_desktop_entry(
                "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n",
                p
            )
            .is_none()
        );
        assert!(
            parse_desktop_entry(
                "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n",
                p
            )
            .is_none()
        );
    }

    #[test]
    fn parses_application() {
        let app = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Alacritty\nExec=alacritty\n",
            Path::new("/usr/share/applications/Alacritty.desktop"),
        )
        .unwrap();
        assert_eq!(app.name, "Alacritty");
        assert_eq!(app.exec, vec!["alacritty"]);
    }
}
