//! Subsequence fuzzy matcher using the fzy / skim affine-gap Smith–Waterman
//! algorithm. A match must be a subsequence of the candidate. Boundary/capital
//! bonuses are awarded only for the leading char and tight (consecutive)
//! matches, never for scattered gaps — otherwise a boundary-rich haystack could
//! stack bonuses across a meaningless spray.

const FZY_MATCH: i32 = 16;
const FZY_GAP_START: i32 = -3;
const FZY_GAP_EXTEND: i32 = -1;
const FZY_BONUS_HEAD: i32 = FZY_MATCH / 2; // 8: start of word / after hard sep
const FZY_BONUS_BREAK: i32 = FZY_MATCH / 2 + FZY_GAP_EXTEND; // 7: after soft sep
const FZY_BONUS_CAMEL: i32 = FZY_MATCH / 2 + 2 * FZY_GAP_EXTEND; // 6: camelCase
const FZY_BONUS_CONSECUTIVE: i32 = -(FZY_GAP_START + FZY_GAP_EXTEND); // 4
const FZY_FIRST_CHAR_MULT: i32 = 2;
const FZY_CASE_MISMATCH: i32 = FZY_GAP_EXTEND * 2; // -2
const FZY_NEG_INF: i32 = i32::MIN / 2;

/// Character role classification for the in-place boundary bonus: the char
/// before and after a match decide how much that match is worth (start of word,
/// after a soft separator, a camelCase bump, or nothing mid-run).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FzyCharType {
    Empty,
    Upper,
    Lower,
    Number,
    HardSep,
    SoftSep,
}

impl FzyCharType {
    fn of(ch: char) -> Self {
        match ch {
            '\0' => FzyCharType::Empty,
            ' ' | '/' | '\\' | '|' | '(' | ')' | '[' | ']' | '{' | '}' => FzyCharType::HardSep,
            '!'..='\'' | '*'..='.' | ':'..='@' | '^'..='`' | '~' => FzyCharType::SoftSep,
            '0'..='9' => FzyCharType::Number,
            'A'..='Z' => FzyCharType::Upper,
            _ => FzyCharType::Lower,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FzyCharRole {
    Head,
    Tail,
    Camel,
    Break,
}

impl FzyCharRole {
    fn of_type(prev: FzyCharType, cur: FzyCharType) -> Self {
        match (prev, cur) {
            (FzyCharType::Empty | FzyCharType::HardSep, _) => FzyCharRole::Head,
            (FzyCharType::SoftSep, _) => FzyCharRole::Break,
            (FzyCharType::Lower | FzyCharType::Number, FzyCharType::Upper) => FzyCharRole::Camel,
            _ => FzyCharRole::Tail,
        }
    }
}

fn fzy_in_place_bonus(prev: FzyCharType, cur: FzyCharType) -> i32 {
    match FzyCharRole::of_type(prev, cur) {
        FzyCharRole::Head => FZY_BONUS_HEAD,
        FzyCharRole::Camel => FZY_BONUS_CAMEL,
        FzyCharRole::Break => FZY_BONUS_BREAK,
        FzyCharRole::Tail => 0,
    }
}

fn fzy_char_eq(c: char, p: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        c == p
    } else {
        c.eq_ignore_ascii_case(&p)
    }
}

/// Best subsequence score of `needle` within `haystack`, or `None` if `needle`
/// is not a subsequence of `haystack`. Case-insensitive unless `needle` contains
/// an ASCII uppercase letter (smart-case), matching skim behavior.
pub fn subsequence_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle: Vec<char> = needle.chars().collect();
    subsequence_score_chars(&needle, haystack)
}

/// Core of [`subsequence_score`] with the needle precomputed as `&[char]` so the
/// hot file-search path builds it once and reuses it across every hit.
///
/// Two score matrices are maintained per pattern row: `M` (match ends here) and
/// `P` (gap — char skipped). Gaps use an affine penalty (`GAP_START` once, then
/// `GAP_EXTEND` per extra char), which lets a tight match survive one trailing
/// gap while a scattered spray accumulates a penalty per gap.
pub(super) fn subsequence_score_chars(needle: &[char], haystack: &str) -> Option<i32> {
    let pattern = needle;
    let choice: Vec<char> = haystack.chars().collect();
    if pattern.is_empty() {
        return Some(0);
    }
    if choice.len() < pattern.len() {
        return None;
    }
    let case_sensitive = needle.iter().any(|c| c.is_ascii_uppercase());

    // first_match[i] = earliest choice column where pattern[i] can align. If any
    // pattern char has no candidate, the needle isn't a subsequence at all.
    let mut first_match = vec![0usize; pattern.len()];
    {
        let mut ci = 0usize;
        for (i, &p) in pattern.iter().enumerate() {
            let mut found = None;
            while ci < choice.len() {
                if fzy_char_eq(choice[ci], p, case_sensitive) {
                    found = Some(ci);
                    break;
                }
                ci += 1;
            }
            {
                let pos = found?;
                first_match[i] = pos;
                ci = pos + 1;
            }
        }
    }

    let rows = pattern.len() + 1;
    let cols = choice.len() + 1;
    // Cell state: M score, P score, running consecutive bonus, indexed by
    // `row * cols + col`. Columns are 1-indexed into `choice` (col 0 = empty
    // prefix); `choice[col - 1]` is the char for column `col`.
    let mut m = vec![FZY_NEG_INF; rows * cols];
    let mut p = vec![FZY_NEG_INF; rows * cols];
    let mut bonus = vec![0i32; rows * cols];

    // Per-column in-place boundary bonus; the first column is doubled, mirroring
    // skim's preference for the leading pattern char.
    let mut in_bonus = vec![0i32; cols];
    {
        let mut prev_ch = '\0';
        for (j, &c) in choice.iter().enumerate() {
            in_bonus[j + 1] = fzy_in_place_bonus(FzyCharType::of(prev_ch), FzyCharType::of(c));
            prev_ch = c;
        }
        in_bonus[1] *= FZY_FIRST_CHAR_MULT;
    }

    // Row 0: no pattern consumed. P[0][j] = GAP_EXTEND (a skipped prefix);
    // M[0][j] stays NEG_INF (a match can't end before the pattern starts).
    for i in p.iter_mut().take(cols) {
        *i = FZY_GAP_EXTEND;
    }

    // Reset the starting cell of every pattern row so the DP never reads stale
    // state from a previous reuse of the buffers.
    for (i, &start) in first_match.iter().enumerate() {
        let idx = (i + 1) * cols + (start + 1);
        m[idx] = FZY_NEG_INF;
        p[idx] = FZY_NEG_INF;
        bonus[idx] = 0;
    }

    for (i, &pch) in pattern.iter().enumerate() {
        let row = i + 1;
        let row_prev = i;
        let row_base = row * cols;
        let row_prev_base = row_prev * cols;
        let to_skip = first_match[i];
        for (j, &c_ch) in choice[to_skip..].iter().enumerate() {
            let col = to_skip + j + 1;
            let col_prev = to_skip + j;
            let idx_cur = row_base + col;
            let idx_last = row_base + col_prev;
            let idx_prev = row_prev_base + col_prev;
            let in_place = in_bonus[col];

            // M matrix: best alignment ending in a match at (row, col).
            if fzy_char_eq(c_ch, pch, case_sensitive) {
                let prev_match = m[idx_prev];
                let prev_skip = p[idx_prev];
                let prev_bonus = bonus[idx_last];
                let match_val = FZY_MATCH
                    + if !case_sensitive && pch != c_ch {
                        FZY_CASE_MISMATCH
                    } else {
                        0
                    };
                let consecutive = prev_bonus.max(in_place).max(FZY_BONUS_CONSECUTIVE);
                bonus[idx_last] = consecutive;
                let score_match = prev_match + consecutive;
                // Boundary/capital bonuses apply only on the tight (`score_match`)
                // path or for the leading char; a scattered (gapped) transition
                // gets no in-place bonus.
                let score_skip = if i == 0 {
                    prev_skip + in_place
                } else {
                    prev_skip
                };

                if score_match >= score_skip {
                    m[idx_cur] = score_match + match_val;
                } else {
                    m[idx_cur] = score_skip + match_val;
                }
            } else {
                m[idx_cur] = FZY_NEG_INF;
                bonus[idx_cur] = 0;
            }

            // P matrix: best alignment with col skipped (gap). Affine gap.
            let gap_match = FZY_GAP_START + FZY_GAP_EXTEND + m[idx_last];
            let gap_skip = FZY_GAP_EXTEND + p[idx_last];
            if gap_match >= gap_skip {
                p[idx_cur] = gap_match;
            } else {
                p[idx_cur] = gap_skip;
            }
        }
    }

    // The score is the best M in the final pattern row, from the last char's
    // first feasible column onward. A valid subsequence always yields a finite
    // value here; a degenerate all-NEG_INF means no real match.
    let last_row = pattern.len();
    let first_col = first_match[pattern.len() - 1] + 1;
    let best = m[last_row * cols + first_col..]
        .iter()
        .copied()
        .max()
        .unwrap_or(FZY_NEG_INF);
    (best > FZY_NEG_INF).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_excludes_loose_fuzzy_hits() {
        // fff's typo/subsequence fuzz returned these for "heap"/"head", but
        // neither is a subsequence, so the matcher drops them.
        assert!(subsequence_score("heap", "head_formatter").is_none());
        assert!(subsequence_score("head", "readme.md").is_none());
        assert!(subsequence_score("head", "readme").is_none());
    }

    #[test]
    fn subsequence_keeps_real_subsequence_matches() {
        assert!(subsequence_score("heap", "heaptrace.rs").is_some());
        assert!(subsequence_score("head", "head.rs").is_some());
        assert!(subsequence_score("head", "src/headless.rs").is_some());
    }

    #[test]
    fn subsequence_consecutive_requires_adjacency() {
        // "ab" in "axb" is gapped (one skipped char): the gap penalty must push
        // it below the consecutive "ab" in "abx". An exact match "ab" must beat
        // a leading-gap match "xab" (fzy doesn't penalize a trailing gap, only
        // leading/trailing-in-choice gaps via P, so exact == consecutive).
        let gapped = subsequence_score("ab", "axb").unwrap();
        let consecutive = subsequence_score("ab", "abx").unwrap();
        let exact = subsequence_score("ab", "ab").unwrap();
        let leading_gap = subsequence_score("ab", "xab").unwrap();
        assert!(gapped < consecutive, "{gapped} < {consecutive}");
        assert!(exact > leading_gap, "{exact} > {leading_gap}");
    }

    #[test]
    fn subsequence_gap_penalty_beats_scattered_boundaries() {
        // fzy tolerates single-char gaps (a one-gap scatter scores ≈ a tight
        // match), but a *multi-gap* scatter is clearly penalized by the affine
        // gap cost.
        let tight = subsequence_score("golang", "golang").unwrap();
        let single_gap = subsequence_score("golang", "g/o/l/a/n/g").unwrap();
        let multi_gap = subsequence_score("golang", "gzzzzozzzlzzzazzznzzzg").unwrap();
        assert!(tight >= single_gap, "{tight} >= {single_gap}");
        assert!(tight > multi_gap, "{tight} > {multi_gap}");
    }

    #[test]
    fn subsequence_ranks_shorter_paths_higher() {
        let bare = subsequence_score("head", "head.rs").unwrap();
        let nested = subsequence_score("head", "src/deep/nested/head.rs").unwrap();
        assert!(bare > nested);
        let late = subsequence_score("head", "zzz/verylongpath/head.rs").unwrap();
        assert!(bare > late);
    }

    #[test]
    fn subsequence_empty_query_matches_everything() {
        assert_eq!(subsequence_score("", "anything"), Some(0));
    }

    #[test]
    fn subsequence_repro_golang_boundary_spray() {
        // Repro: query "golang" must rank the real goland tarball (a tight
        // "golan" core + one trailing-grab gap) ABOVE a fully-scattered spray
        // like `G..o..l..a..n..g`, whose letters all land on capitals/dots. The
        // affine-gap scorer keeps the tight match ahead because the spray pays
        // GAP_START per gap.
        let spray = subsequence_score("golang", "G..o..l..a..n..g").unwrap();
        let goland = subsequence_score("golang", "goland-2026.2.0.1.tar.gz").unwrap();
        assert!(goland > spray, "goland({goland}) must beat spray({spray})");
    }

    #[test]
    fn subsequence_gapped_match_is_not_filtered() {
        // Regression: a match that needs one large trailing gap (grab the final
        // `g` from `.tar.gz`) must still score as a real match, never None. Our
        // previous linear-gap scorer returned None here and dropped the file.
        assert!(subsequence_score("golang", "goland-2026.2.0.1.tar.gz").is_some());
    }
}