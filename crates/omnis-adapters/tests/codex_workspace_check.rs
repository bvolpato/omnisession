use std::{fs, io::Write, path::Path};

use anyhow::Result;
use flate2::{Compression, write::GzEncoder};
use omnis_adapters::{CodexAdapter, CodexWorkspaceProjection, ProviderAdapter};
use omnis_ir::{Provider, SessionRef, SourceRevision};
use serde_json::json;

const ID: &str = "44444444-4444-4444-8444-444444444444";

fn write_rollout(path: &Path, contents: &str) -> Result<SourceRevision> {
    let bytes = match path.extension().unwrap().to_str().unwrap() {
        "gz" => {
            let mut writer = GzEncoder::new(Vec::new(), Compression::fast());
            writer.write_all(contents.as_bytes())?;
            writer.finish()?
        }
        "zst" => zstd::encode_all(contents.as_bytes(), 1)?,
        _ => contents.as_bytes().to_vec(),
    };
    fs::write(path, bytes)?;
    Ok(CodexAdapter::index_file_revision(path).unwrap())
}

#[test]
fn selected_workspace_retains_only_known_directories_across_encodings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let header = json!({"type":"session_meta","payload":{"id":ID,"cwd":"creation"}});
    let moved = json!({"type":"turn_context","payload":{"cwd":"next"}});
    let recovered = json!({"type":"turn_context","payload":{"cwd":"final"}});
    let harmless = r#"{"type":"response_item","payload":{"text":"turn_context"#;
    for extension in ["jsonl", "jsonl.gz", "jsonl.zst"] {
        let path = temp.path().join(format!("rollout-{ID}.{extension}"));
        let mut cases = vec![(harmless.to_owned(), Some("next"))];
        for torn in [
            r#"{"type":"turn_context","payload":"#,
            r#"{"type":"turn_co"#,
            r#"{"type":"t"#,
            r#"{"t"#,
            "{",
        ] {
            cases.push((torn.to_owned(), None));
            cases.push((format!("{torn}\n{recovered}"), Some("final")));
        }
        for (suffix, effective) in cases {
            let revision = write_rollout(&path, &format!("{header}\n{moved}\n{suffix}\n"))?;
            let (actual, creation) = CodexAdapter::rollout_workspaces(&session, &path, revision)?;
            assert_eq!(
                actual.as_deref(),
                effective.map(Path::new),
                "{extension} {suffix}"
            );
            assert_eq!(creation.as_deref(), Some(Path::new("creation")));
        }
    }
    Ok(())
}

#[test]
fn selected_workspace_checks_the_source_and_uses_surviving_turns() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let session = SessionRef::new(Provider::Codex, ID);
    let path = temp.path().join(format!("rollout-{ID}.jsonl"));
    let header = json!({"type":"session_meta","payload":{"id":ID,"cwd":"creation"}});
    let moved = json!({"type":"turn_context","payload":{"cwd":"discarded"}});
    let user = json!({"type":"event_msg","payload":{"type":"user_message","message":"gone"}});
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    let revision = write_rollout(&path, &format!("{header}\n{moved}\n{user}\n{rollback}\n"))?;
    assert_eq!(
        CodexAdapter::rollout_workspaces(&session, &path, revision)?
            .0
            .as_deref(),
        Some(Path::new("creation"))
    );
    assert!(CodexAdapter::rollout_workspaces(&session, &path, SourceRevision::default()).is_err());
    let wrong_header = json!({"type":"session_meta","payload":{"id":"55555555-5555-4555-8555-555555555555","cwd":"creation"}});
    let current = write_rollout(&path, &wrong_header.to_string())?;
    assert!(CodexAdapter::rollout_workspaces(&session, &path, revision).is_err());
    assert!(CodexAdapter::rollout_workspaces(&session, &path, current).is_err());
    Ok(())
}

#[test]
fn a_missing_prefix_requires_a_surviving_local_directory_for_native_routing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("sessions"))?;
    let session = SessionRef::new(Provider::Codex, ID);
    let path = temp.path().join(format!("sessions/rollout-{ID}.jsonl"));
    let adapter = CodexAdapter::with_root(temp.path());
    let inherited = json!({"type":"session_meta","payload":{
        "id":ID,"cwd":"creation","history_mode":"paginated",
        "history_base":{"thread_id":"55555555-5555-4555-8555-555555555555",
        "end_ordinal_exclusive":2, "end_byte_offset":512}
    }});
    let moved = json!({"type":"turn_context","payload":{"cwd":"next"}});
    let later = json!({"type":"turn_context","payload":{"cwd":"later"}});
    let started = json!({"type":"event_msg","payload":{"type":"task_started"}});
    let user = json!({"type":"event_msg","payload":{"type":"user_message","message":"request"}});
    let rollback =
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}});
    for (tail, effective) in [
        (String::new(), None),
        (moved.to_string(), Some(Path::new("next"))),
        (format!("{moved}\n{user}\n{rollback}"), None),
        (
            format!("{moved}\n{user}\n{rollback}\n{moved}"),
            Some(Path::new("next")),
        ),
        (
            format!("{started}\n{moved}\n{user}\n{started}\n{later}\n{user}\n{rollback}"),
            Some(Path::new("next")),
        ),
    ] {
        let inherited_revision = write_rollout(&path, &format!("{inherited}\n{tail}\n"))?;
        let (effective_dir, startup) =
            CodexAdapter::rollout_workspaces(&session, &path, inherited_revision)?;
        assert_eq!(effective_dir.as_deref(), effective, "{tail}");
        assert_eq!(startup.as_deref(), Some(Path::new("creation")));
        let read = adapter.index_session(&session, Some(&path), u64::MAX)?;
        assert_eq!(
            read.workspace_projection,
            CodexWorkspaceProjection::Observed(effective_dir)
        );
        if effective.is_none() {
            assert_eq!(read.snapshot.workspace.current_dir, Path::new("creation"));
        }
    }
    Ok(())
}
