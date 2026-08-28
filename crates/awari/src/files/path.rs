//! Path-shaped query resolution (`~/`, `/`, `.`, separators): expand `~` and
//! decide between browsing a real directory and fuzzy-matching its trailing
//! segment in the parent.

use std::path::{Path, PathBuf};

/// `~`, `/`, `.`, or any path separator — files win ranking for these.
pub fn is_path_shaped(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('~') || q.starts_with('.') || q.contains('/')
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// If `raw` is a path-shaped query, return the directory to browse and the
/// fragment to match within it. When the full path is itself an existing
/// directory (e.g. `~/dev` or `~/dev/`), browse its contents; otherwise search
/// its parent for the trailing segment (e.g. `~/dev` where `dev` does not yet
/// exist, or `~/dev/aw`). Returns `None` if neither the path nor its parent is
/// a real directory.
pub(super) fn path_query_dir(raw: &str) -> Option<(PathBuf, String)> {
    if !is_path_shaped(raw) {
        return None;
    }
    let trimmed = raw.trim();
    let expanded = if let Some(rest) = trimmed.strip_prefix('~') {
        let home = home_dir()?;
        if rest.is_empty() || rest.starts_with('/') {
            home.join(rest.trim_start_matches('/'))
        } else {
            home.join(rest)
        }
    } else {
        PathBuf::from(trimmed)
    };
    if expanded.is_dir() {
        return Some((expanded, String::new()));
    }
    // Otherwise treat it as a partial: search the parent for the trailing
    // segment. Only when the parent exists and isn't the filesystem root
    // (scanning `/` would be pathological).
    let parent = expanded.parent().filter(|p| !p.as_os_str().is_empty())?;
    if !parent.is_dir() || parent.as_os_str() == "/" {
        return None;
    }
    let term = expanded
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some((parent.to_path_buf(), term))
}

/// True when `root` is (or canonicalizes to) `$HOME`, which enables fff's home
/// dir scanning on it.
pub(super) fn is_home_root(root: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    if root == home {
        return true;
    }
    match (root.canonicalize(), home.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_query_dir_resolves_existing_directory() {
        let base =
            std::env::temp_dir().join(format!("awari_pathq_existing_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // Trailing slash: browse the directory's contents (empty fragment).
        let got = path_query_dir(&format!("{}/", base.display()));
        // No trailing slash on an existing directory: still browse it.
        let got2 = path_query_dir(&base.display().to_string());
        let _ = std::fs::remove_dir_all(&base);
        let (dir, frag) = got.expect("existing directory with trailing slash should resolve");
        assert_eq!(
            dir.as_os_str()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string(),
            base.to_string_lossy().to_string(),
            "resolved dir should match the queried directory"
        );
        assert_eq!(frag, "", "browsing an existing dir uses an empty fragment");
        let (dir2, frag2) = got2.expect("existing directory without slash should resolve");
        assert_eq!(dir2, base, "no-slash existing dir resolves to itself");
        assert_eq!(frag2, "", "no-slash existing dir uses an empty fragment");
    }

    #[test]
    fn path_query_dir_resolves_parent_for_partial() {
        let base = std::env::temp_dir().join(format!("awari_pathq_partial_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let got = path_query_dir(&format!("{}/aw", base.display()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, frag) = got.expect("partial under an existing dir should resolve");
        assert_eq!(dir, base, "partial searches the parent directory");
        assert_eq!(frag, "aw", "partial matches the trailing segment");
    }

    #[test]
    fn path_query_dir_ignores_missing_and_plain() {
        assert!(path_query_dir("firefox").is_none());
        assert!(path_query_dir("/nonexistent_awari_xyz_123/").is_none());
    }
}