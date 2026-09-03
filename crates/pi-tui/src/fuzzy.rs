//! Fuzzy matching utilities (lower score = better match).
//!
//! Ports `.references/pi-2.0/packages/tui/src/fuzzy.ts`.

/// Result of a single fuzzy match attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    /// Whether every query character appeared in order.
    pub matches: bool,
    /// Score (lower is better).
    pub score: f64,
}

/// Match `query` against `text`. Lower score is better.
#[must_use]
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    let swapped = swap_alpha_numeric(&query_lower);
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

fn match_query(normalized_query: &str, text_lower: &str) -> FuzzyMatch {
    if normalized_query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if normalized_query.len() > text_lower.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let query_chars: Vec<char> = normalized_query.chars().collect();
    let text_chars: Vec<char> = text_lower.chars().collect();
    let mut query_index = 0usize;
    let mut score = 0.0_f64;
    let mut last_match_index: Option<usize> = None;
    let mut consecutive_matches = 0u32;

    for (i, ch) in text_chars.iter().copied().enumerate() {
        if query_index >= query_chars.len() {
            break;
        }
        if ch != query_chars[query_index] {
            continue;
        }
        let is_word_boundary =
            i == 0 || matches!(text_chars[i - 1], ' ' | '\t' | '-' | '_' | '.' | '/' | ':');

        let consecutive = matches!(last_match_index, Some(last) if last + 1 == i);
        if consecutive {
            consecutive_matches = consecutive_matches.saturating_add(1);
            score -= f64::from(consecutive_matches) * 5.0;
        } else {
            consecutive_matches = 0;
            if let Some(last) = last_match_index {
                let gap = i.saturating_sub(last).saturating_sub(1);
                let gap_u = u32::try_from(gap.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX)))
                    .unwrap_or(u32::MAX);
                score += f64::from(gap_u) * 2.0;
            }
        }

        if is_word_boundary {
            score -= 10.0;
        }
        let i_u = u32::try_from(i.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX)))
            .unwrap_or(u32::MAX);
        score += f64::from(i_u) * 0.1;

        last_match_index = Some(i);
        query_index = query_index.saturating_add(1);
    }

    if query_index < query_chars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

fn swap_alpha_numeric(query: &str) -> String {
    let bytes = query.as_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    let mut i = 0usize;
    if bytes[0].is_ascii_lowercase() {
        while i < bytes.len() && bytes[i].is_ascii_lowercase() {
            i += 1;
        }
        let letters = &query[..i];
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if i > 0 && j > i && j == bytes.len() {
            return format!("{}{letters}", &query[i..j]);
        }
        return String::new();
    }
    if bytes[0].is_ascii_digit() {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &query[..i];
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_lowercase() {
            j += 1;
        }
        if i > 0 && j > i && j == bytes.len() {
            return format!("{}{digits}", &query[i..j]);
        }
    }
    String::new()
}

/// Filter and sort items by fuzzy match quality (best first).
///
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<T, F>(items: &[T], query: &str, get_text: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> &str,
{
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return items.to_vec();
    }

    let tokens: Vec<&str> = trimmed
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, f64)> = Vec::new();
    for item in items {
        let text = get_text(item);
        let mut total_score = 0.0;
        let mut all_match = true;
        for token in &tokens {
            let m = fuzzy_match(token, text);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item.clone(), total_score));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_scores_best() {
        let exact = fuzzy_match("help", "help");
        let partial = fuzzy_match("help", "helper");
        assert!(exact.matches && partial.matches);
        assert!(exact.score < partial.score);
    }

    #[test]
    fn filter_tokens() {
        let items = vec!["src/main.rs", "lib/foo.ts", "readme.md"];
        let filtered = fuzzy_filter(&items, "src main", |s| s);
        assert_eq!(filtered, vec!["src/main.rs"]);
    }

    #[test]
    fn empty_query_returns_all() {
        let items = vec!["a", "b"];
        assert_eq!(fuzzy_filter(&items, "  ", |s| s), items);
    }

    #[test]
    fn alpha_numeric_swap() {
        let m = fuzzy_match("abc12", "12abc");
        assert!(m.matches);
    }
}
