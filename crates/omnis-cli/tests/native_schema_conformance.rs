#![cfg(target_os = "linux")]

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const SYNTHETIC_TEXTS: [&str; 4] = [
    "Synthetic schema acceptance turn 1 user",
    "Synthetic schema acceptance turn 1 assistant",
    "Synthetic schema acceptance turn 2 user",
    "Synthetic schema acceptance turn 2 assistant",
];
const COMPATIBILITY_MANIFEST: &str = include_str!("../provider-compatibility.json");

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_BIN with the installed Cursor Agent bundle"]
fn installed_cursor_schema_accepts_cli_and_ide_synthetic_states() {
    let cursor_binary = env::var_os("OMNI_TEST_CURSOR_BIN")
        .or_else(|| env::var_os("OMNI_CURSOR_AGENT_BIN"))
        .map(PathBuf::from)
        .expect("OMNI_TEST_CURSOR_BIN must point to the installed Cursor Agent executable");
    assert!(
        cursor_binary.is_absolute() && cursor_binary.is_file(),
        "Cursor Agent binary must be an existing absolute path"
    );

    let mut fixture = Fixture::new();
    fixture.write_source();
    let cli_session = fixture.materialize("cursor", &cursor_binary);
    let ide_session = fixture.materialize("cursor-ide", &cursor_binary);

    let mut entries = Vec::new();
    entries.extend(fixture.cursor_cli_states(&cli_session));
    entries.extend(fixture.cursor_ide_states(&ide_session));
    assert_eq!(entries.len(), 6);
    fixture.assert_source_unchanged();
    validate_with_installed_cursor_schema(&cursor_binary, &entries);
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    codex: PathBuf,
    cursor_chats: PathBuf,
    cursor_ide: PathBuf,
    source: PathBuf,
    cursor_ide_binary: PathBuf,
    source_before: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("synthetic conformance root");
        let root = temporary
            .path()
            .canonicalize()
            .expect("canonical conformance root");
        let workspace = root.join("workspace");
        let codex = root.join("codex");
        let cursor_chats = root.join("cursor/chats");
        let cursor_ide = root.join("cursor-ide/User");
        for directory in [
            root.join("home"),
            workspace.clone(),
            codex.join("sessions/2026/01/01"),
            cursor_chats.clone(),
            cursor_ide.join("globalStorage"),
            root.join("omni"),
        ] {
            fs::create_dir_all(directory).expect("create isolated fixture directory");
        }

        let source = codex
            .join("sessions/2026/01/01")
            .join(format!("rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"));
        let ide_database = Connection::open(cursor_ide.join("globalStorage/state.vscdb"))
            .expect("create synthetic Cursor IDE database");
        ide_database
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
                 CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .expect("create synthetic Cursor IDE schema");
        drop(ide_database);

        let manifest = serde_json::from_str::<Value>(COMPATIBILITY_MANIFEST)
            .expect("provider compatibility manifest");
        let minimum_ide_version = manifest
            .get("providers")
            .and_then(Value::as_array)
            .and_then(|providers| {
                providers.iter().find(|provider| {
                    provider.get("id").and_then(Value::as_str) == Some("cursor-ide")
                })
            })
            .and_then(|provider| provider.get("minimum_version"))
            .and_then(Value::as_str)
            .expect("Cursor IDE minimum version");
        let cursor_ide_binary = root.join(format!("Cursor-{minimum_ide_version}-x86_64.AppImage"));
        fs::write(&cursor_ide_binary, "#!/bin/sh\nexit 1\n")
            .expect("create non-launchable Cursor IDE identity stub");
        fs::set_permissions(&cursor_ide_binary, fs::Permissions::from_mode(0o755))
            .expect("make Cursor IDE identity stub executable");

        Self {
            _temporary: temporary,
            root,
            workspace,
            codex,
            cursor_chats,
            cursor_ide,
            source,
            cursor_ide_binary,
            source_before: Vec::new(),
        }
    }

    fn write_source(&mut self) {
        let mut records = vec![json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": SOURCE_ID,
                "cwd": self.workspace,
                "cli_version": "synthetic",
                "model_provider": "synthetic"
            }
        })];
        for text in SYNTHETIC_TEXTS {
            let role = if text.ends_with("user") {
                "user"
            } else {
                "assistant"
            };
            records.push(json!({
                "timestamp": "2026-01-01T00:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": role,
                    "content": [{
                        "type": if role == "user" { "input_text" } else { "output_text" },
                        "text": text
                    }]
                }
            }));
        }
        let document = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&self.source, document).expect("write synthetic source trajectory");
        self.source_before = fs::read(&self.source).expect("read synthetic source bytes");
    }

    fn isolated_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").expect("test PATH"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("CODEX_HOME", &self.codex)
            .env("CURSOR_AGENT_HOME", &self.cursor_chats)
            .env("CURSOR_IDE_HOME", &self.cursor_ide)
            .env("OMNISESSION_HOME", self.root.join("omni"))
            .env(
                "OMNI_CURSOR_AGENT_BIN",
                env::var_os("OMNI_TEST_CURSOR_BIN")
                    .or_else(|| env::var_os("OMNI_CURSOR_AGENT_BIN"))
                    .expect("Cursor Agent binary"),
            )
            .env("OMNI_CURSOR_IDE_BIN", &self.cursor_ide_binary)
            .env("PI_CODING_AGENT_SESSION_DIR", self.root.join("pi"))
            .env("PI_CODING_AGENT_DIR", self.root.join("pi-agent"))
            .env("OMP_SESSION_DIR", self.root.join("omp"))
            .env("ANTIGRAVITY_CLI_HOME", self.root.join("antigravity"))
            .env("HERMES_HOME", self.root.join("hermes"))
            .env("GROK_HOME", self.root.join("grok"))
            .env("OPENCODE_TEST_HOME", self.root.join("home"))
            .env("OPENCODE_DB", self.root.join("opencode/state.db"))
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_CONFIG_CONTENT", "{}")
            .env("OMP_PROFILE", "")
            .current_dir(&self.workspace);
        command
    }

    fn materialize(&self, target: &str, cursor_binary: &Path) -> String {
        let output = self
            .isolated_command()
            .args([
                "resume",
                &format!("codex:{SOURCE_ID}"),
                "--in",
                target,
                "--materialize-only",
            ])
            .output()
            .expect("run isolated synthetic Cursor materialization");
        assert!(
            output.status.success(),
            "{target} synthetic materialization failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let prefix = if target == "cursor" {
            "cursor-cli:"
        } else {
            "cursor-ide:"
        };
        let session = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| {
                line.strip_prefix("Created and verified ")
                    .and_then(|value| value.strip_suffix('.'))
                    .and_then(|value| value.strip_prefix(prefix))
            })
            .map_or_else(
                || panic!("{target} materialization omitted native session ID"),
                str::to_owned,
            );
        // Keep the binary parameter explicit: it must be the same installed
        // bundle passed to the schema validator below.
        assert!(cursor_binary.is_file());
        session
    }

    fn cursor_cli_states(&self, session: &str) -> Vec<Value> {
        let session_dir = self
            .cursor_chats
            .read_dir()
            .expect("Cursor CLI workspace directories")
            .flatten()
            .map(|workspace| workspace.path().join(session))
            .find(|path| path.join("store.db").is_file())
            .expect("materialized Cursor CLI database");
        let connection = Connection::open_with_flags(
            session_dir.join("store.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read isolated Cursor CLI database");
        let metadata_hex: String = connection
            .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
                row.get(0)
            })
            .expect("Cursor CLI root metadata");
        let metadata: Value =
            serde_json::from_slice(&hex::decode(metadata_hex).expect("metadata hex"))
                .expect("Cursor CLI metadata JSON");
        let root_id = metadata["latestRootBlobId"]
            .as_str()
            .expect("Cursor CLI latest root ID");
        let mut statement = connection
            .prepare("SELECT id, data FROM blobs")
            .expect("prepare Cursor CLI blob query");
        let blobs = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .expect("query Cursor CLI blobs")
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()
            .expect("read Cursor CLI blobs");
        collect_state_entries("cursor-cli", root_id, &blobs)
    }

    fn cursor_ide_states(&self, session: &str) -> Vec<Value> {
        let connection = Connection::open_with_flags(
            self.cursor_ide.join("globalStorage/state.vscdb"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read isolated Cursor IDE database");
        let composer_key = format!("composerData:{session}");
        let composer_bytes: Vec<u8> = connection
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                params![composer_key],
                |row| row.get(0),
            )
            .expect("materialized Cursor IDE composer root");
        let composer: Value = serde_json::from_slice(&composer_bytes).expect("composer JSON");
        let encoded_state = composer["conversationState"]
            .as_str()
            .and_then(|value| value.strip_prefix('~'))
            .expect("Cursor IDE conversation state");
        let root = BASE64
            .decode(encoded_state)
            .expect("Cursor IDE state base64");
        let mut statement = connection
            .prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'")
            .expect("prepare Cursor IDE blob query");
        let blobs = statement
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                Ok((key.trim_start_matches("agentKv:blob:").to_owned(), bytes))
            })
            .expect("query Cursor IDE native blobs")
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()
            .expect("read Cursor IDE native blobs");
        collect_state_entries_from_root("cursor-ide", &root, &blobs)
    }

    fn assert_source_unchanged(&self) {
        assert_eq!(
            fs::read(&self.source).expect("read synthetic source after imports"),
            self.source_before,
            "native import modified the synthetic source trajectory"
        );
    }
}

fn collect_state_entries(
    provider: &str,
    root_id: &str,
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Vec<Value> {
    let root_id = hex::decode(root_id).expect("Cursor CLI root ID hex");
    let root = verified_blob(blobs, &root_id);
    collect_state_entries_from_root(provider, &root, blobs)
}

fn collect_state_entries_from_root(
    provider: &str,
    root: &[u8],
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Vec<Value> {
    let turns = length_delimited_fields(root, 8);
    assert_eq!(
        turns.len(),
        2,
        "synthetic Cursor root should have two turns"
    );
    let mut entries = vec![state_entry(
        &format!("{provider}-root"),
        "root",
        root,
        &SYNTHETIC_TEXTS,
        blobs,
    )];

    for (index, turn_id) in turns.iter().enumerate() {
        let turn = verified_blob(blobs, turn_id);
        let agent = first_length_delimited(&turn, 1).expect("agent turn payload");
        let user_id = first_length_delimited(agent, 1).expect("user message reference");
        let user = verified_blob(blobs, user_id);
        let user_text = first_length_delimited(&user, 1).expect("native user text");
        assert_eq!(user_text, SYNTHETIC_TEXTS[index * 2].as_bytes());

        let step_ids = length_delimited_fields(agent, 2);
        assert_eq!(
            step_ids.len(),
            1,
            "synthetic assistant turn should have one step"
        );
        let step = verified_blob(blobs, &step_ids[0]);
        let assistant = first_length_delimited(&step, 1).expect("assistant step payload");
        let assistant_text = first_length_delimited(assistant, 1).expect("native assistant text");
        assert_eq!(assistant_text, SYNTHETIC_TEXTS[index * 2 + 1].as_bytes());

        let anchor_id = first_length_delimited(&user, 10).expect("rewind anchor reference");
        let anchor = verified_blob(blobs, anchor_id);
        assert_eq!(length_delimited_fields(&anchor, 8).len(), index);
        entries.push(state_entry(
            &format!("{provider}-rewind-{}", index + 1),
            "rewind",
            &anchor,
            &SYNTHETIC_TEXTS[..=index * 2],
            blobs,
        ));
    }
    entries
}

fn state_entry(
    label: &str,
    kind: &str,
    state: &[u8],
    expected_texts: &[&str],
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Value {
    let prompt_blobs = length_delimited_fields(state, 1)
        .iter()
        .map(|id| {
            let bytes = verified_blob(blobs, id);
            json!({"id": hex::encode(id), "bytes": BASE64.encode(bytes)})
        })
        .collect::<Vec<_>>();
    json!({
        "label": label,
        "kind": kind,
        "bytes": BASE64.encode(state),
        "expectedTurns": if kind == "root" { 2 } else { expected_texts.len().saturating_sub(1) / 2 },
        "expectedTexts": expected_texts,
        "promptBlobs": prompt_blobs,
    })
}

fn verified_blob(blobs: &BTreeMap<String, Vec<u8>>, id: &[u8]) -> Vec<u8> {
    let key = hex::encode(id);
    let bytes = blobs
        .get(&key)
        .unwrap_or_else(|| panic!("native blob {key} is missing"));
    assert_eq!(
        hex::encode(Sha256::digest(bytes)),
        key,
        "native blob hash mismatch"
    );
    bytes.clone()
}

fn length_delimited_fields(bytes: &[u8], wanted_tag: u64) -> Vec<Vec<u8>> {
    let mut fields = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor).expect("protobuf field key");
        let tag = key >> 3;
        match key & 7 {
            0 => {
                let _ = read_varint(bytes, &mut cursor).expect("protobuf varint");
            }
            1 => {
                cursor = cursor
                    .checked_add(8)
                    .filter(|end| *end <= bytes.len())
                    .expect("protobuf fixed64");
            }
            2 => {
                let size =
                    usize::try_from(read_varint(bytes, &mut cursor).expect("protobuf byte length"))
                        .expect("protobuf length fits usize");
                let end = cursor
                    .checked_add(size)
                    .filter(|end| *end <= bytes.len())
                    .expect("protobuf bytes");
                if tag == wanted_tag {
                    fields.push(bytes[cursor..end].to_vec());
                }
                cursor = end;
            }
            5 => {
                cursor = cursor
                    .checked_add(4)
                    .filter(|end| *end <= bytes.len())
                    .expect("protobuf fixed32");
            }
            wire => panic!("unsupported synthetic protobuf wire type {wire}"),
        }
    }
    fields
}

fn first_length_delimited(bytes: &[u8], tag: u64) -> Option<&[u8]> {
    let mut cursor = 0;
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)?;
        match key & 7 {
            0 => {
                read_varint(bytes, &mut cursor)?;
            }
            1 => cursor = cursor.checked_add(8).filter(|end| *end <= bytes.len())?,
            2 => {
                let size = usize::try_from(read_varint(bytes, &mut cursor)?).ok()?;
                let end = cursor.checked_add(size).filter(|end| *end <= bytes.len())?;
                if key >> 3 == tag {
                    return Some(&bytes[cursor..end]);
                }
                cursor = end;
            }
            5 => cursor = cursor.checked_add(4).filter(|end| *end <= bytes.len())?,
            _ => return None,
        }
    }
    None
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn validate_with_installed_cursor_schema(binary: &Path, entries: &[Value]) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-cursor-protobuf.cjs");
    let path = env::var_os("PATH").expect("test PATH");
    let mut child = Command::new("node")
        .env_clear()
        .env("PATH", path)
        .arg(script)
        .arg(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start schema-only Cursor validator");
    serde_json::to_writer(child.stdin.take().expect("validator stdin"), entries)
        .expect("write synthetic native state blobs");
    let output = child
        .wait_with_output()
        .expect("wait for Cursor schema validator");
    assert!(
        output.status.success(),
        "installed Cursor schema rejected synthetic native state: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("validator summary JSON");
    assert_eq!(result["decoded"], 6);
    assert_eq!(result["providers"], 2);
    assert_eq!(result["roots"], 2);
    assert_eq!(result["rewindAnchors"], 4);
}
