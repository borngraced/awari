//! `~/.config/awari/config.kdl`. Unknown keys are ignored. No `exec`.
//! Parsing lives in [`parse`] (a hand-rolled KDL subset lexer, separate so the
//! config model and loader stay readable).

use std::path::PathBuf;

use crate::ui::theme::Theme;

mod parse;
pub use parse::parse;

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

/// `fff { }` block: toggles for the fff-search file pickers.
/// `base_path` and `enable_home_dir_scanning` are per-root derivations
/// (the configured root and whether that root is `$HOME`), so they are not
/// configurable here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FffConfig {
    /// Spawn the background file watcher. Default `true`.
    pub watch: bool,
    /// Allow indexing the filesystem root (`/`). Default `true`.
    pub enable_fs_root_scanning: bool,
    /// Pre-populate mmap caches for top-frecency files. Default `false`.
    pub enable_mmap_cache: bool,
    /// Build a content index after the initial scan. Default `false`.
    pub enable_content_indexing: bool,
    /// Follow symbolic links during indexing. Default `false`.
    pub follow_symlinks: bool,
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
    pub fff: FffConfig,
    pub sources: SourcesConfig,
    /// Max total rows in the All view (apps + files + windows). Default 30.
    pub max_results: usize,
    /// Keep the GPU overlay in memory (hidden when closed) instead of quitting it
    /// on every dismiss. When kept alive, re-opens are instant; when dropped, the
    /// GUI exits on dismiss and only a tiny shell stays up (re-open rebuilds the
    /// interface). Default true (keep alive). The daemon flag `--no-keep-alive`
    /// forces drop mode.
    pub keep_alive: bool,
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

impl Default for FffConfig {
    fn default() -> Self {
        Self {
            watch: true,
            enable_fs_root_scanning: true,
            enable_mmap_cache: false,
            enable_content_indexing: false,
            follow_symlinks: false,
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
            fff: FffConfig::default(),
            sources: SourcesConfig::default(),
            max_results: 30,
            keep_alive: true,
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

pub(super) fn home_dir() -> Option<PathBuf> {
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
    use crate::ui::theme;

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

    #[test]
    fn files_max_results_parses_inside_block() {
        let c = parse("files { max-results 80 }\nmax-results 40");
        assert_eq!(c.files.max_results, 80);
        assert_eq!(c.max_results, 40);
        let d = parse("files { max_results 1 }");
        assert_eq!(d.files.max_results, 1);
    }

    #[test]
    fn fff_flags_parse_with_defaults() {
        let c = parse(
            r#"fff { watch false fs-root-scanning true mmap-cache true content-indexing true follow-symlinks true }"#,
        );
        assert!(!c.fff.watch);
        assert!(c.fff.enable_fs_root_scanning);
        assert!(c.fff.enable_mmap_cache);
        assert!(c.fff.enable_content_indexing);
        assert!(c.fff.follow_symlinks);
        let d = parse("fff { }");
        assert!(d.fff.watch);
        assert!(d.fff.enable_fs_root_scanning);
        assert!(!d.fff.enable_mmap_cache);
        assert!(!d.fff.enable_content_indexing);
        assert!(!d.fff.follow_symlinks);
    }

    #[test]
    fn quoted_values_with_spaces_stay_one_token() {
        let c = parse(r#"theme { font "JetBrains Mono" }"#);
        assert_eq!(c.theme.font.as_deref(), Some("JetBrains Mono"));
        let d = parse(r#"files { roots "~/My Documents" }"#);
        assert!(!d.files.roots.is_empty());
        assert!(d.files.roots.iter().any(|p| p.ends_with("My Documents")));
    }

    #[test]
    fn raw_strings_and_comments_inside_strings() {
        let c = parse(r##"theme { font #"JetBrains Mono"# }"##);
        assert_eq!(c.theme.font.as_deref(), Some("JetBrains Mono"));
        // A `//` inside a quoted path is data, not a comment.
        let d = parse(r#"files { roots "~/a//b" }"#);
        assert!(d.files.roots.iter().any(|p| p.ends_with("a//b")));
    }
}