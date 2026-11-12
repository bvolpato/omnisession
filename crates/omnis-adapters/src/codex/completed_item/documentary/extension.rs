use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::Value;

#[derive(Deserialize)]
#[serde(tag = "kind")]
pub(super) enum Extension<'a> {
    #[serde(rename = "image_gen.generation", borrow)]
    Image(Image<'a>),
    #[serde(rename = "clock.sleep")]
    Sleep {
        id: &'a str,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
    },
    #[serde(rename = "web.search", borrow)]
    Search(Search<'a>),
}

impl Extension<'_> {
    pub(super) fn normalized(
        &self,
    ) -> (
        &'static str,
        &'static [&'static str],
        Option<(&'static str, Value)>,
    ) {
        match self {
            Self::Image(image) => (
                "image_gen.generation",
                &[],
                image
                    .failure
                    .as_ref()
                    .and_then(|f| serde_json::to_value(f).ok())
                    .map(|value| ("failure", value)),
            ),
            Self::Sleep { .. } => ("clock.sleep", &["type", "kind", "id", "durationMs"], None),
            Self::Search(search) => (
                "web.search",
                &["type", "kind", "id", "query", "action", "results"],
                search
                    .action
                    .as_ref()
                    .and_then(|a| serde_json::to_value(a).ok())
                    .map(|value| ("action", value)),
            ),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Search<'a> {
    id: &'a str,
    query: &'a str,
    #[serde(borrow)]
    action: Option<Action<'a>>,
    #[serde(default)]
    results: Option<Vec<IgnoredAny>>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Action<'a> {
    Search {
        query: Option<&'a str>,
        #[serde(borrow)]
        queries: Option<Vec<&'a str>>,
    },
    OpenPage {
        url: Option<&'a str>,
    },
    FindInPage {
        url: Option<&'a str>,
        pattern: Option<&'a str>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Image<'a> {
    id: &'a str,
    status: &'a str,
    revised_prompt: Option<&'a str>,
    #[serde(flatten, borrow)]
    reference: ImageRef<'a>,
    #[serde(default)]
    transparent_background: Option<bool>,
    #[serde(default, borrow)]
    failure: Option<Failure<'a>>,
    #[serde(default)]
    saved_path: Option<super::AbsolutePath>,
    #[serde(default, borrow)]
    input_images: Vec<Input<'a>>,
    #[serde(default)]
    generation_id: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageRef<'a> {
    File {
        #[serde(rename = "fileId")]
        file_id: &'a str,
    },
    Inline {
        result: &'a str,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Failure<'a> {
    UsageLimitExceeded {
        #[serde(rename = "limitId")]
        limit_id: &'a str,
        #[serde(rename = "resetsAt")]
        resets_at: Option<i64>,
    },
}

#[derive(Deserialize)]
struct Input<'a> {
    path: super::AbsolutePath,
    sha256: &'a str,
}
