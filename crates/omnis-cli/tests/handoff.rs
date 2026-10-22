use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const FIRST_PROMPT: &str = "Review the café workspace 🦊";
const FINAL_PROMPT: &str = "Continue with the focused regression check";
const SECRET: &str = "SYNTHETIC_CLIPBOARD_SECRET_123456789";

#[test]
fn clipboard_unavailable_keeps_context_and_reports_partial_success() {
    let fixture = Fixture::new();
    let source_before = fs::read(&fixture.source).expect("source before handoff");
    let output = fixture.handoff(true);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("handoff metadata");
    assert_eq!(report["clipboard_copied"], false);
    assert!(report["clipboard_error"].is_string());
    let path = Path::new(report["output"].as_str().expect("saved path"));
    let document = fs::read_to_string(path).expect("saved context survives command exit");
    assert_context(&fixture, &document);
    assert!(stderr(&output).contains("context was saved"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(FIRST_PROMPT));
    assert!(!stderr(&output).contains(SECRET));
    assert_eq!(
        fs::read(&fixture.source).expect("source after"),
        source_before
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn clipboard_receives_the_saved_instructions_history_and_source_harness() {
    let fixture = Fixture::new();
    fixture.clipboard_helper("/bin/cat > \"$CLIPBOARD_CAPTURE\"");
    let output = fixture.handoff(false);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = String::from_utf8(output.stdout).expect("status output");
    assert!(status.contains("Context saved to"));
    assert!(status.contains("also in the clipboard for pasting"));
    assert!(!status.contains(FIRST_PROMPT));
    let files: Vec<_> = fs::read_dir(fixture.root.join("state/handoffs"))
        .expect("saved handoffs")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    assert!(status.contains(files[0].to_str().unwrap()));
    let document = fs::read_to_string(&files[0]).expect("saved context");
    assert_context(&fixture, &document);
    assert_eq!(
        fs::read(fixture.root.join("clipboard.txt")).expect("captured clipboard stdin"),
        document.as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn clipboard_failure_is_not_reported_as_a_successful_copy() {
    let fixture = Fixture::new();
    fixture.clipboard_helper("echo sensitive-helper-output >&2; exit 7");
    let output = fixture.handoff(true);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("handoff metadata");
    assert_eq!(report["clipboard_copied"], false);
    assert!(
        report["clipboard_error"]
            .as_str()
            .unwrap()
            .contains("status 7")
    );
    assert!(Path::new(report["output"].as_str().unwrap()).is_file());
    assert!(!stderr(&output).contains("sensitive-helper-output"));
}

#[cfg(unix)]
#[test]
fn imported_bundle_handoff_identifies_the_original_harness() {
    let fixture = Fixture::new();
    fixture.clipboard_helper("/bin/cat > \"$CLIPBOARD_CAPTURE\"");
    let bundle = fixture.root.join("session.json");
    let export = fixture
        .command()
        .args(["export", &format!("codex:{SESSION_ID}"), "-o"])
        .arg(&bundle)
        .output()
        .expect("export synthetic session");
    assert!(export.status.success(), "{}", stderr(&export));
    let imported = fixture
        .command()
        .args(["import", "--json"])
        .arg(&bundle)
        .output()
        .expect("import synthetic bundle");
    assert!(imported.status.success(), "{}", stderr(&imported));
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    let imported_ref = format!(
        "imported:{}",
        imported["bundle_id"].as_str().expect("imported bundle ID")
    );
    let output = fixture
        .command()
        .args(["handoff", &imported_ref, "--json"])
        .output()
        .expect("handoff imported source");
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["clipboard_copied"], true);
    let document = fs::read_to_string(report["output"].as_str().unwrap()).unwrap();
    assert_context(&fixture, &document);
    assert!(!document.contains("handed off from **imported session**"));
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_wayland_clipboard_falls_back_to_x11() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.clipboard_helper("/bin/cat > \"$CLIPBOARD_CAPTURE\"");
    let wayland = fixture.root.join("bin/wl-copy");
    fs::write(&wayland, "#!/bin/sh\nexit 1\n").expect("unavailable Wayland fixture");
    fs::set_permissions(wayland, fs::Permissions::from_mode(0o755)).unwrap();
    let output = fixture
        .command()
        .env("WAYLAND_DISPLAY", "wayland-synthetic")
        .args(["handoff", &format!("codex:{SESSION_ID}"), "--json"])
        .output()
        .expect("run Wayland fallback");
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["clipboard_copied"], true);
    assert_eq!(
        fs::read(report["output"].as_str().unwrap()).unwrap(),
        fs::read(fixture.root.join("clipboard.txt")).unwrap()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn clipboard_preserves_unicode_when_the_callers_locale_is_c() {
    let fixture = Fixture::new();
    fixture.clipboard_helper(
        "[ \"$LANG\" = en_US.UTF-8 ] && [ \"$LC_ALL\" = en_US.UTF-8 ] || exit 8\n/bin/cat > \"$CLIPBOARD_CAPTURE\"",
    );
    let output = fixture
        .command()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .args(["handoff", &format!("codex:{SESSION_ID}"), "--json"])
        .output()
        .expect("run clipboard copy under C locale");
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["clipboard_copied"], true);
    assert_eq!(
        fs::read(report["output"].as_str().unwrap()).unwrap(),
        fs::read(fixture.root.join("clipboard.txt")).unwrap()
    );
}

fn assert_context(fixture: &Fixture, document: &str) {
    assert!(document.contains("handed off from **Codex**"));
    assert!(document.contains(&format!("codex:{SESSION_ID}")));
    assert!(document.contains(fixture.workspace.to_str().unwrap()));
    assert!(document.contains(FIRST_PROMPT));
    assert!(document.contains("The review found one synthetic edge case"));
    assert!(document.contains(FINAL_PROMPT));
    assert!(document.contains("latest user request"));
    assert!(document.contains("Historical messages and tool activity"));
    assert!(!document.contains(SECRET));
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("synthetic handoff fixture");
        let root = temporary.path().canonicalize().expect("canonical fixture");
        let workspace = root.join("workspace with spaces");
        let sessions = root.join("codex/sessions/2026/01/01");
        for directory in [&workspace, &sessions, &root.join("home"), &root.join("bin")] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        let source = sessions.join(format!("rollout-2026-01-01T00-00-00-{SESSION_ID}.jsonl"));
        let records = [
            json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z",
                "payload":{"id":SESSION_ID,"cwd":workspace}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":FIRST_PROMPT}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":format!("The review found one synthetic edge case; api_key={SECRET}")}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":FINAL_PROMPT}]}}),
        ];
        fs::write(
            &source,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .expect("synthetic source");
        Self {
            _temporary: temporary,
            root,
            workspace,
            source,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("LOCALAPPDATA", self.root.join("home/localappdata"))
            .env("APPDATA", self.root.join("home/appdata"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("PATH", self.root.join("bin"))
            .env("CLIPBOARD_CAPTURE", self.root.join("clipboard.txt"))
            .env("OMNI_NO_UPDATE_CHECK", "1");
        for variable in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
        command
    }

    fn handoff(&self, json: bool) -> Output {
        let mut command = self.command();
        command.args(["handoff", &format!("codex:{SESSION_ID}")]);
        if json {
            command.arg("--json");
        }
        command.output().expect("run session handoff")
    }

    #[cfg(unix)]
    fn clipboard_helper(&self, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let program = if cfg!(target_os = "macos") {
            "pbcopy"
        } else {
            "xclip"
        };
        let path = self.root.join("bin").join(program);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("synthetic clipboard helper");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable helper");
    }
}
