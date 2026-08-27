use gpui::SharedString;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::types::*;
use super::view::build_subtitle;
use crate::desktop::DesktopApp;
use crate::files::FileHit;

pub fn push_capped(
    out: &mut Vec<LauncherRow>,
    cap: Option<usize>,
    rows: impl IntoIterator<Item = LauncherRow>,
) {
    for r in rows {
        if cap.is_some_and(|c| out.len() >= c) {
            return;
        }
        out.push(r);
    }
}

/// Detect a command prefix at the start of `q`. `r:` switches file search to
/// regex mode (handled in the files source), while `>` and `o:` are inline
/// command modes that replace the result list here. Returned as the literal
/// token so callers color it and branch on it from one source of truth.
pub fn command_prefix(q: &str) -> Option<&'static str> {
    if q.starts_with("r:") {
        Some("r:")
    } else if q.starts_with("o:") {
        Some("o:")
    } else if q.starts_with('>') {
        Some(">")
    } else {
        None
    }
}

pub fn command_token_len(q: &str) -> usize {
    command_prefix(q).map_or(0, |p| p.len())
}

/// Inline autocomplete candidate: the untyped remainder of `top_label` when
/// the live query is a case-insensitive prefix of it. `None` when there is
/// nothing to complete (empty query, no prefix match, or label == query).
pub fn ghost_suffix(query: &str, top_label: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let mut qi = query.chars();
    let mut matched = 0usize;
    for (off, lc) in top_label.char_indices() {
        match qi.next() {
            None => {
                let rest = &top_label[matched..];
                return if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                };
            }
            Some(qc) => {
                let hit = qc == lc
                    || (qc.is_ascii() && lc.is_ascii() && qc.eq_ignore_ascii_case(&lc))
                    || qc.to_lowercase().eq(lc.to_lowercase());
                if !hit {
                    return None;
                }
                matched = off + lc.len_utf8();
            }
        }
    }
    None
}

/// What a Tab keypress should do, decided purely from view state.
#[derive(Debug)]
pub enum TabOutcome {
    /// Inline ghost accept: `completed` is the full query, `accepted_off` the
    /// byte offset where the accent-highlighted suffix starts.
    Inline {
        completed: String,
        accepted_off: usize,
    },
    /// Legacy row completion (selected row's path / label / command / result).
    Row(String),
}

pub fn tab_completion(query: &str, rows: &[LauncherRow], selected: usize) -> Option<TabOutcome> {
    if let Some(r) = rows.get(selected)
        && !query.is_empty()
        && command_prefix(query).is_none()
        && ghost_suffix(query, &r.label).is_some()
    {
        return Some(TabOutcome::Inline {
            accepted_off: query.len(),
            completed: r.label.to_string(),
        });
    }
    rows.get(selected).and_then(|row| {
        let completion = match &row.kind {
            RowKind::File { path } => path.display().to_string(),
            RowKind::App { .. } | RowKind::Window { .. } => row.label.to_string(),
            RowKind::Command { command } => command.clone(),
        };
        (!completion.is_empty()).then_some(TabOutcome::Row(completion))
    })
}

pub fn expand_open_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(rest) = trimmed.strip_prefix('~') {
        let home = home?;
        if rest.is_empty() || rest.starts_with('/') {
            Some(home.join(rest.trim_start_matches('/')))
        } else {
            Some(home.join(rest))
        }
    } else if trimmed.starts_with('/') {
        Some(PathBuf::from(trimmed))
    } else {
        home.map(|h| h.join(trimmed))
    }
}

/// Build the result list for an `o:` (open path) query. Lists real entries
/// under the typed path via `read_dir` (so it works outside the configured
/// fff roots), and shows a direct "Open <path>" row only when that exact path
/// exists. Never returns an optimistic row for a nonexistent path.
pub fn open_path_rows(arg: &str, file_max: usize) -> Vec<LauncherRow> {
    let Some(base) = expand_open_path(arg) else {
        return Vec::new();
    };
    let mut rows: Vec<LauncherRow> = Vec::new();
    // Direct "Open <path>" row, only when the typed path already exists.
    if base.exists() {
        rows.push(open_file_row(&base, true));
    }
    // Real results: entries under the parent dir matching the last segment.
    // When the typed path is itself an existing directory, list its contents
    // (empty fragment) rather than searching the dir for its own name.
    let parent = base
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| base.clone());
    let frag: std::borrow::Cow<str> = if base.is_dir() {
        std::borrow::Cow::Borrowed("")
    } else {
        base.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed(""))
    };
    let search_dir = if base.is_dir() { &base } else { &parent };
    if let Some(entries) = read_dir_matching(search_dir, &frag) {
        for p in entries {
            if rows.len() >= file_max {
                break;
            }
            if p == base {
                continue; // already shown as the direct row
            }
            rows.push(open_file_row(&p, false));
        }
    }
    rows
}

pub fn open_file_row(p: &Path, is_direct: bool) -> LauncherRow {
    let label: SharedString = if is_direct {
        SharedString::from(format!("Open “{}”", p.display()))
    } else {
        let fallback = p.display().to_string();
        let lossy = p.file_name().map(|n| n.to_string_lossy());
        let name = lossy.as_deref().unwrap_or(&fallback);
        SharedString::from(name)
    };
    let kind = RowKind::File { path: Arc::from(p) };
    LauncherRow {
        subtitle: build_subtitle(&kind),
        kind,
        label,
        resolved_icon: None,
    }
}

/// List entries of `dir`, filtered by `frag` (subsequence match, case-insensitive)
/// and sorted best-match first. Returns `None` if `dir` isn't a readable directory.
pub fn read_dir_matching(dir: &Path, frag: &str) -> Option<Vec<PathBuf>> {
    let rd = std::fs::read_dir(dir).ok()?;
    let frag_lc = frag.to_lowercase();
    let mut scored: Vec<(i32, PathBuf)> = Vec::new();

    for entry in rd.flatten() {
        let p = entry.path();
        let name = match p.file_name() {
            Some(n) => n.to_string_lossy(),
            None => continue,
        };
        let name_lc = name.to_lowercase();
        let score = if frag_lc.is_empty() {
            Some(0)
        } else {
            crate::files::subsequence_score(&name_lc, &frag_lc)
        };
        if let Some(s) = score {
            scored.push((s, p));
        }
    }

    scored.sort_by_key(|a| std::cmp::Reverse(a.0));

    Some(scored.into_iter().map(|(_, p)| p).collect())
}

/// Score and rank the app and window rows for `query`. This is the expensive
/// part of filtering (`matchq` over every app/window plus a sort); it depends
/// only on the query, the app/window lists, recents, and usage — never on file
/// hits. The Daemon caches the result by `(query, source_gen, category)` so the
/// re-render that fires when file search returns can reuse it instead of
/// re-scoring the whole list.
/// A single open toplevel window as reported by the compositor. `app_id_lc`
/// is the lowercased `app_id`, precomputed so row scoring and icon lookup
/// never re-lowercase it on every keystroke.
pub fn score_app_window(
    query: &str,
    apps: &[DesktopApp],
    windows: &[WindowEntry],
    recents: &[String],
    app_usage: &HashMap<String, u64>,
    app_icons: &HashMap<String, String>,
    category: Category,
) -> (Vec<LauncherRow>, Vec<LauncherRow>) {
    let q = query.trim();
    let empty = q.is_empty();
    let apps_only = category == Category::Apps;
    let files_only = category == Category::Files;
    let windows_only = category == Category::Windows;

    let mut win_scored: Vec<(i64, usize)> = if files_only || apps_only {
        Vec::new()
    } else {
        windows
            .iter()
            .enumerate()
            .filter_map(|(ix, w)| {
                let s = if empty {
                    1
                } else {
                    crate::matchq::score(&w.title, q)
                        .max(w.app_id.as_deref().and_then(|a| crate::matchq::score(a, q)))?
                };
                Some((s, ix))
            })
            .collect()
    };
    if !empty {
        win_scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    }

    let visible_app_ids: HashSet<&str> = win_scored
        .iter()
        .filter_map(|&(_, ix)| windows[ix].app_id_lc.as_deref())
        .collect();

    let mut app_scored: Vec<(i64, &DesktopApp)> = if files_only || windows_only {
        Vec::new()
    } else {
        apps.iter()
            .filter_map(|app| {
                if !apps_only {
                    let ident_hits_window = |probe: &str| visible_app_ids.contains(&probe);
                    if ident_hits_window(&app.name_lc)
                        || ident_hits_window(app.app_id_lc.as_deref().unwrap_or(""))
                    {
                        return None;
                    }
                }
                let s = if empty {
                    1
                } else {
                    let by_name = crate::matchq::score(&app.name, q);
                    let by_id = app
                        .app_id
                        .as_deref()
                        .and_then(|a| crate::matchq::score(a, q));
                    let base = match (by_name, by_id) {
                        o @ (Some(_), Some(_)) => o.0,
                        (a, b) => a.or(b),
                    }?;
                    // Boost repeatedly-launched apps so muscle-memory picks
                    // stay near the top without overriding a strong match.
                    let usage = app_usage.get(&app.name).copied().unwrap_or(0);
                    base + (usage.saturating_sub(1) as i64) * 5
                };
                Some((s, app))
            })
            .collect()
    };
    if empty {
        // Precompute recent positions once instead of scanning `recents`
        // per comparator call.
        let recent_pos: HashMap<&str, usize> = recents
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();

        app_scored.sort_by(|a, b| {
            let ra = recent_pos.get(a.1.name.as_str()).copied();
            let rb = recent_pos.get(b.1.name.as_str()).copied();
            let r = ra.unwrap_or(usize::MAX).cmp(&rb.unwrap_or(usize::MAX));
            if r != std::cmp::Ordering::Equal {
                return r;
            }
            // Recent ties broken by launch frequency (most-used first).
            let ua = app_usage.get(&a.1.name).copied().unwrap_or(0);
            let ub = app_usage.get(&b.1.name).copied().unwrap_or(0);
            ub.cmp(&ua).then_with(|| a.1.name.cmp(&b.1.name))
        });
    } else {
        app_scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    }

    let win_row = |ix: usize| -> LauncherRow {
        let WindowEntry {
            id,
            title,
            app_id,
            app_id_lc,
        } = &windows[ix];
        let resolved_icon = app_id.as_deref().and_then(|raw| {
            let name = app_icons
                .get(app_id_lc.as_deref().unwrap_or(raw))
                .map(|s| s.as_str())
                .unwrap_or(raw);
            crate::icons::resolve(name)
        });
        let kind = RowKind::Window { id: *id };
        LauncherRow {
            subtitle: build_subtitle(&kind),
            kind,
            label: SharedString::from(title.clone()),
            resolved_icon,
        }
    };
    let app_row = |app: &DesktopApp| -> LauncherRow {
        let name = SharedString::from(app.name.as_str());
        let kind = RowKind::App {
            name: name.clone(),
            exec: app.exec.clone(),
        };
        LauncherRow {
            subtitle: build_subtitle(&kind),
            kind,
            label: name,
            resolved_icon: app.icon.as_deref().and_then(crate::icons::resolve),
        }
    };

    let app_rows: Vec<LauncherRow> = app_scored.into_iter().map(|(_, a)| app_row(a)).collect();
    let win_rows: Vec<LauncherRow> = win_scored.into_iter().map(|(_, ix)| win_row(ix)).collect();
    (app_rows, win_rows)
}

pub fn filter_rows_cached(params: FilterParams) -> Vec<LauncherRow> {
    let FilterParams {
        query,
        apps,
        windows,
        files,
        recents,
        app_usage,
        app_icons,
        category,
        file_max,
        total_max,
        cached_app_rows,
        cached_win_rows,
        prefix,
        calc,
    } = params;
    let q = query.trim();

    // Inline command modes replace the result list. `r:` falls through to the
    // normal path (it only flips file search to regex mode, handled below).
    if let Some(prefix) = prefix {
        match prefix {
            ">" => {
                let cmd = q.strip_prefix('>').unwrap().trim();
                return command_mode_rows(cmd);
            }
            "o:" => {
                return open_path_rows(q.strip_prefix("o:").unwrap(), file_max);
            }
            _ => {}
        }
    }

    if category == Category::Commands {
        return command_mode_rows(q.strip_prefix('>').unwrap_or(q).trim());
    }

    // Score folds case internally; keep the raw trimmed query.
    let empty = q.is_empty();
    let apps_only = category == Category::Apps;
    let files_only = category == Category::Files;
    let windows_only = category == Category::Windows;
    let ranked_cap = if apps_only || files_only || windows_only {
        None
    } else {
        Some(total_max)
    };

    // App/window scoring (`matchq` over every app/window + a sort) is the
    // expensive part. Reuse the caller-supplied cached rows when available;
    // otherwise score now. The Daemon caches these by `query` + `source_gen`,
    // so the re-render that fires when file results arrive reuses them instead
    // of re-scoring the whole app/window list.
    let (app_rows, win_rows): (Vec<LauncherRow>, Vec<LauncherRow>) =
        match (cached_app_rows, cached_win_rows) {
            (Some(a), Some(w)) if !empty => (
                a.iter()
                    .take(ranked_cap.unwrap_or(usize::MAX))
                    .cloned()
                    .collect(),
                w.iter()
                    .take(ranked_cap.unwrap_or(usize::MAX))
                    .cloned()
                    .collect(),
            ),
            _ => score_app_window(q, apps, windows, recents, app_usage, app_icons, category),
        };

    let file_row = |hit: &FileHit| -> LauncherRow {
        let kind = RowKind::File {
            path: hit.path.clone(),
        };
        LauncherRow {
            subtitle: build_subtitle(&kind),
            kind,
            label: {
                let fallback = hit.path.display().to_string();
                let lossy = hit.path.file_name().map(|n| n.to_string_lossy());
                let name = lossy.as_deref().unwrap_or(&fallback);
                SharedString::from(name)
            },
            resolved_icon: None,
        }
    };

    let mut out: Vec<LauncherRow> = Vec::new();
    if files_only {
        // Empty query in the Files category is a frecency browse; show the
        // ranked files without requiring a typed filter.
        push_capped(&mut out, Some(file_max), files.iter().map(file_row));
        return out;
    }
    if apps_only {
        push_capped(&mut out, ranked_cap, app_rows);
        return out;
    }
    if windows_only {
        push_capped(&mut out, ranked_cap, win_rows);
        return out;
    }

    if crate::files::is_path_shaped(q) {
        // Explicit path navigation: files first, then apps, then windows.
        push_capped(
            &mut out,
            ranked_cap,
            files.iter().take(file_max).map(file_row),
        );
        push_capped(&mut out, ranked_cap, app_rows);
        push_capped(&mut out, ranked_cap, win_rows);
    } else {
        // Apps are the primary action: rank above files and windows.
        push_capped(&mut out, ranked_cap, app_rows);
        if !empty {
            push_capped(
                &mut out,
                ranked_cap,
                files.iter().take(file_max).map(file_row),
            );
        }
        push_capped(&mut out, ranked_cap, win_rows);
    }
    // Fallback: nothing matched a non-path query -> offer to run it as a
    // shell command, mirroring the `>` command-mode trigger. A valid
    // calculator expression never reaches here (it surfaces as an inline ghost,
    // not a list row), so it must not spawn a "run in terminal" fallback.
    if out.is_empty() && !empty && calc.is_none() && !crate::files::is_path_shaped(q) {
        out.extend(command_mode_rows(q));
    }
    out
}

fn command_mode_rows(cmd: &str) -> Vec<LauncherRow> {
    if cmd.is_empty() {
        return Vec::new();
    }
    let kind = RowKind::Command {
        command: cmd.to_string(),
    };
    vec![LauncherRow {
        subtitle: build_subtitle(&kind),
        kind,
        label: SharedString::from(format!("Run “{}” in terminal", cmd)),
        resolved_icon: None,
    }]
}
