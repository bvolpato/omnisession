//! Search query grammar shared by metadata matching and full-text search.
//!
//! The session picker, `omni search`, and [`crate::Store`] full-text search all parse with
//! [`SearchQuery::parse`], so a query means the same thing for titles and conversation text.

use std::ops::Range;

/// Longest admitted query in characters; longer input is cut before parsing.
///
/// This also bounds a quoted phrase, which trajectory chunk overlap must cover.
pub const SEARCH_QUERY_MAX_CHARS: usize = 4_096;
/// Most terms kept from one query; later terms are ignored.
pub const SEARCH_QUERY_MAX_TERMS: usize = 64;

/// A parsed search query. Every term must match.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchQuery {
    terms: Vec<SearchTerm>,
}

/// One search term.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchTerm {
    /// Text between double quotes, trimmed of surrounding whitespace. It matches as one contiguous,
    /// case-insensitive substring, spaces and punctuation included.
    Exact(String),
    /// Unquoted whitespace-separated word, trimmed of leading and trailing punctuation.
    Word(String),
}

impl SearchQuery {
    /// Parses `query`.
    ///
    /// A double quote starts a phrase that runs to the next double quote or, when unterminated, to
    /// the end of the query. Terms without a letter or digit are ignored, so `""` adds nothing.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let mut terms = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for character in query.chars().take(SEARCH_QUERY_MAX_CHARS) {
            if character == '"' {
                push_term(&mut terms, &mut current, quoted);
                quoted = !quoted;
            } else if !quoted && character.is_whitespace() {
                push_term(&mut terms, &mut current, false);
            } else {
                current.push(character);
            }
        }
        push_term(&mut terms, &mut current, quoted);
        Self { terms }
    }

    #[must_use]
    pub fn terms(&self) -> &[SearchTerm] {
        &self.terms
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

impl SearchTerm {
    /// Term text without quotes, as typed.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Exact(text) | Self::Word(text) => text,
        }
    }

    /// Whether metadata must contain the text contiguously instead of fuzzily: true for quoted
    /// phrases and for words with inner punctuation, such as `qwen3.8` or `feat/rate-limiter`.
    #[must_use]
    pub fn requires_substring(&self) -> bool {
        match self {
            Self::Exact(_) => true,
            Self::Word(text) => !text.chars().all(char::is_alphanumeric),
        }
    }

    /// Letter-and-digit runs, the units the full-text index tokenizes.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.text()
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
    }
}

fn push_term(terms: &mut Vec<SearchTerm>, current: &mut String, quoted: bool) {
    let text = std::mem::take(current);
    if terms.len() == SEARCH_QUERY_MAX_TERMS || !text.chars().any(char::is_alphanumeric) {
        return;
    }
    terms.push(if quoted {
        SearchTerm::Exact(text.trim().to_owned())
    } else {
        SearchTerm::Word(
            text.trim_matches(|character: char| !character.is_alphanumeric())
                .to_owned(),
        )
    });
}

/// Folds one character for case-insensitive matching. Each character folds to exactly one
/// character, so a match keeps its character count.
#[must_use]
pub fn fold_case(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

/// Folds every character of `text` with [`fold_case`].
#[must_use]
pub fn fold_text(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.chars().map(fold_case).collect()
    }
}

/// Non-ASCII characters that [`fold_case`] maps into ASCII: `İ` to `i` and the Kelvin sign to `k`.
const NON_ASCII_FOLDING_INTO_ASCII: [char; 2] = ['\u{130}', '\u{212A}'];

/// Whether `text` contains `folded_needle`, which must already be folded with [`fold_text`].
#[must_use]
pub fn contains_folded(text: &str, folded_needle: &str) -> bool {
    find_folded(text, folded_needle).is_some()
}

/// Byte range in `text` of the first occurrence of `folded_needle`, which must already be folded
/// with [`fold_text`].
///
/// An ASCII needle searches raw bytes and stops at the first match, so large texts are never
/// folded whole. Only text holding a non-ASCII character that folds into ASCII takes the slower
/// folding path, which keeps results identical to folding everything.
#[must_use]
pub fn find_folded(text: &str, folded_needle: &str) -> Option<Range<usize>> {
    if folded_needle.is_ascii() {
        if let Some(start) = find_ascii_ignore_case(text.as_bytes(), folded_needle.as_bytes()) {
            return Some(start..start + folded_needle.len());
        }
        if text.is_ascii()
            || !NON_ASCII_FOLDING_INTO_ASCII
                .iter()
                .any(|character| text.contains(*character))
        {
            return None;
        }
    }
    find_folded_characters(text, folded_needle)
}

fn find_ascii_ignore_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return Some(0);
    };
    let mut offset = 0;
    while let Some(position) = memchr::memchr2(
        first.to_ascii_lowercase(),
        first.to_ascii_uppercase(),
        &haystack[offset..],
    ) {
        let start = offset + position;
        if haystack
            .get(start + 1..start + needle.len())?
            .eq_ignore_ascii_case(rest)
        {
            return Some(start);
        }
        offset = start + 1;
    }
    None
}

fn find_folded_characters(text: &str, folded_needle: &str) -> Option<Range<usize>> {
    let folded = fold_text(text);
    let folded_start = folded.find(folded_needle)?;
    if text.is_ascii() {
        return Some(folded_start..folded_start + folded_needle.len());
    }
    // Folding keeps character counts, so character positions map back to original bytes.
    let skipped = folded[..folded_start].chars().count();
    let length = folded_needle.chars().count();
    let mut boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .skip(skipped);
    let start = boundaries.next()?;
    let end = match length {
        0 => start,
        length => boundaries.nth(length - 1)?,
    };
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::{
        NON_ASCII_FOLDING_INTO_ASCII, SearchQuery, SearchTerm, find_folded, fold_case, fold_text,
    };

    fn parse(query: &str) -> Vec<SearchTerm> {
        SearchQuery::parse(query).terms().to_vec()
    }

    #[test]
    fn quotes_make_exact_phrases_and_whitespace_splits_words() {
        use SearchTerm::{Exact, Word};

        assert_eq!(
            parse(r#"rate "Qwen3.8 coder" (limiter),"#),
            [
                Word("rate".to_owned()),
                Exact("Qwen3.8 coder".to_owned()),
                Word("limiter".to_owned())
            ]
        );
        assert_eq!(
            parse(r#"fix "rate limiter "#),
            [Word("fix".to_owned()), Exact("rate limiter".to_owned())]
        );
        assert_eq!(
            parse(r#"api"key value"#),
            [Word("api".to_owned()), Exact("key value".to_owned())]
        );
        assert!(SearchQuery::parse(r#""" "  " "->" -- ::"#).is_empty());
    }

    #[test]
    fn words_with_inner_punctuation_require_substrings() {
        let terms = parse(r#"limiter qwen3.8 feat/rate-limiter api_key "retry""#);

        assert_eq!(
            terms
                .iter()
                .map(SearchTerm::requires_substring)
                .collect::<Vec<_>>(),
            [false, true, true, true, true]
        );
        assert_eq!(
            terms[2].tokens().collect::<Vec<_>>(),
            ["feat", "rate", "limiter"]
        );
    }

    #[test]
    fn folded_find_maps_back_to_original_bytes() {
        let text = "İstanbul QWEN3.8-Coder";

        let range = find_folded(text, &fold_text("qwen3.8")).expect("ASCII match");
        assert_eq!(&text[range], "QWEN3.8");
        let range = find_folded(text, "istanbul").expect("folded capital");
        assert_eq!(&text[range], "İstanbul");
        assert!(find_folded(text, "qwen3 8").is_none());
    }

    #[test]
    fn ascii_fast_path_falls_back_for_every_character_folding_into_ascii() {
        let folding_into_ascii = (0x80..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .filter(|character| fold_case(*character).is_ascii())
            .collect::<Vec<_>>();

        assert_eq!(folding_into_ascii, NON_ASCII_FOLDING_INTO_ASCII);
        assert_eq!(find_folded("\u{212A}elvin scale", "kelvin"), Some(0..8));
    }
}
