//! Lexer + parser for the KDL subset the config accepts. Kept separate from
//! the config model so the (small) parsing logic is easy to reason about.

use std::path::PathBuf;
use std::sync::Arc;

use crate::ui::theme::{self, Theme};

use super::{Config, FffConfig, FilesConfig, MotionConfig, SourcesConfig, home_dir};

pub fn parse(src: &str) -> Config {
    let tokens = kdl_tokens(src);
    let mut cfg = Config::default();
    if let Some(body) = block_body(&tokens, "motion") {
        parse_motion_body(body, &mut cfg.motion);
    }
    if let Some(body) = block_body(&tokens, "theme") {
        parse_theme_body(body, &mut cfg.theme);
    }
    if let Some(body) = block_body(&tokens, "files") {
        parse_files_body(body, &mut cfg.files);
    }
    if let Some(body) = block_body(&tokens, "fff") {
        parse_fff_body(body, &mut cfg.fff);
    }
    if let Some(body) = block_body(&tokens, "sources") {
        parse_sources_body(body, &mut cfg.sources);
    }
    parse_top_level(&tokens, &mut cfg);
    cfg
}

/// hand-roll the lexer instead of pulling a KDL parser crate on purpose:
/// the maintained parsers (`kdl-rs`, `knus`) are built on parser-combinator
/// stacks that would ship `winnow` (and, with spans, `miette`) into the
/// daemon for a file read once at startup. This config is a small, fixed
/// subset of KDL — blocks, quoted strings, numbers, booleans — so a ~50-line
/// lexer keeps the binary lean. The one part that must be done right is
/// quoting: values with spaces (`font "JetBrains Mono"`,
/// `roots "~/My Documents"`) have to stay a single token, escapes in strings
/// must be unescaped, and `//` comments inside strings must not be stripped.
fn kdl_tokens(src: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut chars = src.chars().peekable();
    let mut word = String::new();
    let flush = |tokens: &mut Vec<String>, word: &mut String| {
        if !word.is_empty() {
            tokens.push(std::mem::take(word));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                flush(&mut tokens, &mut word);
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(&esc) = chars.peek() {
                                s.push(esc);
                                chars.next();
                            }
                        }
                        '"' => break,
                        c => s.push(c),
                    }
                }
                tokens.push(s);
            }
            '#' if chars.peek() == Some(&'"') => {
                // KDL raw string (`#"…"#`): no escapes, ends at `"#`.
                flush(&mut tokens, &mut word);
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    if c == '"' && chars.peek() == Some(&'#') {
                        chars.next();
                        break;
                    }
                    s.push(c);
                }
                tokens.push(s);
            }
            '/' if chars.peek() == Some(&'/') => {
                // `//` comments out to end of line (KDL has no block comments
                // or escaped `\/` inside strings; quote handling above means a
                // path like "foo//bar" stays intact).
                flush(&mut tokens, &mut word);
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            c if c.is_whitespace() => flush(&mut tokens, &mut word),
            c => word.push(c),
        }
    }
    flush(&mut tokens, &mut word);
    tokens
}

/// The slice between `name {` and its matching `}`, token-wise.
fn block_body<'a>(tokens: &'a [String], name: &str) -> Option<&'a [String]> {
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] == name && tokens[i + 1] == "{" {
            let start = i + 2;
            let mut depth = 1usize;
            let mut j = start;
            loop {
                match tokens.get(j)?.as_str() {
                    "{" => depth += 1,
                    "}" => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(&tokens[start..j]);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        i += 1;
    }
    None
}

fn parse_motion_body(body: &[String], m: &mut MotionConfig) {
    let mut i = 0;
    while i + 1 < body.len() {
        let key = body[i].as_str();
        let val = body[i + 1].as_str();
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

fn parse_theme_body(body: &[String], t: &mut Theme) {
    let mut i = 0;
    while i + 1 < body.len() {
        let key = body[i].as_str();
        let val = body[i + 1].as_str();
        match key {
            "font" => {
                if !val.is_empty() && val != "default" {
                    t.font = Some(Arc::from(val));
                }
                i += 2;
            }
            "font-size" | "font_size" => {
                if let Ok(n) = val.parse::<u32>()
                    && (8..=64).contains(&n)
                {
                    t.font_size = Some(n);
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

fn parse_files_body(body: &[String], f: &mut FilesConfig) {
    let mut i = 0;
    while i < body.len() {
        let key = body[i].as_str();
        if key == "roots" {
            i += 1;
            while i < body.len() && !looks_like_key(&body[i]) {
                if let Some(p) = expand_root(&body[i]) {
                    f.roots.push(p);
                }
                i += 1;
            }
            continue;
        }
        if i + 1 < body.len() {
            let val = body[i + 1].as_str();
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
                "max_results" | "max-results" => {
                    if let Ok(n) = val.parse::<usize>() {
                        f.max_results = n.max(1);
                    }
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
fn parse_top_level(tokens: &[String], cfg: &mut Config) {
    let mut depth = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "{" => depth += 1,
            "}" => depth = depth.saturating_sub(1),
            "max_results" | "max-results" if depth == 0 => {
                if i + 1 < tokens.len()
                    && let Ok(n) = tokens[i + 1].parse::<usize>()
                {
                    cfg.max_results = n.max(1);
                }
                i += 2;
                continue;
            }
            "keep_alive" | "keep-alive" if depth == 0 => {
                if i + 1 < tokens.len() {
                    cfg.keep_alive = is_true(&tokens[i + 1]);
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

fn parse_fff_body(body: &[String], f: &mut FffConfig) {
    let mut i = 0;
    while i + 1 < body.len() {
        let key = body[i].as_str();
        let val = body[i + 1].as_str();
        match key {
            "watch" => f.watch = is_true(val),
            "fs_root_scanning"
            | "fs-root-scanning"
            | "enable_fs_root_scanning"
            | "enable-fs-root-scanning" => f.enable_fs_root_scanning = is_true(val),
            "mmap_cache" | "mmap-cache" | "enable_mmap_cache" | "enable-mmap-cache" => {
                f.enable_mmap_cache = is_true(val)
            }
            "content_indexing"
            | "content-indexing"
            | "enable_content_indexing"
            | "enable-content-indexing" => f.enable_content_indexing = is_true(val),
            "follow_symlinks" | "follow-symlinks" => f.follow_symlinks = is_true(val),
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
}

fn parse_sources_body(body: &[String], s: &mut SourcesConfig) {
    let mut i = 0;
    while i + 1 < body.len() {
        let key = body[i].as_str();
        let val = body[i + 1].as_str();
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
    matches!(
        s,
        "windows"
            | "files"
            | "reduced"
            | "accent"
            | "roots"
            | "index-lockfiles"
            | "index_lockfiles"
            | "regex"
            | "max-results"
            | "max_results"
    )
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
    if p == *"/" {
        return None;
    }
    Some(p)
}
