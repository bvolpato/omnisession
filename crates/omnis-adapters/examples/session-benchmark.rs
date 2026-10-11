//! Small JSON probe used by `scripts/benchmark-sessions.py`.

use std::{env, io, io::Write, path::Path, time::Instant};

use anyhow::{Context, Result, bail};
use omnis_adapters::NativeSession;
use omnis_adapters::{ClaudeAdapter, CodexAdapter, GrokAdapter, PiAdapter, ProviderAdapter};
use omnis_ir::{CanonicalSnapshot, Provider, SessionRef};
use serde::{Serialize, ser::SerializeSeq};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn digest(value: &impl Serialize) -> Result<String> {
    let mut writer = HashWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(hex::encode(writer.0.finalize()))
}

#[derive(Serialize)]
struct SessionDigest<'a> {
    session: &'a SessionRef,
    title: &'a Option<String>,
    project_path: &'a Option<std::path::PathBuf>,
    git_branch: &'a Option<String>,
    created_at: &'a Option<chrono::DateTime<chrono::Utc>>,
    updated_at: &'a Option<chrono::DateTime<chrono::Utc>>,
    updated_at_approximate: bool,
    event_count: usize,
    source_path: &'a Option<std::path::PathBuf>,
}

struct SessionList<'a>(&'a [NativeSession]);

impl Serialize for SessionList<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for session in self.0 {
            seq.serialize_element(&SessionDigest {
                session: &session.session,
                title: &session.title,
                project_path: &session.project_path,
                git_branch: &session.git_branch,
                created_at: &session.created_at,
                updated_at: &session.updated_at,
                updated_at_approximate: session.updated_at_approximate,
                event_count: session.event_count,
                source_path: &session.source_path,
            })?;
        }
        seq.end()
    }
}

fn read_snapshot(
    provider: &str,
    root: &Path,
    source: &Path,
    id: &str,
) -> Result<(CanonicalSnapshot, Provider, u128)> {
    let provider = match provider {
        "claude" => Provider::Claude,
        "grok" => Provider::Grok,
        "pi" => Provider::Pi,
        "omp" => Provider::OhMyPi,
        _ => bail!("unsupported read provider `{provider}`"),
    };
    let session = SessionRef::new(provider, id);
    let started = Instant::now();
    let snapshot = match provider {
        Provider::Claude => {
            ClaudeAdapter::with_root(root).read_session_at(&session, Some(source))?
        }
        Provider::Grok => GrokAdapter::with_root(root).read_session(&session)?,
        Provider::Pi => PiAdapter::with_root(root).read_session_at(&session, Some(source))?,
        Provider::OhMyPi => {
            PiAdapter::oh_my_pi_with_root(root).read_session_at(&session, Some(source))?
        }
        _ => unreachable!("provider was selected above"),
    };
    Ok((snapshot, provider, started.elapsed().as_nanos()))
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("read") => {
            let provider = args.next().context("missing provider")?;
            let root = args.next().context("missing provider root")?;
            let source = args.next().context("missing transcript path")?;
            let id = args.next().context("missing session id")?;
            let (snapshot, provider, elapsed_ns) =
                read_snapshot(&provider, Path::new(&root), Path::new(&source), &id)?;
            println!(
                "{{\"provider\":\"{}\",\"count\":{},\"elapsed_ns\":{},\"sha256\":\"{}\"}}",
                provider,
                snapshot.events.len(),
                elapsed_ns,
                digest(&snapshot)?
            );
        }
        Some("codex-list") => {
            let root = args.next().context("missing Codex root")?;
            let adapter = CodexAdapter::with_root(root);
            let started = Instant::now();
            let sessions = adapter.list_sessions(None)?;
            let elapsed_ns = started.elapsed().as_nanos();
            println!(
                "{{\"provider\":\"codex\",\"count\":{},\"elapsed_ns\":{},\"sha256\":\"{}\"}}",
                sessions.len(),
                elapsed_ns,
                digest(&SessionList(&sessions))?
            );
        }
        _ => bail!("usage: session-benchmark read PROVIDER ROOT SOURCE ID | codex-list ROOT"),
    }
    Ok(())
}
