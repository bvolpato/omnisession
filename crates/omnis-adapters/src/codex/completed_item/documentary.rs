//! Native web, image and extension display items from Codex 0.167.1.
#![expect(
    dead_code,
    reason = "Serde validates fields that the historical projection omits"
)]

use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::Value;

use super::super::rollout_lines::name_fields;
use super::command::valid_path_uri;

mod extension;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Item<'a> {
    WebSearch(#[serde(borrow)] Search<'a>),
    ImageView { id: &'a str, path: &'a str },
    ImageGeneration(#[serde(borrow)] Image<'a>),
    Extension(#[serde(borrow)] extension::Extension<'a>),
}

#[derive(Deserialize)]
struct Search<'a> {
    id: &'a str,
    query: &'a str,
    #[serde(borrow)]
    action: Action<'a>,
    #[serde(default)]
    results: Option<Vec<IgnoredAny>>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action<'a> {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<&'a str>,
        #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<&'a str>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<&'a str>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<&'a str>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<&'a str>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Image<'a> {
    id: &'a str,
    status: &'a str,
    #[serde(default)]
    revised_prompt: Option<&'a str>,
    result: &'a str,
    #[serde(default)]
    saved_path: Option<AbsolutePath>,
}

#[derive(Deserialize)]
#[serde(try_from = "&str")]
struct AbsolutePath;

impl TryFrom<&str> for AbsolutePath {
    type Error = &'static str;

    fn try_from(path: &str) -> Result<Self, Self::Error> {
        std::path::Path::new(path)
            .is_absolute()
            .then_some(Self)
            .ok_or("expected an absolute path in persisted Codex history")
    }
}

pub(super) fn prepare(item: &mut Value) -> bool {
    let Ok(decoded) = Item::deserialize(&*item) else {
        return false;
    };
    let (fields, action): (&[&str], _) = match decoded {
        Item::WebSearch(search) => (
            &["type", "id", "query", "action", "results"],
            serde_json::to_value(search.action).ok(),
        ),
        Item::ImageView { path, .. } if valid_path_uri(path) => (&["type", "id", "path"], None),
        Item::ImageView { .. } => return false,
        Item::ImageGeneration(_) => (
            &[
                "type",
                "id",
                "status",
                "revised_prompt",
                "result",
                "saved_path",
            ],
            None,
        ),
        Item::Extension(extension) => {
            let (kind, fields, detail) = extension.normalized();
            if !name_fields(item, fields) {
                return false;
            }
            item["kind"] = kind.into();
            if let Some((key, value)) = detail {
                item[key] = value;
            }
            return true;
        }
    };
    if !name_fields(item, fields) {
        return false;
    }
    if let Some(action) = action {
        item["action"] = action;
    }
    true
}
