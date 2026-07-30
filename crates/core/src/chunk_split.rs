//! Splits an oversized `chunk_text` into threshold-sized units for `knowledge_process_chunk`
//! (issue #284). Splitting is whitespace-preferred and lossless: `split_into_units(text,
//! max_chars).concat() == text` always holds, which is what lets the idempotency check in
//! `handlers.rs` reconstruct a prior chunk_text by concatenating its episodes' stored content.

/// Splits `text` into units of at most `max_chars` characters (measured via `chars().count()`,
/// never raw byte length — `chunk_text` is caller-supplied and may contain multi-byte chars).
///
/// Each unit boundary prefers the nearest whitespace at or before the `max_chars` mark, so a
/// split never lands mid-word when avoidable; the boundary whitespace is kept at the end of the
/// earlier unit. Falls back to a hard cut at exactly `max_chars` only when no whitespace exists
/// in that window (e.g. one long unbreakable token).
///
/// Text at or below `max_chars` returns a single unit equal to the input. `max_chars == 0` is
/// not an expected input (the advisory threshold is always a positive char count), but is
/// clamped to 1 rather than left to loop forever, degrading to one unit per char.
///
/// The backward whitespace scan has no lower bound other than `start` itself: a window that
/// contains one whitespace character very close to `start`, followed by a long whitespace-free
/// run past the `max_chars` mark, cuts near `start` and repeats — producing a run of small units
/// until the whitespace-free stretch is exhausted. Invariants (unit length, lossless
/// concatenation) still hold; this is a quality/performance caveat for pathological input, not a
/// correctness issue. If that pattern repeats across the whole input, each unit's scan still
/// costs up to `max_chars` steps, so total cost can approach `O(chars.len() * max_chars)` rather
/// than `O(chars.len())` — the caller (`handlers.rs`) runs this via `spawn_blocking` rather than
/// inline on the async executor specifically because of this worst case.
pub fn split_into_units(text: &str, max_chars: usize) -> Vec<String> {
    // A zero-width window never advances `start` below, which would hang the caller forever.
    // Clamp to 1 so `max_chars == 0` degrades to "one unit per char", matching the doc comment.
    let max_chars = max_chars.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }

    let mut units = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let window_end = (start + max_chars).min(chars.len());
        if window_end == chars.len() {
            units.push(chars[start..window_end].iter().collect());
            break;
        }

        // Scan backward from window_end for the nearest whitespace, keeping it in this unit.
        let mut cut = window_end;
        let mut found_whitespace = false;
        while cut > start {
            if chars[cut - 1].is_whitespace() {
                found_whitespace = true;
                break;
            }
            cut -= 1;
        }

        let end = if found_whitespace { cut } else { window_end };
        units.push(chars[start..end].iter().collect());
        start = end;
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_threshold_returns_single_unchanged_unit() {
        let text = "short text";
        let units = split_into_units(text, 8000);
        assert_eq!(units, vec![text.to_string()]);
    }

    #[test]
    fn at_threshold_returns_single_unit() {
        let text = "a".repeat(100);
        let units = split_into_units(&text, 100);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], text);
    }

    #[test]
    fn every_unit_is_at_or_under_max_chars() {
        let text = "word ".repeat(1000);
        let units = split_into_units(&text, 50);
        assert!(units.len() > 1);
        for u in &units {
            assert!(u.chars().count() <= 50, "unit exceeded max_chars: {u:?}");
        }
    }

    #[test]
    fn concatenation_reconstructs_original_exactly() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(500);
        let units = split_into_units(&text, 137);
        assert_eq!(units.concat(), text);
    }

    #[test]
    fn prefers_whitespace_boundary_over_mid_word_cut() {
        let text = "aaaaaaaaaa bbbbbbbbbb cccccccccc";
        // max_chars lands mid-word inside "bbbbbbbbbb"; expect the cut to back up to the
        // space after "aaaaaaaaaa" rather than slicing through the second word.
        let units = split_into_units(text, 15);
        assert_eq!(units[0], "aaaaaaaaaa ");
        assert_eq!(units.concat(), text);
    }

    #[test]
    fn unbreakable_token_falls_back_to_hard_cut() {
        let text = "a".repeat(250);
        let units = split_into_units(&text, 100);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].chars().count(), 100);
        assert_eq!(units[1].chars().count(), 100);
        assert_eq!(units[2].chars().count(), 50);
        assert_eq!(units.concat(), text);
    }

    #[test]
    fn multibyte_chars_split_safely_on_char_boundaries() {
        let text = "héllo wörld ".repeat(50);
        let units = split_into_units(&text, 30);
        assert!(units.len() > 1);
        for u in &units {
            assert!(u.chars().count() <= 30);
        }
        assert_eq!(units.concat(), text);
    }

    #[test]
    fn empty_text_returns_single_empty_unit() {
        let units = split_into_units("", 8000);
        assert_eq!(units, vec!["".to_string()]);
    }

    #[test]
    fn zero_max_chars_degrades_to_one_unit_per_char_without_hanging() {
        // A misconfigured LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS=0 must not hang the caller: a
        // zero-width window makes no progress unless max_chars is clamped to at least 1.
        let text = "abc";
        let units = split_into_units(text, 0);
        assert_eq!(
            units,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(units.concat(), text);
    }
}
