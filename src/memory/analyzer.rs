//! Text analysis for the SQLite FTS5 memory index.
//!
//! This is the Rust half of the `search_text` columns added by
//! `migrations/20260816000001_memory_search_sqlite.sql`. Nothing in the live
//! search path calls it yet — step A of the LanceDB → SQLite migration is
//! additive and a runtime no-op.
//!
//! The pipeline mirrors the one today's LanceDB full-text index uses, so that
//! swapping the index backend does not silently change which documents match.
//! Verified against `lance-index 2.0.0`'s `InvertedIndexParams::new("simple",
//! Language::English)` (the value behind `Index::FTS(Default::default())`, which
//! is what `lance.rs` passes) and the tantivy 0.24.2 filters it composes:
//!
//! `SimpleTokenizer → RemoveLong(40) → LowerCaser → Stemmer(English)`
//! `→ StopWordFilter(English) → AsciiFoldingFilter`
//!
//! Two deliberate differences from that pipeline:
//!
//! * **Stemming moves to write time.** FTS5 has no stemmer, so [`analyze_tokens`]
//!   stems here and [`build_match_query`] stems queries. `rust-stemmers` runs the
//!   same Snowball English algorithm tantivy's `Stemmer` delegates to, so the
//!   two agree — but it also means the two sides must stay in lockstep: a change
//!   to one must be mirrored in the other, and existing rows must be re-analysed.
//! * **Tokens are also indexed whole.** The FTS5 table tokenizes with
//!   `unicode61 tokenchars '_-.'`, which keeps identifier-shaped tokens
//!   (`context_window`, `glm-5.3-flash`) intact, whereas `SimpleTokenizer`
//!   splits on *every* non-alphanumeric character. Applying `tokenchars` alone
//!   would therefore be a recall *regression* (a query for `context` would stop
//!   finding a row containing `context_window`). [`analyzed_text`] avoids that by
//!   emitting the stemmed tokens *and* the raw text: the raw copy yields the
//!   whole-identifier tokens, the stemmed copy yields the split-and-stemmed ones.
//!
//! Known fidelity gap: [`ascii_fold`] covers Latin-1 (and a few Latin Extended-A
//! letters), whereas tantivy's `AsciiFoldingFilter` carries a much larger table.
//! Content outside Latin-1 will index unfolded where tantivy would have folded it.

use rust_stemmers::{Algorithm, Stemmer};

/// Token length limit in bytes, mirroring tantivy's `RemoveLongFilter::limit(40)`.
///
/// `lance-index` sets this to `Some(40)` by default. The filter's predicate is
/// `token.text.len() < limit`, so a token of exactly 40 bytes is dropped.
pub const MAX_TOKEN_BYTES: usize = 40;

/// English stop words, matching tantivy's `StopWordFilter::new(Language::English)`
/// byte for byte. That list is in turn the one Apache Lucene's `EnglishAnalyzer`
/// uses; it is ASCII, which is why the fold below happens after this check.
pub const ENGLISH_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Split text the way tantivy's `SimpleTokenizer` does: runs of alphanumeric
/// characters, with every other character acting as a separator.
///
/// Note this *does* split identifiers — `context_window` yields two tokens. That
/// is the current behaviour and is preserved here; the whole-identifier tokens
/// come from the raw copy that [`analyzed_text`] appends.
pub fn simple_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Apply the analyser's token filters and return the surviving tokens.
///
/// Order matters and follows the tantivy pipeline exactly: the length filter
/// runs on the original token, then lowercasing, then stemming, then the stop
/// word check (against the *stemmed*, unfolded token, as `StopWordFilter` runs
/// before `AsciiFoldingFilter`), then folding.
pub fn analyze_tokens(text: &str) -> Vec<String> {
    let stemmer = Stemmer::create(Algorithm::English);
    analyzed_tokens_with(&stemmer, text)
}

/// The analysed text stored in `memories.search_text` / `chronicle_fts.search_text`.
///
/// Emits `<stemmed tokens> <raw text>`. Both halves are needed — see the module
/// documentation. Whitespace-only or empty input has no analysed half, so the
/// raw text is returned unchanged.
pub fn analyzed_text(content: &str) -> String {
    let stemmed = analyze_tokens(content).join(" ");

    if stemmed.is_empty() {
        content.to_string()
    } else {
        format!("{stemmed} {content}")
    }
}

/// Build an FTS5 `MATCH` expression for a user query, or `None` if the query
/// carries no usable terms (callers must treat that as "no results", not as an
/// error — an empty `MATCH` string is a syntax error).
///
/// Every term is double-quoted and the terms are joined with `OR`, matching the
/// `Operator::Or` that `lance_index::scalar::MatchQuery` uses today. Quoting is
/// mandatory rather than cosmetic: FTS5's query parser reads `-` and `.` as
/// operators (a bare `glm-5.3-flash` fails with `fts5: syntax error near "."`)
/// and `word:term` as a column filter — an error while the name is not a column
/// (`no such column: word`) and a silent restriction when it is. Quoting every
/// term removes the whole class. Verified against SQLite 3.53.1.
///
/// Each token contributes both its raw and its stemmed form, so a query matches
/// whichever half of the indexed text happens to carry it.
pub fn build_match_query(query: &str) -> Option<String> {
    let stemmer = Stemmer::create(Algorithm::English);
    let mut terms: Vec<String> = Vec::new();

    for token in simple_tokens(query) {
        if token.len() >= MAX_TOKEN_BYTES {
            continue;
        }

        let lowered = token.to_lowercase();
        let stemmed = stemmer.stem(&lowered).into_owned();

        if ENGLISH_STOP_WORDS.contains(&stemmed.as_str()) {
            continue;
        }

        push_unique(&mut terms, &ascii_fold(&lowered));

        let folded = ascii_fold(&stemmed);
        if folded != ascii_fold(&lowered) {
            push_unique(&mut terms, &folded);
        }
    }

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Quote a single term so FTS5 parses it as a literal string rather than as
/// query syntax. Embedded double quotes are escaped by doubling them.
///
/// The `*` prefix operator, if ever needed, goes *outside* the quotes
/// (`"glm-5.3"*`) — never inside, where it would be treated as a literal.
pub fn escape_term(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len() + 2);
    escaped.push('"');

    for ch in term.chars() {
        if ch == '"' {
            escaped.push('"');
        }
        escaped.push(ch);
    }

    escaped.push('"');
    escaped
}

fn analyzed_tokens_with(stemmer: &Stemmer, text: &str) -> Vec<String> {
    let mut analyzed = Vec::new();

    for token in simple_tokens(text) {
        if token.len() >= MAX_TOKEN_BYTES {
            continue;
        }

        let lowered = token.to_lowercase();
        let stemmed = stemmer.stem(&lowered).into_owned();

        if ENGLISH_STOP_WORDS.contains(&stemmed.as_str()) {
            continue;
        }

        let folded = ascii_fold(&stemmed);
        if !folded.is_empty() {
            analyzed.push(folded);
        }
    }

    analyzed
}

fn push_unique(terms: &mut Vec<String>, term: &str) {
    if term.is_empty() {
        return;
    }

    let quoted = escape_term(term);
    if !terms.contains(&quoted) {
        terms.push(quoted);
    }
}

/// Fold the accented Latin letters that appear in practice to their ASCII
/// equivalents. An approximation of tantivy's `AsciiFoldingFilter` — see the
/// module documentation for the gap.
fn ascii_fold(token: &str) -> String {
    let mut folded = String::with_capacity(token.len());

    for ch in token.chars() {
        match fold_char(ch) {
            Some(mapped) => folded.push_str(mapped),
            None => folded.push(ch),
        }
    }

    folded
}

fn fold_char(ch: char) -> Option<&'static str> {
    Some(match ch {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' => "c",
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' => "e",
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' => "i",
        'Ð' | 'ð' => "d",
        'Ñ' | 'ñ' => "n",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => "o",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' => "u",
        'Ý' | 'ý' | 'ÿ' => "y",
        'Þ' | 'þ' => "th",
        'ß' => "ss",
        'Ā' | 'ā' => "a",
        'Ē' | 'ē' => "e",
        'Ī' | 'ī' => "i",
        'Ō' | 'ō' => "o",
        'Ū' | 'ū' => "u",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_tokens_splits_on_non_alphanumeric() {
        assert_eq!(simple_tokens("context_window"), vec!["context", "window"]);
        assert_eq!(
            simple_tokens("glm-5.3-flash"),
            vec!["glm", "5", "3", "flash"]
        );
        assert_eq!(simple_tokens("cache_hit v2"), vec!["cache", "hit", "v2"]);
    }

    #[test]
    fn simple_tokens_handles_empty_and_whitespace() {
        assert!(simple_tokens("").is_empty());
        assert!(simple_tokens("   \n\t ").is_empty());
        assert!(simple_tokens("!!! ---").is_empty());
    }

    #[test]
    fn analyze_tokens_stems_like_the_lance_pipeline() {
        // Expected stems taken from rust-stemmers' own Snowball English fixture
        // (test_data/voc_en.txt + res_en.txt) — not guessed.
        assert_eq!(analyze_tokens("memories"), vec!["memori"]);
        assert_eq!(analyze_tokens("memory"), vec!["memori"]);
        assert_eq!(analyze_tokens("stores handled"), vec!["store", "handl"]);
        assert_eq!(analyze_tokens("configuration"), vec!["configur"]);
        // ...and words the stemmer leaves alone.
        assert_eq!(analyze_tokens("flash window"), vec!["flash", "window"]);
    }

    #[test]
    fn analyze_tokens_drops_english_stop_words() {
        assert!(analyze_tokens("the").is_empty());
        assert!(analyze_tokens("this will there with").is_empty());
        assert_eq!(analyze_tokens("the memory of"), vec!["memori"]);
    }

    #[test]
    fn analyze_tokens_drops_overlong_tokens() {
        // `RemoveLongFilter` keeps tokens strictly shorter than the limit.
        let at_limit = "a".repeat(MAX_TOKEN_BYTES);
        assert!(analyze_tokens(&at_limit).is_empty());

        let under_limit = "a".repeat(MAX_TOKEN_BYTES - 1);
        assert_eq!(analyze_tokens(&under_limit).len(), 1);
    }

    #[test]
    fn analyzed_text_emits_stemmed_and_raw_forms() {
        let analyzed = analyzed_text("memories");
        assert!(analyzed.contains("memori"), "missing stem: {analyzed}");
        assert!(
            analyzed.contains("memories"),
            "missing raw copy: {analyzed}"
        );
    }

    #[test]
    fn analyzed_text_keeps_identifiers_whole_for_the_raw_copy() {
        // The raw copy is what the FTS5 `tokenchars` tokenizer turns into a
        // single `context_window` token; the stemmed half splits it.
        let analyzed = analyzed_text("context_window is set");
        assert!(analyzed.contains("context_window"), "got: {analyzed}");
        assert!(analyzed.starts_with("context window"), "got: {analyzed}");
    }

    #[test]
    fn analyzed_text_handles_empty_input() {
        assert_eq!(analyzed_text(""), "");
        assert_eq!(analyzed_text("   "), "   ");
    }

    #[test]
    fn escape_term_quotes_terms_fts5_would_misparse() {
        assert_eq!(escape_term("glm-5.3-flash"), "\"glm-5.3-flash\"");
        assert_eq!(escape_term("x:y"), "\"x:y\"");
        assert_eq!(escape_term("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn build_match_query_ors_quoted_terms() {
        assert_eq!(
            build_match_query("memories").as_deref(),
            Some("\"memories\" OR \"memori\"")
        );
    }

    #[test]
    fn build_match_query_returns_none_without_usable_terms() {
        assert_eq!(build_match_query(""), None);
        assert_eq!(build_match_query("   "), None);
        assert_eq!(build_match_query("the this will there with"), None);
    }

    #[test]
    fn build_match_query_never_leaves_a_bare_special_character() {
        // Each of these would be FTS5 syntax if it reached the query unquoted:
        // `-` and `.` are operators, `:` is a column filter.
        for query in ["glm-5.3-flash", "a:b:c", "who:what.where-when"] {
            let built = build_match_query(query).expect("query should have terms");

            assert!(!built.contains('.'), "bare `.` in: {built}");
            assert!(!built.contains(':'), "bare `:` in: {built}");

            for term in built.split(" OR ") {
                assert!(
                    term.starts_with('"') && term.ends_with('"') && term.len() > 1,
                    "unquoted term in: {built}"
                );
            }
        }
    }
}
