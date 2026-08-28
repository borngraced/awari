//! Regex query resolution: detect `r:`-prefixed or config-enforced regex
//! queries, compile them, and derive a fuzzy hint for the search.

use regex::Regex;

/// Memoizes the last compiled main/transient pattern so identical re-queries
/// within a session skip recompilation (which otherwise runs on every keystroke
/// in regex mode). Dropped on dismiss so a session's worst pattern doesn't pin
/// its regex engine buffers for the daemon's lifetime.
#[derive(Default)]
pub(super) struct RegexCaches {
    pub(super) main: Option<(String, Regex)>,
    pub(super) term: Option<(String, Regex)>,
}

/// Resolve whether `raw` is a regex query and compile it into `cache`. The `r:`
/// prefix forces regex mode per-query; otherwise only the `files.regex` config
/// does. A failed compile falls back to no regex rather than panicking.
pub(super) fn resolve_regex(
    cache: &mut Option<(String, Regex)>,
    raw: &str,
    global_regex: bool,
) -> (String, Option<Regex>) {
    let (pattern, want) = if let Some(p) = raw.strip_prefix("r:") {
        (p.to_string(), true)
    } else if global_regex {
        (raw.to_string(), true)
    } else {
        (raw.to_string(), false)
    };
    if !want {
        *cache = None;
        return (pattern, None);
    }
    if let Some((prev, re)) = cache
        && *prev == pattern
    {
        return (pattern, Some(re.clone()));
    }
    match Regex::new(&pattern) {
        Ok(re) => {
            *cache = Some((pattern.clone(), re.clone()));
            (pattern, Some(re))
        }
        Err(e) => {
            tracing::debug!(%e, "regex compile failed; ignoring regex filter");
            *cache = None;
            (pattern, None)
        }
    }
}

/// Fuzzy-friendly hint derived from a regex pattern (strips metacharacters) so
/// FFF still returns candidate paths for the regex to refine.
pub(super) fn regex_hint(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            c.is_alphanumeric() || *c == '/' || *c == '.' || *c == ' ' || *c == '-' || *c == '_'
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_resolution() {
        let mut cache = None;
        // `r:` prefix forces regex and strips the prefix.
        let (pat, re) = resolve_regex(&mut cache, "r:foo", false);
        assert_eq!(pat, "foo");
        assert!(re.is_some());
        // No prefix and global off → plain fuzzy (no regex).
        assert!(resolve_regex(&mut cache, "foo", false).1.is_none());
        // Global on → regex even without prefix.
        assert!(resolve_regex(&mut cache, "foo", true).1.is_some());
        // Invalid pattern → falls back to no regex rather than panicking.
        assert!(resolve_regex(&mut cache, "r:[", false).1.is_none());
    }

    #[test]
    fn regex_hint_strips_metacharacters() {
        assert_eq!(regex_hint(r"\.rs$"), ".rs");
        assert_eq!(regex_hint(r"src/.*\.rs"), "src/..rs");
    }
}