use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Read, Seek},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider, ReplayPolicy, SessionRef, SourceRevision};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    IndexRead, LaunchPlan, LaunchTarget, NativeSelectionInventory, NativeSession, ProviderAdapter,
    ProviderInstallation, read_budgeted_transcript,
    source_file::ObservedSourceFile,
    support::{
        EventBuilder, JsonLineLocation, MAX_COLLECTED_TRANSCRIPT_FILE_SIZE, ProviderFiles,
        json_lines_preview_file_with_suffix, nested_files, nested_files_matching,
        omitted_images_text, parse_timestamp, paths_match, provider_executable, provider_file,
        provider_root, read_json_line_at, sort_sessions, string_at, validate_provider,
        visit_json_lines_with_offsets,
    },
};

const PI_SESSION_VERSION: u64 = 3;
const PREVIEW_RECORDS: usize = 1_024;
const MAX_HEADER_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_HEADER_LINE_BYTES: u64 = 64 * 1024;

/// A complete read found an invalid parent chain in the selected native transcript.
#[derive(Debug)]
pub struct InvalidPiTree {
    /// Metadata and file revision read from the handle that contained the invalid parent chain.
    pub source: NativeSession,
    problem: &'static str,
}

impl std::fmt::Display for InvalidPiTree {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.problem)
    }
}

impl std::error::Error for InvalidPiTree {}

/// Read-only Pi coding-agent session adapter.
#[derive(Clone, Debug)]
pub struct PiAdapter {
    provider: Provider,
    sessions_root: Option<PathBuf>,
}

impl PiAdapter {
    #[must_use]
    pub fn with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            provider: Provider::Pi,
            sessions_root: Some(sessions_root.into()),
        }
    }

    #[must_use]
    pub fn oh_my_pi() -> Self {
        Self {
            provider: Provider::OhMyPi,
            sessions_root: crate::oh_my_pi_sessions_root(),
        }
    }

    #[must_use]
    pub fn oh_my_pi_with_root(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            provider: Provider::OhMyPi,
            sessions_root: Some(sessions_root.into()),
        }
    }

    fn session_files(&self) -> impl Iterator<Item = Result<SelectedPiFile>> + '_ {
        let root = self.sessions_root.as_deref();
        let mut files = root.map(ProviderFiles::new);
        root.map(|root| nested_files(root, 1, None))
            .unwrap_or_default()
            .into_iter()
            .filter(|path| is_session_file(path))
            .filter_map(move |path| {
                Some(SelectedPiFile::open(
                    files.as_mut()?.resolve(&path)?,
                    path,
                    self.provider,
                ))
            })
    }

    fn find_file(&self, id: &str) -> Result<Option<SelectedPiFile>> {
        self.session_files()
            .find_map(|result| match result {
                Ok(file) if file.header.id == id => Some(Ok(file)),
                Err(error)
                    if error
                        .downcast_ref::<ChangedPiHeader>()
                        .is_some_and(|header| header.id.as_deref() == Some(id)) =>
                {
                    Some(Err(error))
                }
                _ => None,
            })
            .transpose()
    }

    fn hinted_file(&self, id: &str, path: &Path) -> Option<SelectedPiFile> {
        let canonical = provider_file(self.sessions_root.as_deref()?, path)?;
        if !is_session_file(&canonical) {
            return None;
        }
        SelectedPiFile::open(canonical, path.to_path_buf(), self.provider)
            .ok()
            .filter(|file| file.header.id == id)
    }

    fn select_file(&self, id: &str, source_path: Option<&Path>) -> Result<SelectedPiFile> {
        if let Some(file) = source_path.and_then(|path| self.hinted_file(id, path)) {
            return Ok(file);
        }
        self.find_file(id)?
            .ok_or_else(|| anyhow!("Pi session `{id}` was not found"))
    }

    fn sessions_in_project(
        &self,
        files: impl Iterator<Item = SelectedPiFile>,
        project: Option<&Path>,
    ) -> Vec<NativeSession> {
        let mut sessions: Vec<_> = files
            .filter(|file| project.is_none_or(|project| paths_match(&file.header.cwd, project)))
            .map(|file| file.native_session(self.provider))
            .collect();
        sort_sessions(&mut sessions);
        sessions
    }
}

fn is_session_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

#[derive(Eq, PartialEq)]
struct DirectoryRevision {
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    identity: (u64, u64),
    #[cfg(not(unix))]
    created: SystemTime,
}

impl DirectoryRevision {
    fn read(path: &Path, root: &Path) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let metadata = if path == root {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        }
        .ok()?;
        if !metadata.is_dir() {
            return None;
        }
        #[cfg(windows)]
        let info =
            winapi_util::file::information(winapi_util::Handle::from_path_any(path).ok()?).ok()?;
        Some(Self {
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            #[cfg(windows)]
            identity: (info.volume_serial_number(), info.file_index()),
            #[cfg(not(unix))]
            created: metadata.created().ok()?,
        })
    }
}

#[derive(Default)]
struct InventoryFile {
    revision: Option<SourceRevision>,
    native: Option<NativeSession>,
    changing_id: Option<String>,
    prefix: Option<HeaderPrefix>,
    read_denied: bool,
}

impl InventoryFile {
    fn refresh(&mut self, path: &Path, resolver: &mut ProviderFiles, provider: Provider) {
        let revision = crate::source_file_revision(path).filter(|r| r.contents.is_some());
        if !cfg!(windows) && revision.is_some() && self.revision == revision {
            return;
        }
        *self = Self::default();
        let Some(canonical) = resolver.resolve(path) else {
            self.read_denied = path
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
            return;
        };
        match SelectedPiFile::open_header(canonical, path.to_owned(), provider, true) {
            Ok(file) => {
                self.revision = Some(file.observed.revision).filter(|r| r.contents.is_some());
                self.native = Some(file.native_session(provider));
                self.prefix = file.prefix;
            }
            Err(error) => {
                if let Some(changed) = error.downcast_ref::<ChangedPiHeader>() {
                    self.changing_id.clone_from(&changed.id);
                    self.prefix.clone_from(&changed.prefix);
                } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
                    self.read_denied = error.kind() == std::io::ErrorKind::PermissionDenied;
                } else {
                    // A stable invalid header can become a competing ID after an in-place edit.
                    self.revision =
                        revision.filter(|r| Some(*r) == crate::source_file_revision(path));
                }
            }
        }
    }

    #[cfg(windows)]
    fn header_still_matches(&self, root: &Path, path: &Path, provider: Provider) -> bool {
        if let Some(prefix) = &self.prefix {
            return prefix.confirm(root, path).is_some();
        }
        // A previously invalid header can become eligible without changing its file stamp.
        provider_file(root, path).is_some_and(|canonical| {
            SelectedPiFile::open_header(canonical, path.to_owned(), provider, false)
                .is_err_and(|error| !error.is::<std::io::Error>() && !error.is::<ChangedPiHeader>())
        })
    }

    fn still_denies_read(root: &Path, path: &Path) -> bool {
        let access = match provider_file(root, path) {
            Some(canonical) => fs::File::open(canonical).map(drop),
            None => path.symlink_metadata().map(drop),
        };
        access.is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    }
}

struct PiSelectionInventory {
    provider: Provider,
    root: PathBuf,
    directories: Vec<(PathBuf, Option<DirectoryRevision>)>,
    files: BTreeMap<PathBuf, InventoryFile>,
    winners: HashMap<String, PathBuf>,
}

impl PiSelectionInventory {
    fn new(provider: Provider, root: &Path) -> Self {
        Self {
            provider,
            root: root.to_owned(),
            directories: Vec::new(),
            files: BTreeMap::new(),
            winners: HashMap::new(),
        }
    }

    fn refresh_membership(&mut self) {
        // Observe each directory before listing it. A concurrent addition invalidates this inventory.
        let directories = RefCell::new(vec![(
            self.root.clone(),
            DirectoryRevision::read(&self.root, &self.root),
        )]);
        let paths = nested_files_matching(&self.root, 1, &|path, directory| {
            if directory {
                directories
                    .borrow_mut()
                    .push((path.to_owned(), DirectoryRevision::read(path, &self.root)));
            }
            true
        });
        let mut previous = std::mem::take(&mut self.files);
        self.files = paths
            .into_iter()
            .filter(|path| is_session_file(path))
            .map(|path| {
                let observation = previous.remove(&path).unwrap_or_default();
                (path, observation)
            })
            .collect();
        self.directories = directories.into_inner();
    }

    #[cfg(windows)]
    fn first_membership_change(&self) -> Option<PathBuf> {
        use std::collections::BTreeSet;
        // Windows can give the directory the same write time before and after adding an entry.
        // Compare membership once for the whole batch, using the same bounded walk as discovery.
        let current: BTreeSet<_> = nested_files(&self.root, 1, None)
            .into_iter()
            .filter(|path| is_session_file(path))
            .collect();
        let added = current.iter().find(|path| !self.files.contains_key(*path));
        let removed = self.files.keys().find(|path| !current.contains(*path));
        added.into_iter().chain(removed).min().cloned()
    }

    fn directory_changed(&self, path: &Path, known: Option<&DirectoryRevision>) -> bool {
        known.is_none() || DirectoryRevision::read(path, &self.root).as_ref() != known
    }
}

impl NativeSelectionInventory for PiSelectionInventory {
    fn refresh(&mut self) -> Result<()> {
        if cfg!(windows)
            || self.directories.is_empty()
            || self
                .directories
                .iter()
                .any(|(path, known)| self.directory_changed(path, known.as_ref()))
        {
            self.refresh_membership();
        }
        let mut resolver = ProviderFiles::new(&self.root);
        self.winners.clear();
        for (path, file) in &mut self.files {
            file.refresh(path, &mut resolver, self.provider);
            let Some(id) = file
                .native
                .as_ref()
                .map(|native| &native.session.id)
                .or(file.changing_id.as_ref())
            else {
                continue;
            };
            self.winners
                .entry(id.clone())
                .or_insert_with(|| path.clone());
        }
        Ok(())
    }

    fn selected(&self, id: &str) -> Option<&NativeSession> {
        self.files.get(self.winners.get(id)?)?.native.as_ref()
    }

    fn validate(&self, requests: &[(&str, &Path, SourceRevision)]) -> Vec<bool> {
        let Some(last) = requests.iter().map(|(_, path, _)| *path).max() else {
            return Vec::new();
        };
        // Selection uses path order. An earlier changed directory could contain a new winning file,
        // and a changed header could now name any later session in the batch.
        let mut changed_sources = HashSet::new();
        let first_file = self
            .files
            .iter()
            .take_while(|(path, _)| path.as_path() <= last)
            .find(|(path, file)| {
                if file.read_denied {
                    // An unreadable file cannot win an ID. Check access again before committing.
                    return !InventoryFile::still_denies_read(&self.root, path);
                }
                let current = crate::source_file_revision(path);
                if current.is_some() && current == file.revision {
                    #[cfg(windows)]
                    return !file.header_still_matches(&self.root, path, self.provider);
                    #[cfg(not(windows))]
                    return false;
                }
                changed_sources.insert(path.as_path());
                !file.prefix.as_ref().is_some_and(|prefix| {
                    current.and_then(|r| r.file) == Some(prefix.object)
                        && prefix.confirm(&self.root, path).is_some()
                })
            })
            .map(|(path, _)| path.as_path());
        let first_change = self
            .directories
            .iter()
            .filter(|(path, revision)| {
                path.as_path() <= first_file.unwrap_or(last)
                    && self.directory_changed(path, revision.as_ref())
            })
            .map(|(path, _)| path.as_path())
            .min()
            .or(first_file);
        #[cfg(windows)]
        let membership = self.first_membership_change();
        #[cfg(windows)]
        let first_change = first_change.into_iter().chain(membership.as_deref()).min();
        requests
            .iter()
            .map(|(id, path, revision)| {
                first_change.is_none_or(|first| *path < first)
                    && !changed_sources.contains(path)
                    && self.selected(id).is_some_and(|native| {
                        native.source_path.as_deref() == Some(*path)
                            && native
                                .source_revision
                                .is_some_and(|r| r.matches_file_contents(*revision))
                    })
            })
            .collect()
    }
}

impl Default for PiAdapter {
    fn default() -> Self {
        let sessions_root = env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                provider_root("PI_CODING_AGENT_DIR", &[".pi", "agent"])
                    .map(|root| root.join("sessions"))
            });
        Self {
            provider: Provider::Pi,
            sessions_root,
        }
    }
}

#[derive(Clone, Debug)]
struct PiHeader {
    id: String,
    title: Option<String>,
    cwd: PathBuf,
    timestamp: Option<DateTime<Utc>>,
}

fn header(records: &[Value], provider: Provider) -> Result<PiHeader> {
    let record = records
        .first()
        .context("Pi session contains no valid JSONL records")?;
    if provider == Provider::OhMyPi && record["type"] == "title" {
        let mut header = header_record(
            records
                .get(1)
                .context("Oh My Pi title slot has no session header")?,
        )?;
        header.title = string_at(record, &[&["title"]]).map(str::to_owned);
        return Ok(header);
    }
    header_record(record)
}

fn header_record(record: &Value) -> Result<PiHeader> {
    if record.get("type").and_then(Value::as_str) != Some("session") {
        bail!("Pi session did not start with a session header");
    }
    if record.get("version").and_then(Value::as_u64) != Some(PI_SESSION_VERSION) {
        bail!("Pi session is not supported session format v{PI_SESSION_VERSION}");
    }
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Pi session header omitted ID")?
        .to_owned();
    let cwd = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .context("Pi session header omitted working directory")?;
    Ok(PiHeader {
        id,
        title: string_at(record, &[&["title"]]).map(str::to_owned),
        cwd: PathBuf::from(cwd),
        timestamp: parse_timestamp(record.get("timestamp")),
    })
}

#[derive(Clone, Debug)]
struct HeaderPrefix {
    object: Uuid,
    length: u64,
    digest: [u8; 32],
    newline: bool,
}

impl HeaderPrefix {
    // A provider can append while we check another session. Only its header decides which ID it shadows.
    fn confirm(&self, root: &Path, path: &Path) -> Option<()> {
        let canonical = provider_file(root, path)?;
        let mut file = fs::File::open(&canonical).ok()?;
        let opened = ObservedSourceFile::from_open_file(&canonical, &file, &file.metadata().ok()?)?;
        (opened.revision.file == Some(self.object)).then_some(())?;
        let mut bytes = Vec::with_capacity(usize::try_from(self.length).ok()?);
        (&mut file).take(self.length).read_to_end(&mut bytes).ok()?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != self.digest {
            return None;
        }
        if !self.newline {
            let mut next = [0];
            if file.read(&mut next).ok()? != 0 && next[0] != b'\n' {
                return None;
            }
        }
        (crate::source_file_revision(path)?.file == Some(self.object)).then_some(())
    }
}

/// Skips malformed leading lines, then requires the first parsed entry to be a v3 session header.
fn read_header(
    reader: &mut BufReader<fs::File>,
    provider: Provider,
    record_prefix: Option<Uuid>,
) -> Result<(PiHeader, Option<HeaderPrefix>)> {
    let mut scanned = 0_u64;
    let mut title_seen = false;
    let mut title = None;
    let mut digest = record_prefix.map(|object| (object, Sha256::new()));
    while scanned < MAX_HEADER_SCAN_BYTES {
        let remaining = MAX_HEADER_SCAN_BYTES - scanned;
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take(remaining.min(MAX_HEADER_LINE_BYTES) + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let read = u64::try_from(read)?;
        scanned = scanned.saturating_add(read);
        if read > MAX_HEADER_LINE_BYTES {
            bail!("Pi session header line exceeds safe scan limit");
        }
        if let Some((_, digest)) = &mut digest {
            digest.update(&line);
        }
        let first = line.iter().position(|byte| !byte.is_ascii_whitespace());
        let last = line.iter().rposition(|byte| !byte.is_ascii_whitespace());
        let Some((first, last)) = first.zip(last) else {
            continue;
        };
        if let Ok(record) = serde_json::from_slice::<Value>(&line[first..=last]) {
            if provider == Provider::OhMyPi && !title_seen && record["type"] == "title" {
                title_seen = true;
                title = string_at(&record, &[&["title"]]).map(str::to_owned);
                continue;
            }
            let mut header = header_record(&record)?;
            if title_seen {
                header.title = title;
            }
            let prefix = digest.map(|(object, digest)| HeaderPrefix {
                object,
                length: scanned,
                digest: digest.finalize().into(),
                newline: line.ends_with(b"\n"),
            });
            return Ok((header, prefix));
        }
    }
    bail!("Pi session contains no valid v3 header within safe scan limit")
}

#[derive(Default)]
enum HeaderState {
    #[default]
    Empty,
    OhMyPiTitle(Option<String>),
    Parsed(Result<PiHeader>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BranchEntryKind {
    Compaction,
    Reset,
    Other,
}

fn branch_entry_kind(provider: Provider, entry: &Value) -> BranchEntryKind {
    match entry["type"].as_str() {
        Some("compaction") => BranchEntryKind::Compaction,
        Some("reset_boundary") if provider == Provider::OhMyPi => BranchEntryKind::Reset,
        _ => BranchEntryKind::Other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionRole {
    Unmarked,
    Current,
    Retained,
}

#[derive(Clone, Debug)]
struct PiTopologyEntry {
    parent_id: Option<String>,
    kind: BranchEntryKind,
    first_kept_entry_id: Option<String>,
    location: JsonLineLocation,
}

#[derive(Default)]
struct PiTranscriptIndex {
    header: HeaderState,
    latest_title: Option<String>,
    latest_timestamp: Option<DateTime<Utc>>,
    by_id: HashMap<String, PiTopologyEntry>,
    leaf: Option<String>,
}

impl PiTranscriptIndex {
    fn observe(&mut self, provider: Provider, location: JsonLineLocation, record: &Value) {
        let first_record = matches!(&self.header, HeaderState::Empty);
        let header = match &self.header {
            HeaderState::Empty => Some(
                if provider == Provider::OhMyPi && record["type"] == "title" {
                    HeaderState::OhMyPiTitle(string_at(record, &[&["title"]]).map(str::to_owned))
                } else {
                    HeaderState::Parsed(header_record(record))
                },
            ),
            HeaderState::OhMyPiTitle(title) => Some(HeaderState::Parsed(
                header_record(record).map(|mut header| {
                    header.title.clone_from(title);
                    header
                }),
            )),
            HeaderState::Parsed(_) => None,
        };
        if let Some(header) = header {
            self.header = header;
        }

        if let Some(timestamp) = parse_timestamp(record.get("timestamp")) {
            self.latest_timestamp = Some(
                self.latest_timestamp
                    .take()
                    .map_or(timestamp, |latest| latest.max(timestamp)),
            );
        }
        if provider != Provider::OhMyPi
            && matches!(
                record["type"].as_str(),
                Some("session_info" | "title" | "title_change" | "session")
            )
            && let Some(title) = string_at(record, &[&["name"], &["title"]])
        {
            self.latest_title = Some(title.to_owned());
        }

        if first_record || record.get("type").and_then(Value::as_str) == Some("session") {
            return;
        }
        let Some(id) = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let kind = branch_entry_kind(provider, record);
        self.by_id.insert(
            id.to_owned(),
            PiTopologyEntry {
                parent_id: record
                    .get("parentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind,
                first_kept_entry_id: (kind == BranchEntryKind::Compaction)
                    .then(|| {
                        record
                            .get("firstKeptEntryId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten(),
                location,
            },
        );
        self.leaf = Some(id.to_owned());
    }

    fn take_header(&mut self) -> Result<PiHeader> {
        match std::mem::take(&mut self.header) {
            HeaderState::Empty => Err(anyhow!("Pi session contains no valid JSONL records")),
            HeaderState::OhMyPiTitle(_) => {
                Err(anyhow!("Oh My Pi title slot has no session header"))
            }
            HeaderState::Parsed(header) => header,
        }
    }
}

struct SelectedPiFile {
    canonical: PathBuf,
    selected: PathBuf,
    reader: BufReader<fs::File>,
    header: PiHeader,
    prefix: Option<HeaderPrefix>,
    observed: ObservedSourceFile,
    length: u64,
}

#[derive(Debug)]
struct ChangedPiHeader {
    id: Option<String>,
    prefix: Option<HeaderPrefix>,
}

impl std::fmt::Display for ChangedPiHeader {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Pi transcript changed during header discovery")
    }
}

impl std::error::Error for ChangedPiHeader {}

impl SelectedPiFile {
    fn open(canonical: PathBuf, selected: PathBuf, provider: Provider) -> Result<Self> {
        Self::open_header(canonical, selected, provider, false)
    }

    fn open_header(
        canonical: PathBuf,
        selected: PathBuf,
        provider: Provider,
        record_prefix: bool,
    ) -> Result<Self> {
        let file = fs::File::open(&canonical)?;
        let metadata = file.metadata()?;
        let observed = ObservedSourceFile::from_open_file(&canonical, &file, &metadata)
            .context("Pi session path is not a regular file")?;
        let mut reader = BufReader::new(file);
        let header = read_header(
            &mut reader,
            provider,
            observed.revision.file.filter(|_| record_prefix),
        );
        if observed
            .ensure_unchanged(&canonical, &selected, reader.get_ref())
            .is_err()
        {
            let (id, prefix) = header
                .map(|(h, prefix)| (Some(h.id), prefix))
                .unwrap_or_default();
            return Err(ChangedPiHeader { id, prefix }.into());
        }
        let (header, prefix) = header?;
        Ok(Self {
            canonical,
            selected,
            reader,
            header,
            prefix,
            observed,
            length: metadata.len(),
        })
    }

    fn native_session(&self, provider: Provider) -> NativeSession {
        NativeSession {
            session: SessionRef::new(provider, self.header.id.clone()),
            title: self.header.title.clone(),
            project_path: Some(self.header.cwd.clone()),
            git_branch: None,
            created_at: self.header.timestamp,
            updated_at: self.observed.modified.or(self.header.timestamp),
            updated_at_approximate: self.observed.modified.is_some(),
            event_count: 0,
            source_path: Some(self.selected.clone()),
            source_revision: Some(self.observed.revision),
        }
    }

    fn full_snapshot(&mut self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        let mut index = PiTranscriptIndex::default();
        let mut parsed_suffix = 0..0;
        let oversized_records = visit_json_lines_with_offsets(
            &mut self.reader,
            MAX_COLLECTED_TRANSCRIPT_FILE_SIZE,
            |location, record| {
                if location.offset != parsed_suffix.end {
                    parsed_suffix.start = location.offset;
                }
                parsed_suffix.end = location.offset + location.length;
                index.observe(session.provider, location, &record);
                Ok(())
            },
        )?;
        self.observed
            .ensure_handle_unchanged(&self.canonical, self.reader.get_ref())?;

        let header = index.take_header()?;
        if header.id != session.id {
            bail!(
                "Pi session file identifies `{}`, not requested session `{}`",
                header.id,
                session.id
            );
        }
        let captured_at = index
            .latest_timestamp
            .unwrap_or_else(|| header.timestamp.unwrap_or_else(Utc::now));
        let locations = topology_path(&index).map_err(|problem| {
            let (message, confirmed) = match problem {
                TreeProblem::MissingParent => ("Pi session tree contains a missing leaf", false),
                TreeProblem::Cycle { earliest_link } => (
                    "Pi session tree contains a parent cycle",
                    parsed_suffix.end == self.length && parsed_suffix.contains(&earliest_link),
                ),
            };
            if oversized_records == 0 || confirmed {
                anyhow::Error::new(InvalidPiTree {
                    source: self.native_session(session.provider),
                    problem: message,
                })
            } else {
                // An omitted record could contain a missing link or a newer leaf.
                crate::support::TranscriptReadLimit(message).into()
            }
        })?;
        let mut title = if session.provider == Provider::OhMyPi {
            header.title.clone()
        } else {
            index.latest_title.take()
        };
        drop(index);
        let mut builder = EventBuilder::new(session.provider, &session.id);
        builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
        let mut record_line = Vec::new();
        let mut read_position = self.reader.stream_position()?;
        for (location, role) in locations {
            let entry = read_json_line_at(
                &mut self.reader,
                location,
                &mut read_position,
                &mut record_line,
            )?;
            if session.provider == Provider::OhMyPi
                && let Some(name) = omp_branch_title(&entry)
            {
                title = Some(name.to_owned());
            }
            emit_entry(&mut builder, session.provider, &entry, role);
        }
        builder.push_oversized_record_notice(oversized_records, Some(captured_at));
        Ok(builder.snapshot(session.clone(), title, Some(header.cwd), None, captured_at))
    }

    fn preview_snapshot(&mut self, session: &SessionRef) -> Result<CanonicalSnapshot> {
        let (records, suffix_len) =
            json_lines_preview_file_with_suffix(self.reader.get_mut(), PREVIEW_RECORDS)?;
        snapshot_from_records_preview(session, &records, suffix_len)
    }

    // A strict full read propagates the provider limit. A budgeted index read can sample the same file.
    fn read(mut self, session: &SessionRef, budget: Option<u64>) -> Result<IndexRead> {
        ensure!(
            self.observed.revision.contents.is_some(),
            "cannot observe Pi transcript contents"
        );
        let result = if let Some(limit) = budget {
            read_budgeted_transcript(self.length <= limit, |preview| {
                if preview {
                    self.preview_snapshot(session)
                } else {
                    self.full_snapshot(session)
                }
            })
        } else {
            self.full_snapshot(session).map(|snapshot| IndexRead {
                ownership_revision: None,
                native_session: None,
                rollback_title: crate::RollbackTitle::Unknown,
                workspace_projection: crate::CodexWorkspaceProjection::Unchecked,
                snapshot,
                source_complete: true,
                provider_limited: false,
                source_revision: self.observed.revision,
            })
        };
        self.observed
            .ensure_unchanged(&self.canonical, &self.selected, self.reader.get_ref())?;
        let mut read = result?;
        read.native_session = Some(self.native_session(session.provider));
        read.source_revision = self.observed.revision;
        Ok(read)
    }
}

enum TreeProblem {
    MissingParent,
    Cycle { earliest_link: u64 },
}

fn topology_path(
    index: &PiTranscriptIndex,
) -> Result<Vec<(JsonLineLocation, CompactionRole)>, TreeProblem> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = index.leaf.as_deref();
    let mut earliest_link = u64::MAX;
    while let Some(id) = current {
        if !seen.insert(id) {
            return Err(TreeProblem::Cycle { earliest_link });
        }
        let entry = index.by_id.get(id).ok_or(TreeProblem::MissingParent)?;
        earliest_link = earliest_link.min(entry.location.offset);
        path.push((id, entry));
        current = entry.parent_id.as_deref();
    }
    path.reverse();

    Ok(order_compaction(
        path,
        |(_, entry)| entry.kind,
        |(_, entry)| entry.first_kept_entry_id.as_deref(),
        |(id, _)| Some(*id),
    )
    .map(|((_, entry), role)| (entry.location, role))
    .collect())
}

fn latest_timestamp(records: &[Value], fallback: DateTime<Utc>) -> DateTime<Utc> {
    records
        .iter()
        .filter_map(|record| parse_timestamp(record.get("timestamp")))
        .max()
        .unwrap_or(fallback)
}

#[cfg(test)]
fn session_path(
    records: &[Value],
    provider: Provider,
    allow_missing_ancestors: bool,
) -> Result<Vec<(&Value, CompactionRole)>> {
    let path = selected_branch(records, allow_missing_ancestors.then_some(records.len()))?;
    Ok(ordered_entries(path, provider).collect())
}

fn ordered_entries(
    path: Vec<&Value>,
    provider: Provider,
) -> impl Iterator<Item = (&Value, CompactionRole)> {
    order_compaction(
        path,
        move |entry| branch_entry_kind(provider, entry),
        |entry| entry["firstKeptEntryId"].as_str(),
        |entry| entry["id"].as_str(),
    )
}

fn selected_branch(records: &[Value], sampled_suffix_len: Option<usize>) -> Result<Vec<&Value>> {
    let mut by_id = HashMap::new();
    let mut leaf = None;
    for entry in records
        .iter()
        .skip(1)
        .filter(|record| record["type"] != "session")
    {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        by_id.insert(id, entry);
        leaf = Some(id);
    }

    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = leaf;
    while let Some(id) = current {
        if !seen.insert(id) {
            if let Some(length) = sampled_suffix_len {
                let suffix = records[records.len().saturating_sub(length)..].as_ptr_range();
                if !path
                    .iter()
                    .all(|entry| suffix.contains(&std::ptr::from_ref(*entry)))
                {
                    return Err(crate::TranscriptReadLimit(
                        "Pi session tree contains a parent cycle",
                    )
                    .into());
                }
            }
            bail!("Pi session tree contains a parent cycle");
        }
        let Some(entry) = by_id.get(id).copied() else {
            if sampled_suffix_len.is_some() {
                break;
            }
            bail!("Pi session tree contains a missing leaf");
        };
        path.push(entry);
        current = entry.get("parentId").and_then(Value::as_str);
    }
    path.reverse();
    Ok(path)
}

/// Position the latest compaction ahead of its retained entries and identify older summaries in that range.
/// An Oh My Pi clear starts a new task. Keep its marker before any subsequent compaction, even if
/// the compaction's retained entry ID points into the previous task.
fn order_compaction<T>(
    mut path: Vec<T>,
    entry_kind: impl Fn(&T) -> BranchEntryKind,
    first_kept_id: impl Fn(&T) -> Option<&str>,
    entry_id: impl Fn(&T) -> Option<&str>,
) -> impl Iterator<Item = (T, CompactionRole)> {
    let mut compactions = path
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry_kind(entry) == BranchEntryKind::Compaction);
    let latest = compactions.next().map(|(index, _)| index);
    let has_earlier_compaction = compactions.next().is_some();
    let boundary = latest.map(|latest| {
        let mut start = first_kept_id(&path[latest])
            .and_then(|id| {
                path[..latest]
                    .iter()
                    .position(|entry| entry_id(entry) == Some(id))
            })
            .unwrap_or(latest);
        if let Some(reset) = path[start..latest]
            .iter()
            .rposition(|entry| entry_kind(entry) == BranchEntryKind::Reset)
        {
            start += reset + 1;
        }
        path[start..=latest].rotate_right(1);
        start
    });
    path.into_iter().enumerate().map(move |(index, entry)| {
        let role = match boundary {
            Some(start) if index == start && has_earlier_compaction => CompactionRole::Current,
            Some(start) if index > start && entry_kind(&entry) == BranchEntryKind::Compaction => {
                CompactionRole::Retained
            }
            _ => CompactionRole::Unmarked,
        };
        (entry, role)
    })
}

fn text_blocks(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => vec![text.to_owned()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn raw_type(entry: &Value) -> Option<String> {
    entry.get("type").and_then(Value::as_str).map(str::to_owned)
}

fn entry_timestamp(entry: &Value) -> Option<DateTime<Utc>> {
    parse_timestamp(entry.get("timestamp"))
}

fn emit_entry(
    builder: &mut EventBuilder,
    provider: Provider,
    entry: &Value,
    compaction_role: CompactionRole,
) {
    let timestamp = entry_timestamp(entry);
    let raw_type = raw_type(entry);
    match raw_type.as_deref() {
        Some("message") => emit_message(builder, entry, timestamp, raw_type),
        Some("compaction") => {
            let mut payload = json!({
                "summary": entry.get("summary"),
                "first_kept_entry_id": entry.get("firstKeptEntryId"),
                "tokens_before": entry.get("tokensBefore"),
            });
            match compaction_role {
                CompactionRole::Current => payload["active_compaction"] = Value::Bool(true),
                CompactionRole::Retained => payload["retained_by_compaction"] = Value::Bool(true),
                CompactionRole::Unmarked => {}
            }
            // Pi includes older retained summary messages. Oh My Pi only displays them in its archive.
            let policy =
                if compaction_role == CompactionRole::Retained && provider == Provider::OhMyPi {
                    ReplayPolicy::HistoricalOnly
                } else {
                    ReplayPolicy::Contextual
                };
            builder.push(
                EventKind::CompactionCreated,
                payload,
                timestamp,
                policy,
                raw_type,
                None,
            );
        }
        Some("reset_boundary" | "title_change") if provider == Provider::OhMyPi => builder.push(
            EventKind::ProviderEvent,
            selected_entry_metadata(entry),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        Some("branch_summary") => emit_branch_summary(builder, entry, timestamp, raw_type),
        Some("custom_message") => emit_custom_message(builder, entry, timestamp, raw_type),
        Some("model_change" | "thinking_level_change" | "label" | "session_info" | "custom") => {
            builder.push(
                EventKind::ProviderEvent,
                selected_entry_metadata(entry),
                timestamp,
                ReplayPolicy::HistoricalOnly,
                raw_type,
                None,
            );
        }
        Some(_) | None => builder.push(
            EventKind::ProviderEvent,
            json!({
                "type": "pi_unsupported_entry",
                "entry_type": raw_type,
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
    }
}

fn emit_message(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let Some(message) = entry.get("message") else {
        builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_message_without_payload" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => emit_user_message(builder, message, timestamp, raw_type.as_ref()),
        Some("assistant") => emit_assistant_message(builder, message, timestamp, raw_type.as_ref()),
        Some("toolResult") => emit_tool_result(builder, message, timestamp, raw_type),
        Some("bashExecution") => emit_bash_execution(builder, message, timestamp, raw_type),
        Some(role) => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_unsupported_message", "role": role }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
        None => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_message_without_role" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        ),
    }
}

fn emit_user_message(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    let content = message.get("content");
    let mut texts = text_blocks(content);
    // An image-only turn keeps a placeholder so the reply still follows a request.
    let images = image_blocks(content);
    if texts.is_empty() && images > 0 {
        texts.push(omitted_images_text(images));
    }
    for text in texts {
        builder.push(
            EventKind::MessageUser,
            json!({ "text": text }),
            timestamp,
            ReplayPolicy::Contextual,
            raw_type.cloned(),
            None,
        );
    }
}

fn image_blocks(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map_or(0, |blocks| {
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
            .count()
    })
}

/// Pi shows branch summaries in its chat and sends them to the model, so the summary stays visible.
fn emit_branch_summary(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let from_id = entry.get("fromId").cloned().unwrap_or(Value::Null);
    // Pi leaves an empty summary out of model context.
    let Some(summary) = string_at(entry, &[&["summary"]]) else {
        builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "branch_summary", "from_id": from_id }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    builder.push(
        EventKind::MessageAssistant,
        json!({
            "text": format!("[Source branch summary]\n{summary}"),
            "from_id": from_id,
        }),
        timestamp,
        ReplayPolicy::Contextual,
        raw_type,
        None,
    );
}

/// Pi sends extension messages to the model as user input and shows those marked `display`.
fn emit_custom_message(
    builder: &mut EventBuilder,
    entry: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let content = entry.get("content");
    let text = text_blocks(content).join("\n");
    let images = image_blocks(content);
    let body = if text.is_empty() {
        (images > 0).then(|| omitted_images_text(images))
    } else {
        Some(text)
    };
    let displayed = entry.get("display").and_then(Value::as_bool) == Some(true);
    let Some(body) = body.filter(|_| displayed) else {
        builder.push(
            EventKind::ProviderEvent,
            selected_entry_metadata(entry),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type,
            None,
        );
        return;
    };
    let label = string_at(entry, &[&["customType"]]).map_or_else(
        || "[Source extension message]".to_owned(),
        |custom_type| format!("[Source extension message: {custom_type}]"),
    );
    builder.push(
        EventKind::MessageUser,
        json!({
            "text": format!("{label}\n{body}"),
            "custom_type": entry.get("customType").cloned().unwrap_or(Value::Null),
        }),
        timestamp,
        ReplayPolicy::Contextual,
        raw_type,
        None,
    );
}

fn emit_assistant_message(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    if let Some(payload) = pi_session_metadata(message) {
        builder.push(
            EventKind::ProviderEvent,
            payload,
            timestamp,
            ReplayPolicy::HistoricalOnly,
            Some("omnisession.session_metadata".to_owned()),
            None,
        );
    }
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        emit_assistant_content(builder, block, timestamp, raw_type);
    }
}

fn pi_session_metadata(message: &Value) -> Option<Value> {
    let model = string_at(message, &[&["model"], &["modelId"]]);
    let usage = message.get("usage");
    let total_tokens = usage
        .map(|usage| {
            ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add)
        })
        .filter(|tokens| *tokens > 0);
    (model.is_some() || total_tokens.is_some()).then(|| {
        json!({
            "model": model,
            "total_tokens": total_tokens,
            "token_usage": "incremental",
        })
    })
}

fn emit_assistant_content(
    builder: &mut EventBuilder,
    block: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<&String>,
) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = block.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                builder.push(
                    EventKind::MessageAssistant,
                    json!({ "text": text }),
                    timestamp,
                    ReplayPolicy::Contextual,
                    raw_type.cloned(),
                    None,
                );
            }
        }
        Some("thinking") => builder.push(
            EventKind::ProviderEvent,
            json!({ "type": "pi_reasoning_omitted" }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
        Some("toolCall") => builder.push(
            EventKind::ToolCalled,
            json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or(Value::Null),
                "arguments": block.get("arguments").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
        Some(_) | None => builder.push(
            EventKind::ProviderEvent,
            json!({
                "type": "pi_unsupported_assistant_content",
                "content_type": block.get("type").cloned().unwrap_or(Value::Null),
            }),
            timestamp,
            ReplayPolicy::HistoricalOnly,
            raw_type.cloned(),
            None,
        ),
    }
}

fn emit_tool_result(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    let output = text_blocks(message.get("content")).join("\n");
    builder.push(
        if message.get("isError").and_then(Value::as_bool) == Some(true) {
            EventKind::ToolFailed
        } else {
            EventKind::ToolCompleted
        },
        json!({
            "tool_call_id": message.get("toolCallId").cloned().unwrap_or(Value::Null),
            "tool_name": message.get("toolName").cloned().unwrap_or(Value::Null),
            "output": output,
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        raw_type,
        None,
    );
}

fn emit_bash_execution(
    builder: &mut EventBuilder,
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    raw_type: Option<String>,
) {
    builder.push(
        EventKind::CommandExecuted,
        json!({
            "command": message.get("command").cloned().unwrap_or(Value::Null),
            "output": message.get("output").cloned().unwrap_or(Value::Null),
            "exit_code": message.get("exitCode").cloned().unwrap_or(Value::Null),
            "cancelled": message.get("cancelled").cloned().unwrap_or(Value::Null),
            "truncated": message.get("truncated").cloned().unwrap_or(Value::Null),
        }),
        timestamp,
        ReplayPolicy::HistoricalOnly,
        raw_type,
        None,
    );
}

fn selected_entry_metadata(entry: &Value) -> Value {
    let entry_type = entry.get("type").cloned().unwrap_or(Value::Null);
    match entry_type.as_str() {
        Some("model_change") => json!({
            "type": entry_type,
            "provider": entry.get("provider").cloned().unwrap_or(Value::Null),
            "model_id": entry.get("modelId").cloned().unwrap_or(Value::Null),
        }),
        Some("thinking_level_change") => json!({
            "type": entry_type,
            "thinking_level": entry.get("thinkingLevel").cloned().unwrap_or(Value::Null),
        }),
        Some("label") => json!({
            "type": entry_type,
            "target_id": entry.get("targetId").cloned().unwrap_or(Value::Null),
            "label": entry.get("label").cloned().unwrap_or(Value::Null),
        }),
        Some("session_info") => json!({
            "type": entry_type,
            "name": entry.get("name").cloned().unwrap_or(Value::Null),
        }),
        Some("reset_boundary") => json!({ "type": entry_type }),
        Some("title_change") => json!({ "type": entry_type, "title": entry.get("title") }),
        Some("custom" | "custom_message") => json!({
            "type": entry_type,
            "custom_type": entry.get("customType").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({ "type": "pi_unsupported_entry" }),
    }
}

fn session_title(records: &[Value]) -> Option<String> {
    records
        .iter()
        .rev()
        .filter(|record| {
            matches!(
                record["type"].as_str(),
                Some("session_info" | "title" | "title_change" | "session")
            )
        })
        .find_map(|record| string_at(record, &[&["name"], &["title"]]))
        .map(str::to_owned)
}

fn omp_branch_title(entry: &Value) -> Option<&str> {
    match entry["type"].as_str() {
        Some("title_change") => entry["title"].as_str(),
        Some("session_info") => entry["name"].as_str(),
        _ => None,
    }
    .filter(|title| !title.trim().is_empty())
}

#[cfg(test)]
fn snapshot_from_records(
    session: &SessionRef,
    records: &[Value],
    oversized_records: usize,
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records, session.provider)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(session.provider, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    let mut title = if session.provider == Provider::OhMyPi {
        header.title
    } else {
        session_title(records)
    };
    for (entry, role) in session_path(records, session.provider, false)? {
        if session.provider == Provider::OhMyPi
            && let Some(name) = omp_branch_title(entry)
        {
            title = Some(name.to_owned());
        }
        emit_entry(&mut builder, session.provider, entry, role);
    }
    builder.push_oversized_record_notice(oversized_records, Some(captured_at));
    Ok(builder.snapshot(session.clone(), title, Some(header.cwd), None, captured_at))
}

/// Walk the selected parent chain back from EOF. A gap or unknown entry ends the title evidence.
fn sampled_omp_title<'a>(branch: &[&'a Value], suffix: &[Value]) -> (Option<&'a str>, bool) {
    if !branch
        .last()
        .zip(suffix.last())
        .is_some_and(|(leaf, last)| std::ptr::eq(*leaf, last))
    {
        return (None, false);
    }
    let range = suffix.as_ptr_range();
    let mut name = None;
    for &entry in branch
        .iter()
        .rev()
        .take_while(|entry| range.contains(&std::ptr::from_ref(**entry)))
    {
        match entry["type"].as_str() {
            Some("title_change") => {
                name = name.or_else(|| omp_branch_title(entry));
            }
            Some("reset_boundary") => return (name, true),
            Some(
                "message"
                | "model_usage"
                | "thinking_level_change"
                | "model_change"
                | "service_tier_change"
                | "compaction"
                | "branch_summary"
                | "custom"
                | "custom_message"
                | "label"
                | "ttsr_injection"
                | "session_init"
                | "mode_change"
                | "credential_pin",
            ) => {}
            _ => break,
        }
    }
    (name, false)
}

fn snapshot_from_records_preview(
    session: &SessionRef,
    records: &[Value],
    suffix_len: usize,
) -> Result<omnis_ir::CanonicalSnapshot> {
    let header = header(records, session.provider)?;
    if header.id != session.id {
        bail!(
            "Pi session file identifies `{}`, not requested session `{}`",
            header.id,
            session.id
        );
    }
    let captured_at = latest_timestamp(records, header.timestamp.unwrap_or_else(Utc::now));
    let mut builder = EventBuilder::new(session.provider, &session.id);
    builder.set_provider_version(Some(PI_SESSION_VERSION.to_string()));
    let path = selected_branch(records, Some(suffix_len))?;
    let (rename, verified_reset) = if session.provider == Provider::OhMyPi {
        let suffix = &records[records.len().saturating_sub(suffix_len)..];
        sampled_omp_title(&path, suffix)
    } else {
        (None, false)
    };
    let title = if session.provider == Provider::OhMyPi {
        rename.map(str::to_owned).or(header.title)
    } else {
        session_title(records)
    };
    for (entry, role) in ordered_entries(path, session.provider) {
        emit_entry(&mut builder, session.provider, entry, role);
    }
    let mut snapshot =
        builder.snapshot(session.clone(), title, Some(header.cwd), None, captured_at);
    if let Some(name) = rename
        && let Some(event) = snapshot.events.iter_mut().rfind(|event| {
            event.source.raw_record_type.as_deref() == Some("title_change")
                && event.payload["title"] == name
        })
    {
        event.payload["verified_through_leaf"] = true.into();
    }
    if session.provider == Provider::OhMyPi
        && let Some(reset) = snapshot
            .events
            .iter_mut()
            .rfind(|event| event.source.raw_record_type.as_deref() == Some("reset_boundary"))
    {
        reset.payload["verified_through_leaf"] = verified_reset.into();
    }
    Ok(snapshot)
}

impl ProviderAdapter for PiAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn probe(&self) -> ProviderInstallation {
        let executable = provider_executable(self.provider);
        ProviderInstallation {
            provider: self.provider,
            installed: executable.is_some()
                || self.sessions_root.as_deref().is_some_and(Path::is_dir),
            executable,
            data_root: self.sessions_root.clone(),
        }
    }

    fn list_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        Ok(self.sessions_in_project(self.session_files().filter_map(Result::ok), project))
    }

    fn list_search_sessions(&self, project: Option<&Path>) -> Result<Vec<NativeSession>> {
        self.list_search_sessions_with_notes(project)
            .map(|(rows, _)| rows)
    }

    fn list_search_sessions_with_notes(
        &self,
        project: Option<&Path>,
    ) -> Result<(Vec<NativeSession>, Vec<String>)> {
        let mut seen = HashSet::new();
        let mut changed = false;
        let selected = self.session_files().filter_map(|result| match result {
            Ok(file) if seen.insert(file.header.id.clone()) => Some(file),
            Err(error) => {
                if let Some(header) = error.downcast_ref::<ChangedPiHeader>() {
                    changed = true;
                    if let Some(id) = &header.id {
                        seen.insert(id.clone());
                    }
                }
                None
            }
            Ok(_) => None,
        });
        let sessions = self.sessions_in_project(selected, project);
        let notes = if changed {
            vec![format!(
                "{} discovery was incomplete. A transcript changed during header discovery. The next refresh will check it again.",
                self.provider
            )]
        } else {
            Vec::new()
        };
        Ok((sessions, notes))
    }

    fn selected_session_at(&self, id: &str, source_path: &Path) -> Result<Option<NativeSession>> {
        Ok(self
            .find_file(id)?
            .filter(|file| file.selected == source_path)
            .map(|file| file.native_session(self.provider)))
    }

    fn selection_inventory(&self) -> Result<Option<Box<dyn NativeSelectionInventory>>> {
        let Some(root) = self.sessions_root.as_ref() else {
            return Ok(None);
        };
        let mut inventory = PiSelectionInventory::new(self.provider, root);
        inventory.refresh()?;
        Ok(Some(Box::new(inventory)))
    }

    fn read_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.read_session_at(session, None)
    }

    fn read_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        Ok(self.read_full_index_at(session, source_path)?.snapshot)
    }

    fn read_full_index_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<IndexRead> {
        validate_provider(session, self.provider)?;
        self.select_file(&session.id, source_path)?
            .read(session, None)
    }

    fn index_session(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
        full_read_bytes: u64,
    ) -> Result<IndexRead> {
        validate_provider(session, self.provider)?;
        self.select_file(&session.id, source_path)?
            .read(session, Some(full_read_bytes))
    }

    fn search_timestamp_file(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        validate_provider(session, self.provider)?;
        Ok(Some(self.select_file(&session.id, source_path)?.selected))
    }

    fn preview_session(&self, session: &SessionRef) -> Result<omnis_ir::CanonicalSnapshot> {
        self.preview_session_at(session, None)
    }

    fn preview_session_at(
        &self,
        session: &SessionRef,
        source_path: Option<&Path>,
    ) -> Result<omnis_ir::CanonicalSnapshot> {
        Ok(self.index_session(session, source_path, 0)?.snapshot)
    }

    fn new_session_plan(&self, target: &LaunchTarget) -> Result<LaunchPlan> {
        let mut args = Vec::new();
        if self.provider == Provider::OhMyPi
            && env::var_os("OMP_SESSION_DIR").is_some_and(|value| !value.is_empty())
            && let Some(root) = &self.sessions_root
        {
            args.extend([
                "--session-dir".to_owned(),
                root.to_string_lossy().into_owned(),
            ]);
        }
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: self
                .provider
                .command()
                .context("missing Pi-family command")?
                .to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }

    fn launch_plan(&self, session: &SessionRef, target: &LaunchTarget) -> Result<LaunchPlan> {
        validate_provider(session, self.provider)?;
        let reference = if self.provider == Provider::OhMyPi {
            self.select_file(&session.id, None)?
                .selected
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow!("Oh My Pi session path is not UTF-8"))?
        } else {
            session.id.clone()
        };
        let mut args = if target.fork {
            vec!["--fork".to_owned(), reference]
        } else {
            vec!["--session".to_owned(), reference]
        };
        if self.provider == Provider::OhMyPi
            && target.fork
            && env::var_os("OMP_SESSION_DIR").is_some_and(|value| !value.is_empty())
            && let Some(root) = &self.sessions_root
        {
            args.extend([
                "--session-dir".to_owned(),
                root.to_string_lossy().into_owned(),
            ]);
        }
        args.extend(target.prompt.iter().cloned());
        Ok(LaunchPlan {
            program: self
                .provider
                .command()
                .context("missing Pi-family command")?
                .to_owned(),
            args,
            cwd: target.cwd.clone(),
            env: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(root: &Path, filename: &str, contents: &str) -> PathBuf {
        let directory = root.join("project");
        fs::create_dir_all(&directory).expect("create synthetic session directory");
        let path = directory.join(filename);
        fs::write(&path, contents).expect("write synthetic session");
        path
    }

    fn serialized_records(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn assert_matches_snapshot_oracle(
        adapter: &PiAdapter,
        session: &SessionRef,
        path: &Path,
        records: &[Value],
        oversized_records: usize,
    ) -> omnis_ir::CanonicalSnapshot {
        let actual = adapter
            .read_session_at(session, Some(path))
            .expect("read indexed Pi transcript");
        let expected = snapshot_from_records(session, records, oversized_records)
            .expect("read in-memory Pi snapshot oracle");
        assert_eq!(
            serde_json::to_value(&actual).expect("serialize actual snapshot"),
            serde_json::to_value(&expected).expect("serialize oracle snapshot")
        );
        actual
    }

    #[test]
    fn discovered_path_hint_reads_without_rewalking_provider_tree() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path();
        let path = root.join("project/nested/session.jsonl");
        fs::create_dir_all(path.parent().expect("session parent")).expect("create session tree");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"expected\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("synthetic Pi session");
        let adapter = PiAdapter::with_root(root);
        let session = SessionRef::new(Provider::Pi, "expected");

        let snapshot = adapter
            .read_session_at(&session, Some(&path))
            .expect("validated discovered path");

        assert!(snapshot.events.is_empty());
        assert!(adapter.read_session(&session).is_err());
    }

    #[test]
    fn search_resolves_duplicate_ids_before_filtering_by_project() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let north = temp.path().join("north");
        let south = temp.path().join("south");
        let write = |file, id: &str, project: &Path, seconds| {
            let header = json!({"type":"session","version":3,"id":id,"cwd":project});
            let path = write_fixture(&root, file, &header.to_string());
            let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            path
        };
        let chosen = write("000-first.jsonl", "same", &north, 10);
        let unique = write("500-unique.jsonl", "unique", &south, 20);
        let duplicate = write("900-duplicate.jsonl", "same", &south, 30);
        let assert_paths = |sessions: Vec<NativeSession>, expected: &[&Path]| {
            let actual: Vec<_> = sessions
                .iter()
                .map(|row| row.source_path.as_deref().unwrap())
                .collect();
            assert_eq!(actual.as_slice(), expected);
        };
        for adapter in [
            PiAdapter::with_root(&root),
            PiAdapter::oh_my_pi_with_root(&root),
        ] {
            let session = SessionRef::new(adapter.provider(), "same");
            let explicit = adapter
                .read_full_index_at(&session, None)
                .unwrap()
                .native_session
                .unwrap();
            assert_eq!(explicit.source_path.as_ref(), Some(&chosen));
            assert_eq!(
                adapter.list_search_sessions(Some(&north)).unwrap(),
                [explicit]
            );
            assert_paths(
                adapter.list_search_sessions(None).unwrap(),
                &[&unique, &chosen],
            );
            assert_paths(
                adapter.list_search_sessions(Some(&south)).unwrap(),
                &[&unique],
            );
            assert_paths(
                adapter.list_sessions(None).unwrap(),
                &[&duplicate, &unique, &chosen],
            );
            assert_paths(
                adapter.list_sessions(Some(&south)).unwrap(),
                &[&duplicate, &unique],
            );
        }
    }

    fn write_inventory_header(path: &Path, id: &str) {
        fs::write(
            path,
            serialized_records(&[json!({"type":"session","version":3,"id":id,"cwd":"/project"})]),
        )
        .unwrap();
    }

    #[test]
    fn inventory_distinguishes_appends_from_competing_header_edits() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = ["0.jsonl", "1.jsonl", "2.jsonl"].map(|name| root.join(name));
        let ids = ["first", "other", "later"];
        for (path, id) in paths.iter().zip(ids) {
            write_inventory_header(path, id);
        }
        let mut inventory = PiAdapter::with_root(root)
            .selection_inventory()
            .unwrap()
            .unwrap();
        let requests = std::array::from_fn::<_, 3, _>(|i| {
            let id = ids[i];
            let source = inventory.selected(id).unwrap().source_revision.unwrap();
            (id, paths[i].as_path(), source)
        });
        assert_eq!(inventory.validate(&requests), [true, true, true]);
        let append = || {
            fs::OpenOptions::new()
                .append(true)
                .open(&paths[1])
                .unwrap()
                .write_all(b"{}\n")
                .unwrap();
        };
        append();
        assert_eq!(inventory.validate(&requests), [true, false, true]);
        inventory.refresh().unwrap();
        let modified = fs::metadata(&paths[1]).unwrap().modified().unwrap();
        write_inventory_header(&paths[1], "later");
        append();
        fs::File::options()
            .write(true)
            .open(&paths[1])
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(inventory.validate(&requests), [true, false, false]);
        inventory.refresh().unwrap();
        assert_eq!(
            inventory.selected("later").unwrap().source_path.as_ref(),
            Some(&paths[1])
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_predecessor_only_shadows_a_session_after_access_returns() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("first");
        fs::create_dir(&directory).unwrap();
        let earlier = directory.join("earlier.jsonl");
        let later = temp.path().join("last.jsonl");
        write_inventory_header(&earlier, "same-id");
        write_inventory_header(&later, "same-id");
        let later_request = [(
            "same-id",
            later.as_path(),
            crate::source_file_revision(&later).unwrap(),
        )];
        for (restricted, mode) in [(&earlier, 0), (&directory, 0o400)] {
            let original = fs::metadata(restricted).unwrap().permissions();
            fs::set_permissions(restricted, fs::Permissions::from_mode(mode)).unwrap();
            if fs::File::open(&earlier).is_ok() {
                // A privileged user can still read this file.
                fs::set_permissions(restricted, original).unwrap();
                continue;
            }
            let mut inventory = PiAdapter::oh_my_pi_with_root(temp.path())
                .selection_inventory()
                .unwrap()
                .unwrap();
            assert_eq!(inventory.validate(&later_request), [true]);
            fs::set_permissions(restricted, original).unwrap();
            assert_eq!(inventory.validate(&later_request), [false]);
            inventory.refresh().unwrap();
            assert_eq!(
                inventory.selected("same-id").unwrap().source_path.as_ref(),
                Some(&earlier)
            );
            assert_eq!(inventory.validate(&later_request), [false]);
        }
    }

    #[test]
    fn inventory_tracks_empty_directories_and_replaced_parents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let empty = root.join("0-empty");
        let current = root.join("1-current");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir(&current).unwrap();
        let original = current.join("session.jsonl");
        write_inventory_header(&original, "wanted");
        let mut inventory = PiAdapter::oh_my_pi_with_root(&root)
            .selection_inventory()
            .unwrap()
            .unwrap();
        let request = [(
            "wanted",
            original.as_path(),
            crate::source_file_revision(&original).unwrap(),
        )];
        assert_eq!(inventory.validate(&request), [true]);
        let added = empty.join("new.jsonl");
        fs::write(&added, b"malformed header\n").unwrap();
        assert_eq!(inventory.validate(&request), [false]);
        inventory.refresh().unwrap();
        assert_eq!(inventory.validate(&request), [true]);
        write_inventory_header(&added, "wanted");
        assert_eq!(inventory.validate(&request), [false]);
        inventory.refresh().unwrap();
        assert_eq!(
            inventory.selected("wanted").unwrap().source_path.as_ref(),
            Some(&added)
        );
        let replacement = [(
            "wanted",
            added.as_path(),
            crate::source_file_revision(&added).unwrap(),
        )];
        fs::rename(&empty, temp.path().join("withdrawn")).unwrap();
        fs::create_dir(&empty).unwrap();
        assert_eq!(inventory.validate(&replacement), [false]);
        inventory.refresh().unwrap();
        assert_eq!(
            inventory.selected("wanted").unwrap().source_path.as_ref(),
            Some(&original)
        );
    }

    #[test]
    fn stale_or_wrong_session_path_hints_fall_back_to_exact_lookup() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path();
        let actual = root.join("expected.jsonl");
        let wrong = root.join("wrong.jsonl");
        fs::write(
            &actual,
            "{\"type\":\"session\",\"version\":3,\"id\":\"expected\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("expected synthetic session");
        fs::write(
            &wrong,
            "{\"type\":\"session\",\"version\":3,\"id\":\"other\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("other synthetic session");
        let stale = root.join("removed.jsonl");
        let adapter = PiAdapter::with_root(root);
        let session = SessionRef::new(Provider::Pi, "expected");

        assert!(adapter.read_session_at(&session, Some(&stale)).is_ok());
        assert!(adapter.read_session_at(&session, Some(&wrong)).is_ok());
        for other in [&stale, &wrong] {
            assert!(
                adapter
                    .selected_session_at(&session.id, other)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn an_archived_compaction_at_the_start_of_a_new_snapshot_does_not_select_older_requests() {
        let records = [
            json!({"type":"session","id":"session","version":3,"cwd":"/workspace"}),
            json!({"type":"compaction","id":"older","parentId":null,"firstKeptEntryId":"absent","summary":"Older summary"}),
            json!({"type":"message","id":"old-user","parentId":"older","message":{"role":"user","content":"An old request"}}),
            json!({"type":"compaction","id":"current","parentId":"old-user","firstKeptEntryId":"missing","summary":"Current summary"}),
        ];
        let snapshot =
            snapshot_from_records(&SessionRef::new(Provider::Pi, "session"), &records, 0).unwrap();
        let plan = omnis_core::plan_semantic_handoff(&snapshot);
        assert!(plan.objective.is_none());
        assert_eq!(plan.recent_messages.len(), 1);
        assert!(plan.recent_messages[0].text.ends_with("Current summary"));
        assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
    }

    #[test]
    fn latest_compaction_keeps_older_summaries_in_place_even_when_its_own_text_is_empty() {
        let message = |id: &str, parent: &str| {
            json!({
                "type": "message", "id": id, "parentId": parent, "message": {"role": "user", "content": id}
            })
        };
        for summary in ["latest summary", ""] {
            let records = [
                json!({"type":"session","version":3,"id":"session","cwd":"/workspace"}),
                json!({"type":"message","id":"root","parentId":null,"message":{"role":"user","content":"recover earlier decision"}}),
                message("first retained", "root"),
                json!({"type":"compaction","id":"older","parentId":"first retained","firstKeptEntryId":"first retained","summary":"older summary"}),
                message("second retained", "older"),
                message("discarded sibling", "root"),
                json!({"type":"compaction","id":"latest","parentId":"second retained","firstKeptEntryId":"first retained","summary":summary}),
                message("final turn", "latest"),
            ];
            let session = SessionRef::new(Provider::Pi, "session");
            let snapshot = snapshot_from_records(&session, &records, 0).unwrap();
            let trajectory = omnis_core::import_trajectory(&snapshot);
            let conversation = omnis_core::import_conversation(&snapshot);
            for texts in [
                trajectory
                    .items
                    .iter()
                    .map(|item| item.text.as_str())
                    .collect::<Vec<_>>(),
                conversation
                    .messages
                    .iter()
                    .map(|msg| msg.text.as_str())
                    .collect(),
            ] {
                let turns = texts
                    .iter()
                    .filter(|text| !text.starts_with("[OmniSession"))
                    .map(|text| text.lines().last().unwrap())
                    .collect::<Vec<_>>();
                let mut expected = vec![
                    "first retained",
                    "older summary",
                    "second retained",
                    "final turn",
                ];
                if !summary.is_empty() {
                    expected.insert(0, summary);
                }
                assert_eq!(turns, expected);
            }
            assert_eq!(trajectory.truncated, summary.is_empty());
            assert!(omnis_core::needs_recovery_handoff(&snapshot, false));
            assert!(omnis_core::readback_trajectory(&snapshot).is_none());
            let mut handoff = Vec::new();
            omnis_core::write_recovery_handoff(&snapshot, &mut handoff).unwrap();
            let history = String::from_utf8(handoff).unwrap();
            assert!(history.contains("recover earlier decision"));
            assert!(!history.contains("discarded sibling"));
        }
    }

    #[test]
    fn omp_keeps_the_clear_marker_before_a_later_compaction_and_archives_older_summaries() {
        let header = json!({"type":"session","version":3,"id":"session","cwd":"/workspace","timestamp":"2026-01-01T00:00:00Z"});
        let message = |id: &str, parent: Option<&str>| {
            json!({
                "type":"message","id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:00Z",
                "message":{"role":"user","content":id}
            })
        };
        let compact = |id: &str, parent: &str, kept: &str| {
            json!({
                "type":"compaction","id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:00Z",
                "summary":id,"tokensBefore":10,"firstKeptEntryId":kept
            })
        };
        let records = [
            json!({"type":"title","v":1,"title":"Archived title","pad":"","source":"user","updatedAt":"2026-01-01T00:00:00Z"}),
            header,
            message("old request", None),
            compact("pre-clear summary", "old request", "old request"),
            json!({"type":"reset_boundary","id":"clear","parentId":"pre-clear summary","timestamp":"2026-01-01T00:00:00Z"}),
            message("first retained", Some("clear")),
            compact("intermediate summary", "first retained", "first retained"),
            message("second retained", Some("intermediate summary")),
            compact("latest summary", "second retained", "old request"),
            json!({"type":"title_change","id":"rename","parentId":"latest summary","timestamp":"2026-01-01T00:00:00Z","title":"Current title","source":"user"}),
            message("final turn", Some("rename")),
        ];
        let temporary = tempfile::tempdir().unwrap();
        let path = write_fixture(
            temporary.path(),
            "session.jsonl",
            &serialized_records(&records),
        );
        let session = SessionRef::new(Provider::OhMyPi, "session");
        let adapter = PiAdapter::oh_my_pi_with_root(temporary.path());
        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);
        let preview = adapter.preview_session_at(&session, Some(&path)).unwrap();
        let reset_and_titles = |snapshot: &omnis_ir::CanonicalSnapshot| {
            let marker = snapshot
                .events
                .iter()
                .position(|event| event.source.raw_record_type.as_deref() == Some("reset_boundary"))
                .unwrap();
            assert!(
                snapshot.events[..marker]
                    .iter()
                    .any(|event| event.payload["text"] == "old request")
            );
            let after = &snapshot.events[marker + 1..];
            assert_eq!(after[0].payload["summary"], "latest summary");
            assert!(
                after
                    .iter()
                    .any(
                        |event| event.source.raw_record_type.as_deref() == Some("title_change")
                            && event.payload["title"] == "Current title"
                    )
            );
            assert!(
                after
                    .iter()
                    .any(|event| event.payload["summary"] == "intermediate summary"
                        && event.replay_policy == ReplayPolicy::HistoricalOnly)
            );
        };
        reset_and_titles(&snapshot);
        reset_and_titles(&preview);
    }

    #[test]
    fn preview_tolerates_truncated_parent_chain() {
        let records = vec![
            json!({"type": "session", "version": 3, "id": "session", "cwd": "/workspace"}),
            json!({
                "type": "message", "id": "leaf", "parentId": "missing",
                "message": {"role": "user", "content": "tail", "timestamp": 1}
            }),
        ];
        let snapshot = snapshot_from_records_preview(
            &SessionRef::new(Provider::Pi, "session"),
            &records,
            records.len(),
        )
        .expect("partial Pi preview");
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].kind, EventKind::MessageUser);
    }

    #[test]
    fn omp_preview_clears_a_title_only_when_the_tail_confirms_the_active_reset() {
        let root = tempfile::tempdir().unwrap();
        let adapter = PiAdapter::oh_my_pi_with_root(root.path());
        let session = SessionRef::new(Provider::OhMyPi, "current");
        let records = [
            json!({"type":"session","version":3,"id":"current","cwd":"/workspace"}),
            json!({"type":"message","id":"old","message":{"role":"user","content":"Archived request"}}),
            json!({"type":"reset_boundary","id":"clear","parentId":"old"}),
            json!({"type":"message","id":"reply","parentId":"clear","message":{"role":"assistant","content":[{"type":"text","text":"Ready"}]}}),
        ];
        let mut source = serialized_records(&records);
        let path = write_fixture(root.path(), "current.jsonl", &source);
        let read = || adapter.preview_session_at(&session, Some(&path)).unwrap();
        let snapshot = read();
        assert!(omnis_core::session_search_title_is_authoritative(
            &snapshot, false
        ));
        assert_eq!(
            omnis_core::session_preview(&snapshot).title,
            omnis_core::SessionPreviewTitle::AfterClear(None)
        );

        let padding =
            "x".repeat(usize::try_from(crate::support::MAX_TRANSCRIPT_LINE_SIZE).unwrap());
        let sibling = json!({"type":"message","id":"other","parentId":"old","message":{"role":"user","content":padding}});
        source.push_str(&sibling.to_string());
        fs::write(&path, source).unwrap();
        let sample = read();
        assert!(!omnis_core::session_search_title_is_authoritative(
            &sample, false
        ));
        assert_eq!(
            omnis_core::session_preview(&sample).title,
            omnis_core::SessionPreviewTitle::Unverified
        );
    }

    #[test]
    fn visible_summaries_extension_messages_and_images_reach_the_conversation() {
        let records = vec![
            json!({"type": "session", "version": 3, "id": "session", "cwd": "/workspace"}),
            json!({
                "type": "message", "id": "1", "parentId": null,
                "message": {"role": "user", "content": [{"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]}
            }),
            json!({"type": "branch_summary", "id": "2", "parentId": "1", "fromId": "9", "summary": "Explored approach A"}),
            json!({
                "type": "custom_message", "id": "3", "parentId": "2", "customType": "reminder",
                "content": [{"type": "text", "text": "Visible extension note"}], "display": true
            }),
            json!({
                "type": "custom_message", "id": "4", "parentId": "3", "customType": "hidden",
                "content": "Hidden extension context", "display": false
            }),
        ];

        let snapshot =
            snapshot_from_records(&SessionRef::new(Provider::Pi, "session"), &records, 0)
                .expect("Pi snapshot");

        let conversation = omnis_core::import_conversation(&snapshot);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| (message.role, message.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (omnis_core::HandoffRole::User, "[1 image omitted]"),
                (
                    omnis_core::HandoffRole::Assistant,
                    "[Source branch summary]\nExplored approach A"
                ),
                (
                    omnis_core::HandoffRole::User,
                    "[Source extension message: reminder]\nVisible extension note"
                ),
            ]
        );
        let rendered = serde_json::to_string(&snapshot).expect("serialize Pi snapshot");
        assert!(!rendered.contains("Hidden extension context"));
        assert!(!rendered.contains("iVBORw0KGgo"));
    }

    #[test]
    fn indexed_read_matches_oracle_for_duplicate_ids_compaction_and_malformed_lines() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace",
                "timestamp": "2026-01-01T00:00:00Z"
            }),
            json!({
                "type": "message", "id": "root", "parentId": null,
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "user", "content": "retained root"}
            }),
            json!({
                "type": "message", "id": "duplicate", "parentId": "root",
                "timestamp": "2026-01-01T00:00:02Z",
                "message": {"role": "user", "content": "superseded duplicate"}
            }),
            json!({
                "type": "message", "id": "alternate", "parentId": "root",
                "timestamp": "2026-01-01T00:00:03Z",
                "message": {"role": "user", "content": "selected branch"}
            }),
            json!({
                "type": "branch_summary", "id": "summary", "parentId": "duplicate",
                "timestamp": "2026-01-01T00:00:04Z", "summary": "abandoned summary"
            }),
            json!({
                "type": "compaction", "id": "old-compact", "parentId": "summary",
                "timestamp": "2026-01-01T00:00:05Z", "firstKeptEntryId": "duplicate"
            }),
            json!({
                "type": "message", "id": "old-tail", "parentId": "old-compact",
                "timestamp": "2026-01-01T00:00:06Z",
                "message": {"role": "user", "content": "abandoned tail"}
            }),
            json!({
                "type": "message", "id": "duplicate", "parentId": "alternate",
                "timestamp": "2026-01-01T00:00:07Z",
                "message": {"role": "user", "content": "latest duplicate"}
            }),
            json!({
                "type": "compaction", "id": "latest-compact", "parentId": "duplicate",
                "timestamp": "2026-01-01T00:00:08Z", "firstKeptEntryId": "root",
                "summary": "current compacted context"
            }),
            json!({
                "type": "message", "id": "final", "parentId": "latest-compact",
                "timestamp": "2026-01-01T00:00:09Z",
                "message": {"role": "user", "content": "latest request"}
            }),
            json!({
                "type": "title_change", "title": "late title",
                "timestamp": "2026-01-01T00:00:10Z"
            }),
        ];
        let path = write_fixture(
            &root,
            "pi.jsonl",
            &format!(
                "leading malformed line\n{}trailing malformed line\n",
                serialized_records(&records)
            ),
        );
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);

        assert_eq!(snapshot.title.as_deref(), Some("late title"));
        assert_eq!(
            snapshot.captured_at.to_rfc3339(),
            "2026-01-01T00:00:10+00:00"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter_map(|event| event.payload.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [
                "retained root",
                "selected branch",
                "latest duplicate",
                "latest request"
            ]
        );
        assert_eq!(snapshot.events[0].kind, EventKind::CompactionCreated);
    }

    #[test]
    fn sampled_omp_rename_requires_a_known_path_to_the_leaf() {
        let session = SessionRef::new(Provider::OhMyPi, "session");
        let slot = json!({"type":"title","v":1,"title":"Original"});
        let header = json!({"type":"session","version":3,"id":"session","cwd":"/test"});
        let reset = json!({"type":"reset_boundary","id":"reset","parentId":null});
        let rename = json!({"type":"title_change","id":"name","parentId":"missing older entry","title":"Active"});
        let middle = json!({"type":"model_change","id":"middle","parentId":"name"});
        let leaf = json!({"type":"message","id":"leaf","parentId":"middle","message":{"role":"assistant","content":"Reply"}});
        let original = vec![slot, header, reset, rename, middle, leaf];
        for (name, kind, parent, tail, expected) in [
            ("Active", "model_change", "name", 3, true),
            ("Active", "model_change", "name", 2, false),
            ("Active", "model_change", "sibling", 3, false),
            ("Active", "future_entry", "name", 3, false),
            ("Active", "model_change", "name", 0, false),
            (" ", "model_change", "name", 3, false),
        ] {
            let mut records = original.clone();
            records[3]["title"] = name.into();
            records[4]["type"] = kind.into();
            records[4]["parentId"] = parent.into();
            let snapshot = snapshot_from_records_preview(&session, &records, tail).unwrap();
            let title = omnis_core::session_search_title(&snapshot);
            assert_eq!(
                title.as_deref(),
                Some(if expected { "Active" } else { "Original" })
            );
            assert_eq!(
                omnis_core::session_search_title_is_authoritative(&snapshot, false),
                expected
            );
            assert_eq!(
                omnis_core::session_preview(&snapshot).title
                    == omnis_core::SessionPreviewTitle::VerifiedRename("Active".into()),
                expected
            );
        }
        let mut unknown_leaf = original;
        unknown_leaf.push(json!({"type":"future_entry_without_id"}));
        let snapshot = snapshot_from_records_preview(&session, &unknown_leaf, 4).unwrap();
        assert!(!omnis_core::session_search_title_is_authoritative(
            &snapshot, false
        ));
        assert_eq!(
            omnis_core::session_search_title(&snapshot).as_deref(),
            Some("Original")
        );
    }

    #[test]
    fn omp_title_follows_the_selected_branch_for_full_and_sampled_reads() {
        let root = tempfile::tempdir().unwrap();
        let adapter = PiAdapter::oh_my_pi_with_root(root.path());
        let session = SessionRef::new(Provider::OhMyPi, "example");
        for (slot, branch_name, expected) in [
            ("File title", None, Some("File title")),
            ("File title", Some("Active rename"), Some("Active rename")),
            ("", None, None),
        ] {
            let mut rows = vec![
                json!({"type":"title","v":1,"title":slot}),
                json!({"type":"session","id":"example","version":3,"cwd":"/test"}),
                json!({"type":"reset_boundary","id":"clear","parentId":null}),
            ];
            if let Some(name) = branch_name {
                rows.push(
                    json!({"type":"title_change","id":"name","parentId":"clear","title":name}),
                );
            }
            rows.push(json!({"type":"title_change","id":"sibling","parentId":null,"title":"Other branch"}));
            rows.push(json!({"type":"message","id":"leaf","parentId":if branch_name.is_some() {"name"} else {"clear"},
                "message":{"role":"assistant","content":"Current reply"}}));
            let path = write_fixture(root.path(), "example.jsonl", &serialized_records(&rows));
            for snapshot in [
                adapter.read_session_at(&session, Some(&path)).unwrap(),
                adapter.preview_session_at(&session, Some(&path)).unwrap(),
            ] {
                assert_eq!(snapshot.title.as_deref(), expected);
                assert_eq!(
                    omnis_core::session_search_title(&snapshot).as_deref(),
                    expected
                );
            }
        }
    }

    #[test]
    fn indexed_read_keeps_omp_title_slot_out_of_the_entry_tree() {
        let temporary = tempfile::tempdir().expect("temporary OMP root");
        let root = temporary.path().join("sessions");
        let adapter = PiAdapter::oh_my_pi_with_root(&root);
        let title = json!({
            "type": "title", "id": "title-entry", "title": "OMP title slot",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let header = json!({
            "type": "session", "version": 3, "id": "omp-session", "cwd": "/workspace",
            "timestamp": "2026-01-01T00:00:01Z"
        });
        let records = vec![title.clone(), header.clone()];
        let path = write_fixture(&root, "title-only.jsonl", &serialized_records(&records));
        let session = SessionRef::new(Provider::OhMyPi, "omp-session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 0);

        assert_eq!(snapshot.title.as_deref(), Some("OMP title slot"));
        assert!(snapshot.events.is_empty());

        let child = json!({
            "type": "message", "id": "child", "parentId": "title-entry",
            "timestamp": "2026-01-01T00:00:02Z",
            "message": {"role": "user", "content": "synthetic child"}
        });
        let child_records = vec![title, header, child];
        let child_path = write_fixture(
            &root,
            "title-parent.jsonl",
            &serialized_records(&child_records),
        );
        let actual_error = adapter
            .read_session_at(&session, Some(&child_path))
            .expect_err("title slot is not part of the entry tree");
        let expected_error = snapshot_from_records(&session, &child_records, 0)
            .expect_err("oracle excludes the first title slot");
        assert_eq!(actual_error.to_string(), expected_error.to_string());
        assert_eq!(
            actual_error.to_string(),
            "Pi session tree contains a missing leaf"
        );
    }

    #[test]
    fn indexed_read_matches_oracle_with_oversized_line_and_late_metadata() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace",
                "timestamp": "2026-01-01T00:00:00Z"
            }),
            json!({
                "type": "message", "id": "user", "parentId": null,
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "user", "content": "kept request"}
            }),
            json!({
                "type": "session_info", "name": "late metadata",
                "timestamp": "2026-01-01T00:00:03Z"
            }),
        ];
        let mut document = serialized_records(&records[..2]);
        document.push_str("{\"synthetic_padding\":\"");
        document.push_str(&"x".repeat(16 * 1024 * 1024));
        document.push_str("\"}\n");
        document.push_str(&records[2].to_string());
        document.push('\n');
        let path = write_fixture(&root, "oversized.jsonl", &document);
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let snapshot = assert_matches_snapshot_oracle(&adapter, &session, &path, &records, 1);

        assert_eq!(snapshot.title.as_deref(), Some("late metadata"));
        assert_eq!(
            snapshot.captured_at.to_rfc3339(),
            "2026-01-01T00:00:03+00:00"
        );
        let omission_notice = snapshot
            .events
            .iter()
            .find(|event| {
                event.source.raw_record_type.as_deref() == Some("omnisession.record_size_limit")
            })
            .expect("oversized line notice");
        assert_eq!(omission_notice.payload["omitted_records"], 1);
    }

    #[test]
    fn indexed_read_preserves_strict_missing_parent_error() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let root = temporary.path().join("sessions");
        let records = vec![
            json!({
                "type": "session", "version": 3, "id": "session", "cwd": "/workspace"
            }),
            json!({
                "type": "message", "id": "leaf", "parentId": "missing",
                "message": {"role": "user", "content": "synthetic"}
            }),
        ];
        let path = write_fixture(&root, "missing-parent.jsonl", &serialized_records(&records));
        let adapter = PiAdapter::with_root(&root);
        let session = SessionRef::new(Provider::Pi, "session");

        let actual = adapter
            .read_session_at(&session, Some(&path))
            .expect_err("strict Pi read rejects a missing parent");
        let expected = snapshot_from_records(&session, &records, 0)
            .expect_err("snapshot oracle rejects a missing parent");
        assert_eq!(actual.to_string(), expected.to_string());
        let invalid = actual.downcast_ref::<InvalidPiTree>().unwrap();
        assert_eq!(invalid.source.session, session);
        assert_eq!(
            invalid.source.source_revision,
            crate::source_file_revision(&path)
        );

        let cycle_records = vec![
            json!({
                "type": "session", "version": 3, "id": "cycle", "cwd": "/workspace"
            }),
            json!({"type": "message", "id": "a", "parentId": "b"}),
            json!({"type": "message", "id": "b", "parentId": "a"}),
        ];
        let cycle_path = write_fixture(&root, "cycle.jsonl", &serialized_records(&cycle_records));
        let cycle_session = SessionRef::new(Provider::Pi, "cycle");
        let actual_cycle = adapter
            .read_session_at(&cycle_session, Some(&cycle_path))
            .expect_err("strict Pi read rejects a parent cycle");
        let expected_cycle = snapshot_from_records(&cycle_session, &cycle_records, 0)
            .expect_err("snapshot oracle rejects a parent cycle");
        assert_eq!(actual_cycle.to_string(), expected_cycle.to_string());
        assert!(actual_cycle.is::<InvalidPiTree>());
        for (suffix_len, is_limited) in [(1, true), (2, false)] {
            let error = snapshot_from_records_preview(&cycle_session, &cycle_records, suffix_len)
                .expect_err("sampled cycle cannot produce a conversation");
            assert_eq!(error.to_string(), expected_cycle.to_string());
            assert_eq!(error.is::<crate::TranscriptReadLimit>(), is_limited);
        }
    }

    #[cfg(unix)]
    #[test]
    fn selected_file_detects_in_place_edits_and_replacements() {
        let temporary = tempfile::tempdir().expect("temporary Pi root");
        let original = json!({"type":"session","version":3,"id":"one","cwd":"/old"}).to_string();
        let path = write_fixture(temporary.path(), "session.jsonl", &original);
        let adapter = PiAdapter::with_root(temporary.path());
        let session = SessionRef::new(Provider::Pi, "one");
        let before = adapter
            .select_file(&session.id, Some(&path))
            .expect("select Pi session");
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(&path, original.replace("/old", "/new")).expect("rewrite same-length header");
        assert!(
            before
                .read(&session, None)
                .map(|read| read.snapshot)
                .expect_err("strict read paired with an old header")
                .to_string()
                .contains("changed during")
        );
        let before = adapter
            .select_file(&session.id, Some(&path))
            .expect("select updated Pi session");
        let previous_revision = before.observed.revision;
        let modified = before
            .reader
            .get_ref()
            .metadata()
            .unwrap()
            .modified()
            .unwrap();
        let replacement = path.with_extension("tmp");
        fs::write(&replacement, original).expect("write replacement");
        fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        fs::rename(replacement, &path).expect("replace selected path");
        assert!(
            before
                .read(&session, Some(0))
                .map(|read| read.snapshot)
                .expect_err("preview of a replaced path")
                .to_string()
                .contains("changed during")
        );
        let current = adapter.read_full_index_at(&session, Some(&path)).unwrap();
        assert_ne!(current.source_revision.file, previous_revision.file);
        assert_ne!(current.source_revision.contents, previous_revision.contents);
    }

    #[test]
    fn omp_index_pairs_the_selected_header_with_the_branch_title() {
        let temporary = tempfile::tempdir().expect("temporary OMP root");
        let records = [
            json!({"type":"title","title":"Provider slot"}),
            json!({"type":"session","version":3,"id":"one","cwd":"/workspace"}),
            json!({"type":"reset_boundary","id":"clear","parentId":null}),
            json!({"type":"title_change","id":"rename","parentId":"clear","title":"Active title"}),
        ];
        let path = write_fixture(
            temporary.path(),
            "session.jsonl",
            &serialized_records(&records),
        );
        let adapter = PiAdapter::oh_my_pi_with_root(temporary.path());
        let session = SessionRef::new(Provider::OhMyPi, "one");
        let listed = adapter
            .list_sessions(None)
            .expect("list")
            .pop()
            .expect("listed");
        assert_eq!(listed.title.as_deref(), Some("Provider slot"));
        assert!(
            listed
                .source_revision
                .and_then(|revision| revision.contents)
                .is_some()
        );
        for budget in [0, u64::MAX] {
            let read = adapter
                .index_session(&session, Some(&path), budget)
                .expect("read");
            assert_eq!(read.native_session.as_ref(), Some(&listed));
            assert_eq!(Some(read.source_revision), listed.source_revision);
            assert_eq!(read.source_complete, budget != 0);
            assert!(!read.provider_limited);
            assert_eq!(
                omnis_core::session_search_title(&read.snapshot).as_deref(),
                Some("Active title")
            );
        }
    }
}
