use std::{fs, path::PathBuf, process::Command};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const AVAILABLE: &str = "019f0000-0000-7000-8000-000000000011";
const OFFLINE: &str = "019f0000-0000-7000-8000-000000000012";
const NAMED: &str = "019f0000-0000-7000-8000-000000000013";
const CLAUDE: &str = "11111111-1111-4111-8111-111111111114";
const RETIRED: [&str; 4] = [
    "Amberledger",
    "garnetbudget",
    "Linseedledger",
    "linseedbudget",
];

struct Fixture(TempDir);

impl Fixture {
    fn json(&self, args: &[&str]) -> Value {
        let root = self.0.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .current_dir(root)
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("CODEX_HOME", root.join("codex"))
            .env("CODEX_SQLITE_HOME", root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("OMNISESSION_HOME", root.join("omni"))
            .env("OMNI_NO_UPDATE_CHECK", "1");
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        let output = command.arg("--json").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn rollout(&self, id: &str) -> PathBuf {
        self.0.path().join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{id}.jsonl"
        ))
    }

    fn messages(question: &str, answer: &str) -> [Value; 2] {
        [
            json!({"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":question}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":answer}]}}),
        ]
    }

    fn write_rollout<const N: usize>(&self, id: &str, rows: [Value; N]) {
        let path = self.rollout(id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, rows.map(|row| format!("{row}\n")).concat()).unwrap();
    }

    fn write_codex(&self, id: &str, question: &str, answer: &str) {
        let header = json!({"timestamp": "2026-01-02T00:00:00Z", "type": "session_meta",
            "payload": {"id": id, "cwd": self.0.path()}});
        let [question, answer] = Self::messages(question, answer);
        self.write_rollout(id, [header, question, answer]);
    }

    fn write_paginated_subagent(&self, id: &str, question: &str, answer: &str) {
        let header = json!({"timestamp": "2026-01-02T00:00:00Z", "type": "session_meta",
            "ordinal": 0, "payload": {"id": id, "session_id": id, "cwd": self.0.path(),
                "history_mode": "paginated", "subagent_history_start_ordinal": 2}});
        let usage = json!({"type":"event_msg","ordinal":1,"payload":{"type":"token_count"}});
        let [mut question, mut answer] = Self::messages(question, answer);
        question["ordinal"] = json!(2);
        answer["ordinal"] = json!(3);
        self.write_rollout(id, [header, usage, question, answer]);
    }

    fn search(&self, provider: &str, term: &str) -> Vec<Value> {
        let result = self.json(&[
            "search",
            "--cached",
            "--show-text",
            "--all-projects",
            "--provider",
            provider,
            term,
        ]);
        assert_eq!(result["index"]["indexed"], 0);
        result["results"].as_array().unwrap().clone()
    }

    fn hit(&self, provider: &str, term: &str, session: &str) -> Value {
        let mut results = self.search(provider, term);
        assert_eq!(results.len(), 1, "{provider} {term}");
        let hit = results.pop().unwrap();
        assert_eq!(hit["session"], session);
        hit
    }

    fn index(&self, provider: &str) -> Value {
        self.json(&["index", "--provider", provider])
    }

    fn assert_snippet(&self, provider: &str, term: &str, session: &str) {
        let hit = self.hit(provider, term, session);
        assert_eq!(hit["match"], "conversation");
        assert!(hit["snippet"].as_str().unwrap().contains(term));
    }

    fn add_claude_and_import(&self) -> String {
        let root = self.0.path();
        let dir = root.join("claude/projects/notes");
        fs::create_dir_all(&dir).unwrap();
        let rows = [
            json!({"type":"user","uuid":"44444444-4444-4444-8444-444444444441","sessionId":CLAUDE,
                "timestamp":"2026-01-02T00:00:00Z","cwd":root,"message":{"role":"user","content":"Write the planning notes"}}),
            json!({"type":"assistant","uuid":"55555555-5555-4555-8555-555555555551","sessionId":CLAUDE,
                "timestamp":"2026-01-02T00:00:01Z","cwd":root,"message":{"role":"assistant","content":[
                    {"type":"text","text":"The ledgercontrol schedule is ready"}]}}),
        ];
        fs::write(
            dir.join(format!("{CLAUDE}.jsonl")),
            rows.map(|row| format!("{row}\n")).concat(),
        )
        .unwrap();
        assert_eq!(self.index("claude")["indexed"], 1);
        self.json(&[
            "export",
            &format!("claude:{CLAUDE}"),
            "--output",
            "planning.json",
        ]);
        let imported = self.json(&["import", "planning.json"]);
        format!("imported:{}", imported["bundle_id"].as_str().unwrap())
    }

    fn downgrade(&self) {
        let db = Connection::open(self.0.path().join("omni/store.sqlite3")).unwrap();
        for (session, version) in [(AVAILABLE, 45), (OFFLINE, 44), (NAMED, 43)] {
            let changed = db
                .execute(
                    "UPDATE session_trajectories SET document_version = ?2 WHERE provider = 'codex'
                 AND session_id = ?1 AND origin = 'native' AND source_complete AND complete
                 AND source_file_id IS NOT NULL AND source_contents_id IS NOT NULL
                 AND derived_title IS NOT NULL",
                    params![session, version],
                )
                .unwrap();
            assert_eq!(changed, 1, "seeded full native projection for {session}");
        }
        for (provider, origin) in [("claude", "native"), ("imported", "imported_bundle")] {
            assert_eq!(db.execute(
                "UPDATE session_trajectories SET document_version = 6 WHERE provider = ?1 AND origin = ?2",
                params![provider, origin],
            ).unwrap(), 1);
        }
    }
}

#[test]
fn cached_search_hides_old_codex_projections_until_the_source_is_read_again() {
    let fixture = Fixture(tempfile::tempdir().unwrap());
    fixture.write_codex(
        AVAILABLE,
        "Amberledger planning",
        "Review the garnetbudget estimates",
    );
    fixture.write_codex(
        OFFLINE,
        "Linseedledger planning",
        "Review the linseedbudget estimates",
    );
    fixture.write_paginated_subagent(
        NAMED,
        "Write the schedule",
        "Review the calendarrates worksheet",
    );
    fs::write(
        fixture.0.path().join("codex/session_index.jsonl"),
        format!(
            "{}\n",
            json!({"id":NAMED,"thread_name":"Beacon forecast","updated_at":"2026-01-03T00:00:00Z"})
        ),
    )
    .unwrap();
    assert_eq!(fixture.index("codex")["indexed"], 3);
    let imported = fixture.add_claude_and_import();
    let available = format!("codex:{AVAILABLE}");
    let offline = format!("codex:{OFFLINE}");
    let named = format!("codex:{NAMED}");
    assert_eq!(
        fixture.hit("codex", "Amberledger", &available)["match"],
        "metadata"
    );
    assert_eq!(
        fixture.hit("codex", "Linseedledger", &offline)["match"],
        "metadata"
    );
    fixture.assert_snippet("codex", "garnetbudget", &available);
    fixture.assert_snippet("codex", "calendarrates", &named);
    fixture.downgrade();
    fixture.write_codex(
        AVAILABLE,
        "Cobaltledger planning",
        "Review the cobaltbudget estimates",
    );
    fs::remove_file(fixture.rollout(OFFLINE)).unwrap();

    for term in RETIRED.into_iter().chain(["calendarrates", "cobaltbudget"]) {
        assert!(fixture.search("codex", term).is_empty(), "{term}");
    }
    let official = fixture.hit("codex", "Beacon", &named);
    assert_eq!(official["title"], "Beacon forecast");
    assert!(official["snippet"].is_null());
    for (provider, id) in [
        ("claude", format!("claude:{CLAUDE}")),
        ("imported", imported),
    ] {
        fixture.assert_snippet(provider, "ledgercontrol", &id);
    }

    assert_eq!(fixture.index("codex")["indexed"], 2);
    assert_eq!(
        fixture.hit("codex", "Cobaltledger", &available)["title"],
        "Cobaltledger planning"
    );
    fixture.assert_snippet("codex", "cobaltbudget", &available);
    assert_eq!(
        fixture.hit("codex", "calendarrates", &named)["title"],
        "Beacon forecast"
    );
    for term in RETIRED {
        assert!(fixture.search("codex", term).is_empty(), "{term}");
    }
    assert_eq!(fixture.index("codex")["indexed"], 0);
}
