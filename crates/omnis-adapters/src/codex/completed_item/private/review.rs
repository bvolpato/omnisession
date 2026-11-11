use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct ReviewOutput<'a> {
    #[serde(borrow)]
    findings: Vec<Finding<'a>>,
    overall_correctness: &'a str,
    overall_explanation: &'a str,
    overall_confidence_score: f32,
}

#[derive(Deserialize)]
struct Finding<'a> {
    title: &'a str,
    body: &'a str,
    confidence_score: f32,
    priority: i32,
    #[serde(borrow)]
    code_location: Location<'a>,
}

#[derive(Deserialize)]
struct Location<'a> {
    absolute_file_path: &'a str,
    line_range: LineRange,
}

#[derive(Deserialize)]
struct LineRange {
    start: u32,
    end: u32,
}

pub(super) fn render(output: Option<&ReviewOutput<'_>>) -> String {
    const FALLBACK: &str = "Reviewer failed to output a response.";
    let Some(output) = output else {
        return FALLBACK.to_owned();
    };
    let explanation = output.overall_explanation.trim();
    let mut sections = Vec::new();
    if !explanation.is_empty() {
        sections.push(explanation.to_owned());
    }
    if !output.findings.is_empty() {
        let header = if output.findings.len() == 1 {
            "Review comment:"
        } else {
            "Full review comments:"
        };
        let mut lines = vec![header.to_owned()];
        for finding in &output.findings {
            let location = &finding.code_location;
            let range = &location.line_range;
            lines.push(String::new());
            lines.push(format!(
                "- {} — {}:{}-{}",
                finding.title, location.absolute_file_path, range.start, range.end
            ));
            lines.extend(finding.body.lines().map(|line| format!("  {line}")));
        }
        sections.push(lines.join("\n").trim_end().to_owned());
    }
    if sections.is_empty() {
        FALLBACK.to_owned()
    } else {
        sections.join("\n\n")
    }
}
