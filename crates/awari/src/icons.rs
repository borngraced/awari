//! Freedesktop icon name → file path. Resolution only; gpui's `img()` does
//! the decoding (PNG raster + SVG via its renderer) and caches by path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Raster/vector extensions gpui can decode. XPM is intentionally absent.
const ICON_EXTS: [&str; 2] = ["png", "svg"];

/// Theme directories probed in order. 48 first (crisp at 20–22px), then
/// scalable (vector), then larger rasters.
const THEME_SUBDIRS: [&str; 6] = ["48x48", "scalable", "32x32", "64x64", "128x128", "256x256"];

/// Resolve `name` against the user's real XDG data dirs, memoized.
pub fn resolve(name: &str) -> Option<Arc<Path>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<Path>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok()?.get(name) {
        return hit.clone();
    }
    let resolved = resolve_uncached(name);
    let arc = resolved.map(Arc::from);
    if let Ok(mut map) = cache.lock() {
        map.insert(name.to_string(), arc.clone());
    }
    arc
}

fn resolve_uncached(name: &str) -> Option<PathBuf> {
    resolve_in(name, &data_dirs())
}

/// Lookup order: absolute path → `<data>/pixmaps` → hicolor theme sizes.
pub fn resolve_in(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let raw = Path::new(name);
    if raw.is_absolute() {
        return raw.is_file().then(|| raw.to_path_buf());
    }
    for dir in dirs {
        for ext in ICON_EXTS {
            let p = dir.join("pixmaps").join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for dir in dirs {
        for size in THEME_SUBDIRS {
            for ext in ICON_EXTS {
                let p = dir
                    .join("icons/hicolor")
                    .join(size)
                    .join("apps")
                    .join(format!("{name}.{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// `$XDG_DATA_HOME` (default `~/.local/share`) then `$XDG_DATA_DIRS`
/// (default `/usr/local/share:/usr/share`).
fn data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home_data) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(home_data));
    } else if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".local/share"));
    }
    let dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in dirs.split(':').filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(d));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, slice};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awari-icons-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_and_missing_resolve_to_none() {
        let dirs = vec![tmpdir("missing")];
        assert_eq!(resolve_in("", &dirs), None);
        assert_eq!(resolve_in("does-not-exist", &dirs), None);
    }

    #[test]
    fn absolute_path_must_be_a_file() {
        let file = tmpdir("abs").join("icon.png");
        fs::write(&file, b"png").unwrap();
        assert_eq!(resolve_in(file.to_str().unwrap(), &[]), Some(file));
        let ghost = tmpdir("abs-ghost").join("ghost.png");
        assert_eq!(resolve_in(ghost.to_str().unwrap(), &[]), None);
    }

    #[test]
    fn pixmaps_beats_hicolor_and_size_order_holds() {
        let dir = tmpdir("order");
        fs::create_dir_all(dir.join("pixmaps")).unwrap();
        let hicolor48 = dir.join("icons/hicolor/48x48/apps");
        fs::create_dir_all(&hicolor48).unwrap();

        // Only hicolor present → found there.
        let only_theme = hicolor48.join("app.png");
        fs::write(&only_theme, b"png").unwrap();
        assert_eq!(resolve_in("app", slice::from_ref(&dir)), Some(only_theme));

        // pixmaps wins once it exists.
        fs::write(dir.join("pixmaps/app.png"), b"png").unwrap();
        assert_eq!(
            resolve_in("app", slice::from_ref(&dir)),
            Some(dir.join("pixmaps/app.png"))
        );
    }

    #[test]
    fn prefers_48_then_scalable_then_larger() {
        let dir = tmpdir("sizes");
        let base = dir.join("icons/hicolor");
        for sub in ["48x48/apps", "scalable/apps", "128x128/apps"] {
            fs::create_dir_all(base.join(sub)).unwrap();
        }
        fs::write(base.join("scalable/apps/app.svg"), b"<svg/>").unwrap();
        fs::write(base.join("128x128/apps/app.png"), b"png").unwrap();
        // scalable sits ahead of larger rasters in the probe order.
        assert_eq!(
            resolve_in("app", slice::from_ref(&dir)),
            Some(base.join("scalable/apps/app.svg"))
        );

        fs::write(base.join("48x48/apps/app.png"), b"png").unwrap();
        assert_eq!(
            resolve_in("app", &[dir]),
            Some(base.join("48x48/apps/app.png"))
        );
    }

    #[test]
    fn svg_only_app_resolves() {
        let dir = tmpdir("svg-only");
        let apps = dir.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("vec.svg"), b"<svg/>").unwrap();
        assert_eq!(resolve_in("vec", &[dir]), Some(apps.join("vec.svg")));
    }
}
