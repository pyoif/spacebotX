//! Heuristic parsing for the graph-structured memory layer.
//!
//! The store's supersede annotations are plain prose inside a memory entry's
//! `content` (e.g. "Supersedes the earlier dispatch-only note", "supersedes the
//! prior record `abc123`"). There is no structured write path that names the
//! predecessor, so edge capture has to read the text. This module owns the
//! text-level heuristics; `MemoryStore` owns the SQLite writes.
//!
//! Kept deliberately small and dependency-free so it is trivially testable
//! without a database.

/// Words that introduce a supersede relationship in the store's prose. Matched
/// case-insensitively as a whole word so "supersedes", "superseded" both count
/// but e.g. "supersedesomething" does not.
const SUPERSEDE_CUES: &[&str] = &["supersedes", "superseded", "superseding"];

/// Words that carry no distinguishing signal when matching a supersede clause
/// against candidate entries. Kept short and generic — this is a heuristic, not
/// a stopword list for retrieval.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "by", "with", "at", "from",
    "earlier", "prior", "previous", "old", "older", "record", "this", "that", "it", "its", "was",
    "is", "are", "were", "has", "have", "had", "which", "whose",
];

/// Return the text after the first supersede cue, or `None` when the entry
/// carries no supersede annotation at all.
///
/// The returned clause is trimmed of leading punctuation/whitespace so callers
/// can tokenise it directly. Only the first cue is considered — a single entry
/// asserting several supersessions is rare and out of scope for a heuristic.
pub fn supersede_clause(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let bytes = lower.as_bytes();

    for cue in SUPERSEDE_CUES {
        let mut search_from = 0usize;
        while let Some(found) = lower[search_from..].find(cue) {
            let start = search_from + found;
            let end = start + cue.len();

            // Whole-word check: the characters around the cue must not be
            // alphanumeric (so "supersedes" matches but "xsupersedes" does not).
            let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);

            if before_ok && after_ok {
                let clause = content[end..].trim_start_matches(|c: char| {
                    c.is_whitespace() || matches!(c, ':' | ',' | '-' | '—' | '(' | ')')
                });
                // Ensure we sliced on a char boundary — `end` follows an ASCII
                // cue, so it is one.
                return Some(clause.trim().to_string());
            }
            search_from = end;
        }
    }
    None
}

/// Extract id-shaped tokens from a clause: backtick-quoted spans and bare
/// hex-ish identifiers (>= 8 chars). These unambiguously name an entry.
pub fn id_tokens(clause: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    // Backtick-quoted spans: `abc123ef`.
    let mut chars = clause.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '`' {
            let mut token = String::new();
            for (_, inner) in chars.by_ref() {
                if inner == '`' {
                    break;
                }
                token.push(inner);
            }
            let token = token.trim().to_string();
            if is_id_shaped(&token) {
                tokens.push(token);
            }
        }
    }

    // Bare id-shaped words (no backticks).
    for word in clause.split(|c: char| !is_id_char(c)) {
        if is_id_shaped(word) && !tokens.iter().any(|t| t == word) {
            tokens.push(word.to_string());
        }
    }

    tokens
}

/// Significant (non-stopword, >= 3 char, alphanumeric) terms from a clause,
/// lowercased. Used for fuzzy predecessor matching when no explicit id is given.
pub fn significant_terms(clause: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for word in clause
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
    {
        if word.len() < 3 || STOPWORDS.contains(&word.as_str()) {
            continue;
        }
        if !terms.iter().any(|t| t == &word) {
            terms.push(word);
        }
    }
    terms
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// An id-shaped token: at least 8 chars, all id chars, and containing at least
/// one ASCII digit. This keeps "20260818" and "e9ff9136" while rejecting plain
/// English words and hyphenated prose such as "dispatch-only".
fn is_id_shaped(token: &str) -> bool {
    token.len() >= 8 && token.chars().all(is_id_char) && token.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_detected_and_clause_extracted() {
        let content = "UTF-8 panic FIXED and shipped as `e9ff9136`. \
                       Supersedes the earlier dispatch-only note.";
        let clause = supersede_clause(content).expect("clause");
        assert!(clause.starts_with("the earlier dispatch-only note"));
    }

    #[test]
    fn no_cue_returns_none() {
        assert!(supersede_clause("Just a normal fact about the provider.").is_none());
    }

    #[test]
    fn whole_word_cue_only() {
        // "supersedesomething" must not match.
        assert!(supersede_clause("xsupersedesomething happened").is_none());
    }

    #[test]
    fn id_tokens_picks_backticked_and_bare_ids() {
        let clause = "the earlier dispatch-only note e9ff9136 and `abcd1234`";
        let tokens = id_tokens(clause);
        assert!(tokens.contains(&"e9ff9136".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"abcd1234".to_string()), "{tokens:?}");
        // Ordinary words are not ids.
        assert!(!tokens.iter().any(|t| t == "dispatch-only"));
    }

    #[test]
    fn significant_terms_drops_stopwords_and_short_words() {
        let terms = significant_terms("the earlier dispatch-only note");
        assert!(terms.contains(&"dispatch".to_string()), "{terms:?}");
        assert!(terms.contains(&"note".to_string()), "{terms:?}");
        assert!(!terms.iter().any(|t| t == "the"));
        assert!(!terms.iter().any(|t| t == "earlier"));
    }
}
