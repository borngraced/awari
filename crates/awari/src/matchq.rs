//! Cheap fuzzy subsequence scorer for small row sets (windows, apps).
//! FFF owns file matching; this only has to rank a few hundred labels well.

/// Score `needle` against `haystack`, case-insensitive. `None` = no match.
///
/// Subsequence is the floor; contiguity and word-boundary bonuses make
/// prefix matches beat scattered ones so `firfox` still hits Firefox but
/// ranks below `fire`.
pub fn score(haystack: &str, needle: &str) -> Option<i64> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Some(0);
    }    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let ned_str = needle.trim().to_lowercase();
    let mut ned = ned_str.chars().peekable();

    let mut total = 0i64;
    let mut prev_hit: Option<usize> = None;

    for (i, c) in hay.iter().enumerate() {
        let Some(&want) = ned.peek() else { break };
        if *c != want {
            continue;
        }
        ned.next();
        total += 1;
        // Boundary: start of string or after a non-alphanumeric char.
        let boundary = i == 0 || !hay[i - 1].is_alphanumeric();
        total += if boundary { 10 } else { 0 };
        // Contiguity: adjacent to the previous matched char.
        match prev_hit {
            Some(p) if p + 1 == i => total += 8,
            Some(_) => total -= 1,
            None => {}
        }
        prev_hit = Some(i);
    }

    if ned.peek().is_some() {
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
