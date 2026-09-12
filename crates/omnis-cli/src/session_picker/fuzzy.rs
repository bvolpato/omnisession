use super::NativeSession;

const SUBSTRING_SCORE: i64 = 1_000;
const SUBSEQUENCE_SCORE: i64 = 500;
const WORD_START_BONUS: i64 = 150;
const GAP_PENALTY: i64 = 60;
const POSITION_PENALTY_LIMIT: usize = 100;
const TITLE_BONUS: i64 = 400;
const NAME_BONUS: i64 = 200;
const FIELD_CHARACTER_LIMIT: usize = 512;

/// Lowercased searchable metadata. Fields are scored independently so a match never spans a
/// title and a path.
pub(super) struct SearchFields {
    fields: Vec<(Vec<char>, i64)>,
}

impl SearchFields {
    pub(super) fn new(session: &NativeSession, derived_title: Option<&str>) -> Self {
        let project = session.project_path.as_deref();
        let candidates = [
            (session.title.clone(), TITLE_BONUS),
            (derived_title.map(str::to_owned), TITLE_BONUS),
            (
                project
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
                NAME_BONUS,
            ),
            (session.git_branch.clone(), NAME_BONUS),
            (project.map(|path| path.display().to_string()), 0),
            (Some(session.session.provider.to_string()), 0),
            (Some(session.session.id.clone()), 0),
        ];
        Self {
            fields: candidates
                .into_iter()
                .filter_map(|(value, bonus)| value.map(|value| (lowercase_chars(&value), bonus)))
                .filter(|(value, _)| !value.is_empty())
                .collect(),
        }
    }

    /// Scores each query term against its best field. Every term must match.
    pub(super) fn score(&self, terms: &[Vec<char>]) -> Option<i64> {
        terms.iter().try_fold(0_i64, |total, term| {
            self.fields
                .iter()
                .filter_map(|(field, bonus)| term_score(field, term).map(|score| score + bonus))
                .max()
                .map(|best| total.saturating_add(best))
        })
    }
}

pub(super) fn query_terms(query: &str) -> Vec<Vec<char>> {
    query
        .split_whitespace()
        .map(lowercase_chars)
        .filter(|term| !term.is_empty())
        .collect()
}

/// Marks characters of `text` matched by query terms.
pub(super) fn highlight_marks(text: &str, terms: &[Vec<char>]) -> Vec<bool> {
    let field = lowercase_chars(text);
    let mut marks = vec![false; field.len()];
    for term in terms {
        if let Some(start) = find_substring(&field, term) {
            marks[start..start + term.len()].fill(true);
        } else if let Some((start, end)) = tightest_subsequence(&field, term) {
            let mut matched = 0;
            for (index, character) in field.iter().enumerate().take(end + 1).skip(start) {
                if matched < term.len() && *character == term[matched] {
                    marks[index] = true;
                    matched += 1;
                }
            }
        }
    }
    marks
}

fn lowercase_chars(value: &str) -> Vec<char> {
    value
        .chars()
        .take(FIELD_CHARACTER_LIMIT)
        .map(|character| character.to_lowercase().next().unwrap_or(character))
        .collect()
}

fn term_score(field: &[char], term: &[char]) -> Option<i64> {
    if term.is_empty() || term.len() > field.len() {
        return None;
    }
    if let Some(start) = find_substring(field, term) {
        return Some(SUBSTRING_SCORE + word_start_bonus(field, start) - position_penalty(start));
    }
    let (start, end) = tightest_subsequence(field, term)?;
    let gaps = i64::try_from(end + 1 - start - term.len()).unwrap_or(i64::MAX);
    Some(
        SUBSEQUENCE_SCORE - gaps.saturating_mul(GAP_PENALTY) + word_start_bonus(field, start)
            - position_penalty(start),
    )
}

fn find_substring(field: &[char], term: &[char]) -> Option<usize> {
    if term.is_empty() || term.len() > field.len() {
        return None;
    }
    field.windows(term.len()).position(|window| window == term)
}

// The window bound keeps scattered letters across unrelated words from matching.
fn tightest_subsequence(field: &[char], term: &[char]) -> Option<(usize, usize)> {
    let first = *term.first()?;
    let limit = term.len() + term.len().div_ceil(2).max(2);
    let mut best: Option<(usize, usize)> = None;
    for start in (0..field.len()).filter(|index| field[*index] == first) {
        let window_end = start.saturating_add(limit).min(field.len());
        let mut matched = 1;
        let mut end = (term.len() == 1).then_some(start);
        if end.is_none() {
            for (index, character) in field.iter().enumerate().take(window_end).skip(start + 1) {
                if *character == term[matched] {
                    matched += 1;
                    if matched == term.len() {
                        end = Some(index);
                        break;
                    }
                }
            }
        }
        if let Some(end) = end {
            if best.is_none_or(|(best_start, best_end)| end - start < best_end - best_start) {
                best = Some((start, end));
            }
            if end + 1 - start == term.len() {
                break;
            }
        }
    }
    best
}

fn word_start_bonus(field: &[char], start: usize) -> i64 {
    if start == 0 || !field[start - 1].is_alphanumeric() {
        WORD_START_BONUS
    } else {
        0
    }
}

fn position_penalty(start: usize) -> i64 {
    i64::try_from(start.min(POSITION_PENALTY_LIMIT)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use omnis_ir::{Provider, SessionRef};

    use super::*;

    fn fields(title: &str, project: &str) -> SearchFields {
        SearchFields::new(
            &NativeSession {
                session: SessionRef::new(Provider::Codex, "019f-synthetic"),
                title: Some(title.to_owned()),
                project_path: Some(PathBuf::from(project)),
                git_branch: Some("main".to_owned()),
                created_at: Some(Utc::now()),
                updated_at: Some(Utc::now()),
                updated_at_approximate: false,
                event_count: 0,
                source_path: None,
            },
            None,
        )
    }

    #[test]
    fn words_match_in_any_order_and_across_small_gaps() {
        let session = fields("Fix the rate limiter retry", "/workspace/api");

        assert!(session.score(&query_terms("ratelimiter")).is_some());
        assert!(session.score(&query_terms("limiter rate")).is_some());
        assert!(session.score(&query_terms("RATE api")).is_some());
        assert!(session.score(&query_terms("rate billing")).is_none());
    }

    #[test]
    fn scattered_letters_across_words_do_not_match() {
        let session = fields("Unrelated visible title", "/workspace");

        assert!(session.score(&query_terms("needle")).is_none());
    }

    #[test]
    fn title_substrings_outrank_path_subsequences() {
        let title = fields("Pagination cleanup", "/workspace/other");
        let path = fields("Unrelated", "/workspace/pagi-nation");

        assert!(title.score(&query_terms("pagination")) > path.score(&query_terms("pagination")));
    }

    #[test]
    fn highlight_marks_cover_substring_and_fuzzy_characters() {
        let text = "Rate Limiter";
        let marks = highlight_marks(text, &query_terms("ratelim"));
        let highlighted = text
            .chars()
            .zip(&marks)
            .filter(|(_, mark)| **mark)
            .map(|(character, _)| character)
            .collect::<String>();

        assert_eq!(highlighted, "RateLim");
    }
}
