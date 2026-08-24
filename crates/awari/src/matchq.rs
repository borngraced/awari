//! Cheap fuzzy subsequence scorer for small row sets (windows, apps).
//! FFF owns file matching; this only has to rank a few hundred labels well.

/// Score `needle` against `haystack`, case-insensitive. `None` = no match.
///
/// Subsequence is the floor; contiguity and word-boundary bonuses make
/// prefix matches beat scattered ones so `firfox` still hits Firefox but
/// ranks below `fire`.
/// Primary lowercase fold of one char (first char of the full Unicode
/// fold). Allocation-free; multi-char expansions collapse to their head,
/// which is fine for ranking app/window labels.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub fn score(haystack: &str, needle: &str) -> Option<i64> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Some(0);
    }
    // No lowercase Strings, no Vec<char>: fold per char as we walk. Called
    // for every app/window on every keystroke, so it must not allocate.
    let mut ned = needle.chars().map(fold);
    let mut want = ned.next();

    let mut total = 0i64;
    let mut prev_hit: Option<usize> = None;
    let mut prev_hay: Option<char> = None;

    for (i, c) in haystack.chars().enumerate() {
        let Some(w) = want else { break };
        if fold(c) != w {
            prev_hay = Some(c);
            continue;
        }
        want = ned.next();
        total += 1;
        // Boundary: start of string or after a non-alphanumeric char.
        let boundary = !prev_hay.is_some_and(|p| p.is_alphanumeric());
        total += if boundary { 10 } else { 0 };
        // Contiguity: adjacent to the previous matched char.
        match prev_hit {
            Some(p) if p + 1 == i => total += 8,
            Some(_) => total -= 1,
            None => {}
        }
        prev_hit = Some(i);
        prev_hay = Some(c);
    }

    if want.is_some() {
        return None;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typo_subsequence_matches() {
        assert!(score("Firefox", "firfox").is_some());
    }

    #[test]
    fn missing_chars_fail() {
        assert_eq!(score("Firefox", "xyzzy"), None);
    }

    #[test]
    fn empty_needle_is_zero_and_prefix_beats_middle() {
        assert_eq!(score("Firefox", ""), Some(0));
        // "fire" prefix: boundary at 0 + contiguous run.
        let prefix = score("Firefox", "fire").unwrap();
        let scattered = score("xxfirxexx", "fire").unwrap();
        assert!(prefix > scattered);
    }

    #[test]
    fn case_insensitive() {
        assert!(score("FIREFOX", "fire").is_some());
        assert!(score("firefox", "FIRE").is_some());
    }
}
