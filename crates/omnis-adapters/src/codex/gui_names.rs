use std::collections::HashMap;

use rusqlite::Row;

pub(super) const WHITESPACE: &str = " \t\n\r\u{b}\u{c}\u{85}\u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}";

#[derive(Debug, Default)]
pub(super) enum StateName {
    #[default]
    Index,
    Gui(Option<String>),
    Guardian(String),
}

impl StateName {
    pub(super) fn projection(columns: &HashMap<String, i64>) -> String {
        if columns.get("id") != Some(&1) || !columns.contains_key("history_mode") {
            return "NULL, 0, 0".into();
        }
        let legacy = if columns.contains_key("title") && columns.contains_key("first_user_message")
        {
            "CASE WHEN trim(title, ?2) IS NOT trim(first_user_message, ?2) THEN title END"
        } else {
            "NULL"
        };
        let paginated = if columns.contains_key("name") {
            "name"
        } else {
            "NULL"
        };
        let authoritative = if columns.contains_key("name") {
            "history_mode IS 'paginated'"
        } else {
            "0"
        };
        let guardian = if columns.contains_key("source") {
            "CASE WHEN history_mode = 'legacy' AND json_valid(source) THEN json_extract(source, '$.subagent.other') IS 'guardian' ELSE 0 END"
        } else {
            "0"
        };
        format!(
            "trim(CASE history_mode WHEN 'paginated' THEN {paginated} WHEN 'legacy' THEN {legacy} END, ?2), {authoritative}, {guardian}"
        )
    }

    pub(super) fn from_row(row: &Row<'_>) -> Self {
        let name = row
            .get::<_, Option<String>>(3)
            .ok()
            .flatten()
            .filter(|text| !text.is_empty());
        if row.get::<_, bool>(4).unwrap_or(false) {
            return Self::Gui(name);
        }
        match name {
            Some(text) if text == "Guardian review" && row.get::<_, bool>(5).unwrap_or(false) => {
                Self::Guardian(text)
            }
            Some(text) => Self::Gui(Some(text)),
            None => Self::Index,
        }
    }

    pub(super) fn title(&self, indexed: impl FnOnce() -> Option<String>) -> Option<String> {
        match self {
            Self::Index => indexed(),
            Self::Gui(name) => name.clone(),
            Self::Guardian(name) => indexed().or_else(|| Some(name.clone())),
        }
    }
}
