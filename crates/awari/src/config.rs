//! `~/.config/awari/config.kdl`. Unknown keys are ignored. No `exec`.

use std::path::PathBuf;

use crate::ui::theme::{self, Theme};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionConfig {
    pub reduced: bool,
    pub duration_ms: u32,
}

/// Directories `fff-search` may index. Never `/`. `~` expands. Empty = XDG user dirs that exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesConfig {
    pub roots: Vec<PathBuf>,
    /// Index lock files (Cargo.lock, package-lock.json, *.lock, …).
    /// Default `false` → lock files are hidden from results.
    pub index_lockfiles: bool,
    /// Treat file queries as regex. Default `false`. The `r:` query prefix
    /// forces regex per-query regardless of this setting.
    pub regex: bool,
    /// Max file rows to display. Default 50.
    pub max_results: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcesConfig {
    pub windows: bool,
    pub files: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub motion: MotionConfig,
    pub theme: Theme,
    pub files: FilesConfig,
    pub sources: SourcesConfig,
    /// Max total rows in the All view (apps + files + windows). Default 30.
    pub max_results: usize,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            reduced: false,
            duration_ms: 140,
        }
    }
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            index_lockfiles: false,
            regex: false,
            max_results: 50,
        }
    }
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            windows: true,
            files: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            motion: MotionConfig::default(),
            theme: Theme::default(),
            files: FilesConfig::default(),
            sources: SourcesConfig::default(),
            max_results: 30,
        }
    }
}

impl FilesConfig {
    /// Empty `roots` → existing XDG user dirs. `/` is dropped.
    pub fn resolved_roots(&self) -> Vec<PathBuf> {
        let listed: Vec<PathBuf> = self
            .roots
            .iter()
            .filter(|p| p.as_os_str() != "/" && p.as_os_str() != "/")
            .cloned()
            .collect();
        if !listed.is_empty() {
            return listed;
        }
        xdg_user_dirs()
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("awari/config.kdl")
}

pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(src) => {
            tracing::info!(path = %path.display(), "loaded config");
            parse(&src)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "config unreadable; defaults");
            Config::default()
        }
    }
}

pub fn parse(src: &str) -> Config {
    let stripped = strip_comments(src);
    let mut cfg = Config::default();
    if let Some(body) = block_body(&stripped, "motion") {
        parse_motion_body(body, &mut cfg.motion);
    }
    if let Some(body) = block_body(&stripped, "theme") {
        parse_theme_body(body, &mut cfg.theme);
    }
    if let Some(body) = block_body(&stripped, "files") {
        parse_files_body(body, &mut cfg.files);
    }
    if let Some(body) = block_body(&stripped, "sources") {
        parse_sources_body(body, &mut cfg.sources);
    }
    parse_top_level(&stripped, &mut cfg);
    cfg
}

fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn block_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name} {{");
    let start = src.find(&key)? + key.len();
    let rest = src.get(start..)?;
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_motion_body(body: &str, m: &mut MotionConfig) {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let key = tokens[i];
        let val = tokens[i + 1].trim_matches('"');
        match key {
            "reduced" => {
                m.reduced = is_true(val);
                i += 2;
            }
            "duration-ms" | "duration_ms" => {
                if let Ok(d) = val.parse::<u32>() {
                    m.duration_ms = d.min(1000);
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
}

fn parse_theme_body(body: &str, t: &mut Theme) {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let key = tokens[i];
        let val = tokens[i + 1].trim_matches('"');
        match key {
            "font" => {
                if !val.is_empty() && val != "default" {
                    t.font = Some(val.to_string());
                }
                i += 2;
            }
            "font-size" | "font_size" => {
                if let Ok(n) = val.parse::<u32>() {
                    if (8..=64).contains(&n) {
                        t.font_size = Some(n);
                    }
                }
                i += 2;
            }
            "name" => {
                if let Some(p) = theme::Theme::preset(val) {
                    *t = p;
                }
                i += 2;
            }
            _ => {
                if let Some(c) = theme::parse_hex(val) {
                    match key {
                        "accent" => t.accent = c,
                        "accent-dim" | "accent_dim" | "select" => t.accent_dim = c,
                        "bg" => t.bg = c,
                        "panel" => t.panel = c,
                        "raise" | "hover" | "surface" => t.raise = c,
                        "border" => t.border = c,
                        "text" | "fg" => t.text = c,
                        "text-dim" | "text_dim" | "muted" => t.text_dim = c,
                        "text-faint" | "text_faint" | "faint" => t.text_faint = c,
                        "scrim" => t.scrim = c,
                        _ => {}
                    }
                }
                i += 2;
            }
        }
    }
}

fn parse_files_body(body: &str, f: &mut FilesConfig) {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "roots" {
            i += 1;
            while i < tokens.len() && tokens[i] != "roots" {
                let raw = tokens[i].trim_matches('"');
                if raw == "roots" {
                    break;
                }
                if looks_like_key(raw) {
                    break;
                }
                if let Some(p) = expand_root(raw) {
                    f.roots.push(p);
                }
                i += 1;
            }
            continue;
        }
        if i + 1 < tokens.len() {
            let key = tokens[i];
            let val = tokens[i + 1].trim_matches('"');
            match key {
                "index_lockfiles" | "index-lockfiles" => {
                    f.index_lockfiles = is_true(val);
                    i += 2;
                    continue;
                }
                "regex" => {
                    f.regex = is_true(val);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
}

/// Parse top-level scalar keys (outside any `{ … }` block). Currently only
/// `max-results`, which caps the All-view result list.
fn parse_top_level(src: &str, cfg: &mut Config) {
    let tokens: Vec<&str> = src.split_whitespace().collect();
    let mut depth = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "{" => depth += 1,
            "}" => depth = depth.saturating_sub(1),
            "max_results" | "max-results" if depth == 0 => {
                if i + 1 < tokens.len() {
                    if let Ok(n) = tokens[i + 1].parse::<usize>() {
                        cfg.max_results = n.max(1);
                    }
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

fn parse_sources_body(body: &str, s: &mut SourcesConfig) {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let key = tokens[i];
        let val = tokens[i + 1].trim_matches('"');
        match key {
            "windows" => s.windows = is_true(val),
            "files" => s.files = is_true(val),
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
}

fn is_true(val: &str) -> bool {
    matches!(val, "true" | "1" | "yes" | "on")
}

fn looks_like_key(s: &str) -> bool {
    matches!(s, "windows" | "files" | "reduced" | "accent")
}

fn expand_root(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || raw == "/" {
        return None;
    }
    let p = if raw == "~" {
        home_dir()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if p == PathBuf::from("/") {
        return None;
    }
    Some(p)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn xdg_user_dirs() -> Vec<PathBuf> {
    let home = match home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    [
        "Desktop",
        "Documents",
        "Downloads",
        "Music",
        "Pictures",
        "Videos",
    ]
    .into_iter()
    .map(|n| home.join(n))
    .filter(|p| p.is_dir())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults() {
        let c = parse("");
        assert_eq!(
            c.motion,
            MotionConfig {
                reduced: false,
                duration_ms: 140
            }
        );
        assert_eq!(c.theme, Theme::default());
        assert!(c.files.roots.is_empty());
        assert!(c.sources.files);
    }

    #[test]
    fn unknown_keys_ignored() {
        let c = parse(
            r#"
            bar { height 32 }
            motion { reduced true duration-ms 0 }
            "#,
        );
        assert!(c.motion.reduced);
        assert_eq!(c.motion.duration_ms, 0);
    }

    #[test]
    fn motion_clamp() {
        let big = parse(r#"motion { duration-ms 99999 }"#);
        assert_eq!(big.motion.duration_ms, 1000);
        let bad = parse(r#"motion { duration-ms "abc" }"#);
        assert_eq!(bad.motion.duration_ms, 140);
    }

    #[test]
    fn theme_font_tokens() {
        let c = parse("theme { font \"Inter\" font-size 14 }");
        assert_eq!(c.theme.font.as_deref(), Some("Inter"));
        assert_eq!(c.theme.font_size, Some(14));
        // "default" clears back to the system font; out-of-range sizes drop.
        let d = parse("theme { font \"default\" font-size 999 }");
        assert_eq!(d.theme.font, None);
        assert_eq!(d.theme.font_size, None);
    }

    #[test]
    fn theme_overrides_accent() {
        let c = parse("theme { accent \"#ff00aa\" text \"#eeeeee\" }");
        assert_eq!(c.theme.accent, theme::Color::rgb(0xff00aa));
        assert_eq!(c.theme.text, theme::Color::rgb(0xeeeeee));
        assert_eq!(c.theme.panel, Theme::default().panel);
    }

    #[test]
    fn theme_preset_name_and_override() {
        let c = parse(r##"theme { name "gruvbox" accent "#ff0000" }"##);
        assert_eq!(c.theme.panel, Theme::gruvbox().panel);
        assert_eq!(c.theme.accent, theme::Color::rgb(0xff0000));
        // Unknown preset name is ignored, leaving the default theme intact.
        let d = parse(r#"theme { name "nope" }"#);
        assert_eq!(d.theme, Theme::default());
    }

    #[test]
    fn files_roots_drop_slash_and_expand_tilde() {
        let c = parse("files { roots \"/\" \"~/Documents\" \"/var/tmp\" }");
        assert!(c.files.roots.iter().all(|p| p.as_os_str() != "/"));
        assert!(c.files.roots.iter().any(|p| p.ends_with("Documents")));
        assert!(
            c.files
                .roots
                .iter()
                .any(|p| p == &PathBuf::from("/var/tmp"))
        );
    }

    #[test]
    fn sources_can_disable_chips() {
        let c = parse("sources { files true windows false }");
        assert!(c.sources.files);
        assert!(!c.sources.windows);
    }

    #[test]
    fn files_flags_parse() {
        let c = parse("files { index_lockfiles true regex true }");
        assert!(c.files.index_lockfiles);
        assert!(c.files.regex);
        // Defaults are conservative: lock files hidden, regex off.
        let d = parse("files { roots \"~/Documents\" }");
        assert!(!d.files.index_lockfiles);
        assert!(!d.files.regex);
        assert!(d.files.roots.iter().any(|p| p.ends_with("Documents")));
    }
}
