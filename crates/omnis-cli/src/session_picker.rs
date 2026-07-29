use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, IsTerminal, Write},
    path::{MAIN_SEPARATOR, Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use directories::BaseDirs;
use omnis_adapters::{AdapterRegistry, NativeSession};
use omnis_core::{
    HandoffMessage, HandoffRole, SessionPreview, first_user_message_after, safe_terminal_line,
    session_preview, trajectory_search_document,
};
use omnis_ir::{Provider, SessionRef};
use omnis_store::{HandoffRecord, IndexedSession, Store};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::PROVIDERS;

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(75);
const PREVIEW_CACHE_CAPACITY: usize = 128;
const SEARCH_INDEX_DEBOUNCE: Duration = Duration::from_millis(120);
const TRAJECTORY_SEARCH_LIMIT: usize = 100_000;
const SESSION_CACHE_TTL: Duration = Duration::from_secs(15);
const LINEAGE_PREVIEW_LIMIT: usize = 12;

pub struct PickerSelection {
    pub session: SessionRef,
    pub project_path: Option<PathBuf>,
    pub across_projects: bool,
    pub target: Provider,
    pub fork: bool,
    pub workspace_override: Option<PathBuf>,
}

struct PickerEntry {
    key: String,
    session: NativeSession,
    current_workspace: bool,
    search: String,
    cached: bool,
}

struct PickerState {
    entries: Vec<PickerEntry>,
    entry_positions: HashMap<String, usize>,
    search_index: Option<InvertedIndex>,
    entries_generation: u64,
    search_index_deadline: Option<Instant>,
    query: String,
    trajectory_matches: HashSet<String>,
    trajectory_search_generation: u64,
    trajectory_search_deadline: Option<Instant>,
    trajectory_search_pending: bool,
    provider_index: usize,
    all_projects: bool,
    selected: usize,
    current_git_branch: Option<String>,
    lineage: LineageGraph,
    previews: PreviewCache,
    preview_window: Vec<PreviewKey>,
    preview_deadline: Option<Instant>,
}

impl PickerState {
    fn new(
        sessions: Vec<NativeSession>,
        current_project: &Path,
        initial_provider: Option<Provider>,
        all_projects: bool,
    ) -> Self {
        let entries = picker_entries(sessions, current_project, true);
        let provider_index = initial_provider
            .and_then(|provider| {
                PROVIDERS
                    .iter()
                    .position(|candidate| *candidate == provider)
            })
            .map_or(0, |index| index + 1);
        let entry_positions = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.key.clone(), index))
            .collect();
        Self {
            entries,
            entry_positions,
            search_index: None,
            entries_generation: 0,
            search_index_deadline: None,
            query: String::new(),
            trajectory_matches: HashSet::new(),
            trajectory_search_generation: 0,
            trajectory_search_deadline: None,
            trajectory_search_pending: false,
            provider_index,
            all_projects,
            selected: 0,
            current_git_branch: workspace_git_branch(current_project),
            lineage: LineageGraph::default(),
            previews: PreviewCache::default(),
            preview_window: Vec::new(),
            preview_deadline: None,
        }
    }

    fn rebuild_entry_positions(&mut self) {
        self.entry_positions = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.key.clone(), index))
            .collect();
    }

    fn provider(&self) -> Option<Provider> {
        self.provider_index
            .checked_sub(1)
            .and_then(|index| PROVIDERS.get(index).copied())
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.lineage.grouped_indices(
            self.matching_indices(),
            &self.entries,
            &self.entry_positions,
        )
    }

    fn matching_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let mut candidates = self
            .search_index
            .as_ref()
            .and_then(|index| index.candidates(&query))
            .unwrap_or_else(|| (0..self.entries.len()).collect());
        if !query.is_empty() {
            candidates.extend(
                self.trajectory_matches
                    .iter()
                    .filter_map(|key| self.entry_positions.get(key).copied()),
            );
            candidates.sort_unstable();
            candidates.dedup();
        }
        candidates
            .into_iter()
            .filter(|index| self.all_projects || self.entries[*index].current_workspace)
            .filter(|index| {
                self.provider().is_none_or(|provider| {
                    self.entries[*index].session.session.provider == provider
                })
            })
            .filter(|index| {
                query.is_empty()
                    || self.entries[*index].search.contains(&query)
                    || self.trajectory_matches.contains(&self.entries[*index].key)
            })
            .collect()
    }

    fn replace_lineage(&mut self, records: Vec<HandoffRecord>) {
        let selected_key = self.selected_entry().map(|entry| entry.key.clone());
        self.lineage.replace(records);
        self.restore_selection(selected_key);
    }

    #[cfg(test)]
    fn replace_provider(
        &mut self,
        provider: Provider,
        sessions: Vec<NativeSession>,
        current_project: &Path,
        cached: bool,
    ) {
        self.replace_provider_entries(provider, picker_entries(sessions, current_project, cached));
    }

    fn replace_provider_entries(&mut self, provider: Provider, entries: Vec<PickerEntry>) {
        let selected_key = self.selected_entry().map(|entry| entry.key.clone());
        self.entries
            .retain(|entry| entry.session.session.provider != provider);
        self.entries.extend(entries);
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.session.updated_at));
        self.rebuild_entry_positions();
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.search_index = None;
        self.search_index_deadline = Some(Instant::now() + SEARCH_INDEX_DEBOUNCE);
        self.restore_selection(selected_key);
    }

    fn replace_all_entries(&mut self, mut entries: Vec<PickerEntry>) {
        let selected_key = self.selected_entry().map(|entry| entry.key.clone());
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.session.updated_at));
        self.entries = entries;
        self.rebuild_entry_positions();
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.search_index = None;
        self.search_index_deadline = Some(Instant::now() + SEARCH_INDEX_DEBOUNCE);
        self.restore_selection(selected_key);
    }

    fn restore_selection(&mut self, selected_key: Option<String>) {
        let visible = self.visible_indices();
        self.selected = selected_key
            .and_then(|key| {
                visible
                    .iter()
                    .position(|index| self.entries[*index].key == key)
            })
            .unwrap_or_else(|| self.selected.min(visible.len().saturating_sub(1)));
    }

    fn replace_trajectory_matches(&mut self, matches: Vec<SessionRef>) {
        let selected_key = self.selected_entry().map(|entry| entry.key.clone());
        self.trajectory_matches = matches
            .into_iter()
            .map(|session| session.to_string())
            .collect();
        self.restore_selection(selected_key);
    }

    fn due_search_index_request(&mut self) -> Option<SearchIndexRequest> {
        let deadline = self.search_index_deadline?;
        if Instant::now() < deadline {
            return None;
        }
        self.search_index_deadline = None;
        Some(SearchIndexRequest {
            generation: self.entries_generation,
            values: self
                .entries
                .iter()
                .map(|entry| entry.search.clone())
                .collect(),
        })
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
    }

    fn query_changed(&mut self) {
        self.trajectory_matches.clear();
        self.trajectory_search_pending = false;
        self.trajectory_search_generation = self.trajectory_search_generation.wrapping_add(1);
        self.trajectory_search_deadline =
            (!self.query.trim().is_empty()).then(|| Instant::now() + SEARCH_INDEX_DEBOUNCE);
    }

    fn trajectory_index_changed(&mut self) {
        if self.query.trim().is_empty() {
            return;
        }
        self.trajectory_search_generation = self.trajectory_search_generation.wrapping_add(1);
        self.trajectory_search_deadline = Some(Instant::now() + SEARCH_INDEX_DEBOUNCE);
    }

    fn due_trajectory_search_request(&mut self) -> Option<TrajectorySearchRequest> {
        let deadline = self.trajectory_search_deadline?;
        if Instant::now() < deadline {
            return None;
        }
        self.trajectory_search_deadline = None;
        self.trajectory_search_pending = true;
        Some(TrajectorySearchRequest {
            generation: self.trajectory_search_generation,
            query: self.query.clone(),
        })
    }

    fn cycle_provider(&mut self, backwards: bool) {
        let count = PROVIDERS.len() + 1;
        self.provider_index = if backwards {
            (self.provider_index + count - 1) % count
        } else {
            (self.provider_index + 1) % count
        };
        self.reset_selection();
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.visible_indices().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        if delta < 0 {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = self
                .selected
                .saturating_add(delta.unsigned_abs())
                .min(count - 1);
        }
    }

    fn selected_entry(&self) -> Option<&PickerEntry> {
        let visible = self.visible_indices();
        visible
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
    }

    fn preview_key(&self, session: &NativeSession) -> PreviewKey {
        PreviewKey {
            session: session.session.clone(),
            updated_at: session.updated_at,
            continuation_after: self.lineage.continuation_after(&session.session),
        }
    }

    fn refresh_preview_window(&mut self, row_count: usize) {
        let visible = self.visible_indices();
        let (first, selected) = centered_list_window(visible.len(), self.selected, row_count);
        let mut window = visible
            .iter()
            .skip(first)
            .take(row_count)
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| self.preview_key(&entry.session))
            .collect::<Vec<_>>();
        if let Some(selected_key) = window.get(selected.saturating_sub(first)).cloned() {
            window.retain(|key| key != &selected_key);
            window.insert(0, selected_key);
        }
        if let Some(selected) = self.selected_entry() {
            for node in self
                .lineage
                .tree(&selected.session.session)
                .into_iter()
                .take(LINEAGE_PREVIEW_LIMIT)
            {
                let key = node.session.to_string();
                let Some(entry) = self
                    .entry_positions
                    .get(&key)
                    .and_then(|index| self.entries.get(*index))
                else {
                    continue;
                };
                let preview_key = self.preview_key(&entry.session);
                if !window.contains(&preview_key) {
                    window.push(preview_key);
                }
            }
        }
        if self.preview_window == window {
            return;
        }
        self.preview_window = window;
        self.preview_deadline = self
            .preview_window
            .iter()
            .any(|key| !self.previews.contains(key))
            .then(|| Instant::now() + PREVIEW_DEBOUNCE);
    }

    fn due_preview_request(&mut self) -> Option<Vec<PreviewKey>> {
        let deadline = self.preview_deadline?;
        if Instant::now() < deadline {
            return None;
        }
        self.preview_deadline = None;
        let missing = self
            .preview_window
            .iter()
            .filter(|key| !self.previews.contains(key))
            .cloned()
            .collect::<Vec<_>>();
        (!missing.is_empty()).then_some(missing)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PreviewKey {
    session: SessionRef,
    updated_at: Option<DateTime<Utc>>,
    continuation_after: Option<DateTime<Utc>>,
}

enum PreviewValue {
    Ready {
        preview: Box<SessionPreview>,
        continuation: Option<HandoffMessage>,
    },
    Unavailable,
}

#[derive(Default)]
struct PreviewCache {
    values: HashMap<PreviewKey, PreviewValue>,
    order: VecDeque<PreviewKey>,
}

impl PreviewCache {
    fn contains(&self, key: &PreviewKey) -> bool {
        self.values.contains_key(key)
    }

    fn get(&self, key: &PreviewKey) -> Option<&PreviewValue> {
        self.values.get(key)
    }

    fn insert(&mut self, key: PreviewKey, value: PreviewValue, protected: &[PreviewKey]) {
        if self.values.contains_key(&key) {
            self.order.retain(|candidate| candidate != &key);
        }
        self.order.push_back(key.clone());
        self.values.insert(key, value);
        while self.order.len() > PREVIEW_CACHE_CAPACITY {
            let Some(index) = self
                .order
                .iter()
                .position(|candidate| !protected.contains(candidate))
            else {
                break;
            };
            let expired = self.order.remove(index).expect("preview cache entry");
            self.values.remove(&expired);
        }
    }
}

#[derive(Default)]
struct LineageGraph {
    parents: HashMap<SessionRef, SessionRef>,
    children: HashMap<SessionRef, Vec<SessionRef>>,
    edge_order: HashMap<SessionRef, DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LineageTreeNode {
    session: SessionRef,
    branch: String,
    selected: bool,
}

impl LineageGraph {
    fn replace(&mut self, records: Vec<HandoffRecord>) {
        self.parents.clear();
        self.children.clear();
        self.edge_order.clear();
        for record in records {
            self.edge_order
                .insert(record.target.clone(), record.created_at);
            self.parents.insert(record.target, record.source);
        }
        for (target, source) in &self.parents {
            self.children
                .entry(source.clone())
                .or_default()
                .push(target.clone());
        }
        for children in self.children.values_mut() {
            children.sort_by(|left, right| {
                self.edge_order
                    .get(left)
                    .cmp(&self.edge_order.get(right))
                    .then_with(|| left.to_string().cmp(&right.to_string()))
            });
            children.dedup();
        }
    }

    fn grouped_indices(
        &self,
        candidates: Vec<usize>,
        entries: &[PickerEntry],
        positions: &HashMap<String, usize>,
    ) -> Vec<usize> {
        let candidate_set = candidates.iter().copied().collect::<HashSet<_>>();
        let mut grouped = Vec::with_capacity(candidates.len());
        let mut emitted = HashSet::with_capacity(candidates.len());
        for anchor in candidates {
            if emitted.contains(&anchor) {
                continue;
            }
            let tree = self.tree(&entries[anchor].session.session);
            for index in tree.into_iter().filter_map(|node| {
                positions
                    .get(&node.session.to_string())
                    .copied()
                    .filter(|index| candidate_set.contains(index))
            }) {
                if emitted.insert(index) {
                    grouped.push(index);
                }
            }
            if emitted.insert(anchor) {
                grouped.push(anchor);
            }
        }
        grouped
    }

    fn list_prefix(&self, session: &SessionRef) -> String {
        self.tree(session)
            .into_iter()
            .find(|node| node.session == *session)
            .map_or_else(String::new, |node| {
                if node.branch.is_empty() {
                    "┬─ ".to_owned()
                } else {
                    node.branch
                }
            })
    }

    fn continuation_after(&self, session: &SessionRef) -> Option<DateTime<Utc>> {
        self.edge_order.get(session).copied()
    }

    fn tree(&self, selected: &SessionRef) -> Vec<LineageTreeNode> {
        if !self.parents.contains_key(selected) && !self.children.contains_key(selected) {
            return Vec::new();
        }

        let component = self.component(selected);
        let mut roots = component
            .iter()
            .filter(|session| {
                self.parents
                    .get(*session)
                    .is_none_or(|parent| !component.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        roots.sort_by_key(ToString::to_string);
        if roots.is_empty() {
            roots.push(selected.clone());
        }

        let mut nodes = Vec::with_capacity(component.len());
        let mut visited = HashSet::new();
        for root in roots {
            self.append_tree(&root, selected, &mut visited, String::new(), "", &mut nodes);
        }
        nodes
    }

    fn component(&self, selected: &SessionRef) -> HashSet<SessionRef> {
        let mut component = HashSet::new();
        let mut pending = VecDeque::from([selected.clone()]);
        while let Some(session) = pending.pop_front() {
            if !component.insert(session.clone()) {
                continue;
            }
            if let Some(parent) = self.parents.get(&session) {
                pending.push_back(parent.clone());
            }
            if let Some(children) = self.children.get(&session) {
                pending.extend(children.iter().cloned());
            }
        }
        component
    }

    fn append_tree(
        &self,
        session: &SessionRef,
        selected: &SessionRef,
        visited: &mut HashSet<SessionRef>,
        prefix: String,
        connector: &str,
        nodes: &mut Vec<LineageTreeNode>,
    ) {
        if !visited.insert(session.clone()) {
            return;
        }
        nodes.push(LineageTreeNode {
            session: session.clone(),
            branch: format!("{prefix}{connector}"),
            selected: session == selected,
        });

        let children = self
            .children
            .get(session)
            .into_iter()
            .flatten()
            .filter(|child| !visited.contains(*child))
            .cloned()
            .collect::<Vec<_>>();
        let child_prefix = if connector == "├─ " {
            format!("{prefix}│  ")
        } else if connector.is_empty() {
            prefix
        } else {
            format!("{prefix}   ")
        };
        for (index, child) in children.iter().enumerate() {
            let child_connector = if index + 1 == children.len() {
                "└─ "
            } else {
                "├─ "
            };
            self.append_tree(
                child,
                selected,
                visited,
                child_prefix.clone(),
                child_connector,
                nodes,
            );
        }
    }
}

fn picker_entries(
    sessions: Vec<NativeSession>,
    current_project: &Path,
    cached: bool,
) -> Vec<PickerEntry> {
    let mut matcher = WorkspaceMatcher::new(current_project);
    sessions
        .into_iter()
        .map(|session| picker_entry(session, &mut matcher, cached))
        .collect()
}

fn picker_entry(
    session: NativeSession,
    matcher: &mut WorkspaceMatcher,
    cached: bool,
) -> PickerEntry {
    PickerEntry {
        key: session.session.to_string(),
        current_workspace: session
            .project_path
            .as_deref()
            .is_some_and(|path| matcher.matches(path)),
        search: search_text(&session),
        session,
        cached,
    }
}

struct WorkspaceMatcher {
    current: PathBuf,
    canonical_current: Option<PathBuf>,
    matches: HashMap<PathBuf, bool>,
}

impl WorkspaceMatcher {
    fn new(current: &Path) -> Self {
        Self {
            current: current.to_path_buf(),
            canonical_current: current.canonicalize().ok(),
            matches: HashMap::new(),
        }
    }

    fn matches(&mut self, candidate: &Path) -> bool {
        if candidate == self.current {
            return true;
        }
        if let Some(matches) = self.matches.get(candidate) {
            return *matches;
        }
        let matches = self
            .canonical_current
            .as_ref()
            .is_some_and(|current| candidate.canonicalize().is_ok_and(|path| path == *current));
        self.matches.insert(candidate.to_path_buf(), matches);
        matches
    }
}

fn workspace_git_branch(workspace: &Path) -> Option<String> {
    let marker = workspace
        .ancestors()
        .map(|path| path.join(".git"))
        .find(|path| path.exists())?;
    let git_dir = if marker.is_dir() {
        marker
    } else {
        let pointer = fs::read_to_string(&marker).ok()?;
        let path = pointer.trim().strip_prefix("gitdir: ")?;
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            marker.parent()?.join(path)
        }
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(safe_terminal_line(branch));
    }
    (!head.is_empty()).then(|| format!("detached @ {}", short_id(head)))
}

#[derive(Default)]
struct InvertedIndex {
    postings: HashMap<u64, Vec<usize>>,
}

impl InvertedIndex {
    fn build(values: &[String]) -> Self {
        let mut postings: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, value) in values.iter().enumerate() {
            for trigram in trigrams(value) {
                postings.entry(trigram).or_default().push(index);
            }
        }
        Self { postings }
    }

    fn candidates(&self, query: &str) -> Option<Vec<usize>> {
        let mut query_trigrams = trigrams(query).into_iter().collect::<Vec<_>>();
        if query_trigrams.is_empty() {
            return None;
        }
        query_trigrams
            .sort_by_key(|trigram| self.postings.get(trigram).map_or(usize::MAX, Vec::len));
        let Some(first) = self.postings.get(&query_trigrams[0]) else {
            return Some(Vec::new());
        };
        let mut matches = first.clone();
        for trigram in &query_trigrams[1..] {
            let Some(posting) = self.postings.get(trigram) else {
                return Some(Vec::new());
            };
            matches.retain(|index| posting.binary_search(index).is_ok());
            if matches.is_empty() {
                break;
            }
        }
        Some(matches)
    }
}

struct SearchIndexRequest {
    generation: u64,
    values: Vec<String>,
}

struct TrajectorySearchRequest {
    generation: u64,
    query: String,
}

fn trigrams(value: &str) -> HashSet<u64> {
    let mut characters = value.chars();
    let (Some(mut first), Some(mut second)) = (characters.next(), characters.next()) else {
        return HashSet::new();
    };
    let mut trigrams = HashSet::new();
    for third in characters {
        trigrams.insert(trigram_hash(first, second, third));
        first = second;
        second = third;
    }
    trigrams
}

fn trigram_hash(first: char, second: char, third: char) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    [first, second, third]
        .into_iter()
        .flat_map(|character| u32::from(character).to_le_bytes())
        .fold(OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
}

pub fn pick_session(
    current_project: &Path,
    target: Option<Provider>,
    available_targets: &[Provider],
    initial_provider: Option<Provider>,
    all_projects: bool,
    force_cross_provider: bool,
) -> Result<Option<PickerSelection>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "SOURCE is required without an interactive terminal; run `omnis list` or pass `provider:id`"
        );
    }

    let _terminal = TerminalGuard::enter()?;
    let workers = spawn_updates(current_project);
    let mut pending = HashSet::new();
    let mut warnings = Vec::new();
    let mut state = PickerState::new(Vec::new(), current_project, initial_provider, all_projects);
    let mut render_state = PickerRenderState::default();
    render_state.render(&state, target, warnings.len(), pending.len())?;

    let mut dirty = false;
    loop {
        dirty |= receive_updates(
            &workers.receiver,
            &mut state,
            &mut pending,
            &mut warnings,
            current_project,
        );
        state.refresh_preview_window(terminal_list_row_count()?);
        dirty |= dispatch_background_requests(&mut state, &workers);
        if dirty {
            render_state.render(&state, target, warnings.len(), pending.len())?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(75)).context("polling session picker input")? {
            continue;
        }
        let key = match event::read().context("reading session picker input")? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                dirty = true;
                continue;
            }
            _ => continue,
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match handle_key(&mut state, key) {
            PickerAction::Continue => dirty = true,
            PickerAction::Cancel => return Ok(None),
            PickerAction::Select => {
                let Some(entry) = state.selected_entry() else {
                    continue;
                };
                let session = entry.session.session.clone();
                let project_path = entry.session.project_path.clone();
                let across_projects = state.all_projects;
                let workspace_override = if project_path.as_deref().is_some_and(Path::is_dir) {
                    None
                } else {
                    match pick_workspace(&session, project_path.as_deref(), current_project)? {
                        WorkspaceOutcome::Selected(path) => Some(path),
                        WorkspaceOutcome::Back => {
                            dirty = true;
                            continue;
                        }
                        WorkspaceOutcome::Cancel => return Ok(None),
                    }
                };
                let selected_target = target.map_or_else(
                    || {
                        let targets = available_targets
                            .iter()
                            .copied()
                            .filter(|provider| {
                                !force_cross_provider || *provider != session.provider
                            })
                            .collect::<Vec<_>>();
                        pick_target(&session, &targets)
                    },
                    |target| {
                        Ok(TargetOutcome::Selected(TargetChoice {
                            provider: target,
                            fork: false,
                        }))
                    },
                )?;
                let choice = match selected_target {
                    TargetOutcome::Selected(choice) => choice,
                    TargetOutcome::Back => {
                        dirty = true;
                        continue;
                    }
                    TargetOutcome::Cancel => return Ok(None),
                };
                return Ok(Some(PickerSelection {
                    session,
                    project_path,
                    across_projects,
                    target: choice.provider,
                    fork: choice.fork,
                    workspace_override,
                }));
            }
        }
    }
}

fn dispatch_background_requests(state: &mut PickerState, workers: &PickerWorkers) -> bool {
    let mut changed = false;
    if let Some(request) = state.due_preview_request() {
        let _ = workers.preview_sender.send(request);
        changed = true;
    }
    if let Some(request) = state.due_search_index_request() {
        if matches!(
            workers.search_sender.try_send(request),
            Err(TrySendError::Full(_))
        ) {
            state.search_index_deadline = Some(Instant::now() + SEARCH_INDEX_DEBOUNCE);
        }
    }
    if let Some(request) = state.due_trajectory_search_request() {
        changed = true;
        if matches!(
            workers.trajectory_search_sender.try_send(request),
            Err(TrySendError::Full(_))
        ) {
            state.trajectory_search_pending = false;
            state.trajectory_search_deadline = Some(Instant::now() + SEARCH_INDEX_DEBOUNCE);
        }
    }
    changed
}

struct DiscoveryUpdate {
    provider: Provider,
    result: anyhow::Result<Vec<PickerEntry>>,
}

enum PickerUpdate {
    Cached(Result<Vec<PickerEntry>, String>),
    Lineage(Result<Vec<HandoffRecord>, String>),
    Preview {
        key: PreviewKey,
        value: PreviewValue,
    },
    SearchIndex {
        generation: u64,
        index: InvertedIndex,
    },
    TrajectorySearch {
        generation: u64,
        result: Result<Vec<SessionRef>, String>,
    },
    Discovered(DiscoveryUpdate),
    RefreshStarted(Provider),
    Warning(String),
}

struct PickerWorkers {
    receiver: Receiver<PickerUpdate>,
    preview_sender: Sender<Vec<PreviewKey>>,
    search_sender: SyncSender<SearchIndexRequest>,
    trajectory_search_sender: SyncSender<TrajectorySearchRequest>,
}

fn spawn_updates(current_project: &Path) -> PickerWorkers {
    let (sender, receiver) = mpsc::channel();
    let (preview_sender, preview_receiver) = mpsc::channel::<Vec<PreviewKey>>();
    let (search_sender, search_receiver) = mpsc::sync_channel::<SearchIndexRequest>(1);
    let (trajectory_search_sender, trajectory_search_receiver) =
        mpsc::sync_channel::<TrajectorySearchRequest>(1);
    let (index_sender, index_receiver) = mpsc::channel::<(Provider, Vec<IndexedSession>)>();

    spawn_index_writer(sender.clone(), index_receiver);
    spawn_cache_updates(sender.clone(), index_sender, current_project.to_path_buf());
    spawn_preview_updates(sender.clone(), preview_receiver);
    spawn_search_updates(sender.clone(), search_receiver);
    spawn_trajectory_search_updates(sender, trajectory_search_receiver);

    PickerWorkers {
        receiver,
        preview_sender,
        search_sender,
        trajectory_search_sender,
    }
}

fn spawn_cache_updates(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<(Provider, Vec<IndexedSession>)>,
    current_project: PathBuf,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Warning(
                "session index is unavailable".to_owned(),
            ));
            spawn_all_provider_updates(&sender, &index_sender, &current_project);
            return;
        };
        let cached = store
            .indexed_sessions()
            .map(|sessions| {
                picker_entries(
                    sessions.into_iter().map(native_session).collect(),
                    &current_project,
                    true,
                )
            })
            .map_err(|error| error.to_string());
        let cache_loaded = cached.is_ok();
        if sender.send(PickerUpdate::Cached(cached)).is_err() {
            return;
        }
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        if sender.send(PickerUpdate::Lineage(lineage)).is_err() {
            return;
        }
        let freshness = PROVIDERS.map(|provider| {
            (
                provider,
                store
                    .session_index_checked_at(provider)
                    .map_err(|error| error.to_string()),
            )
        });
        for (provider, checked_at) in freshness {
            let refresh = match checked_at {
                Ok(Some(checked_at)) => !cache_loaded || !session_cache_is_fresh(checked_at),
                Ok(None) => true,
                Err(error) => {
                    if sender
                        .send(PickerUpdate::Warning(format!(
                            "{provider} session index: {error}"
                        )))
                        .is_err()
                    {
                        return;
                    }
                    true
                }
            };
            if !refresh {
                continue;
            }
            if let Err(error) = store.mark_session_index_checked(provider) {
                if sender
                    .send(PickerUpdate::Warning(format!(
                        "{provider} session index: {error}"
                    )))
                    .is_err()
                {
                    return;
                }
            }
            start_provider_update(&sender, &index_sender, &current_project, provider);
        }
    });
}

fn spawn_all_provider_updates(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<(Provider, Vec<IndexedSession>)>,
    current_project: &Path,
) {
    for provider in PROVIDERS {
        start_provider_update(sender, index_sender, current_project, provider);
    }
}

fn start_provider_update(
    sender: &Sender<PickerUpdate>,
    index_sender: &Sender<(Provider, Vec<IndexedSession>)>,
    current_project: &Path,
    provider: Provider,
) {
    if sender.send(PickerUpdate::RefreshStarted(provider)).is_ok() {
        spawn_provider_update(
            sender.clone(),
            index_sender.clone(),
            current_project.to_path_buf(),
            provider,
        );
    }
}

fn session_cache_is_fresh(checked_at: DateTime<Utc>) -> bool {
    Utc::now()
        .signed_duration_since(checked_at)
        .to_std()
        .map_or(true, |age| age <= SESSION_CACHE_TTL)
}

fn spawn_index_writer(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<(Provider, Vec<IndexedSession>)>,
) {
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = sender.send(PickerUpdate::Warning(
                "session index writer is unavailable".to_owned(),
            ));
            return;
        };
        while let Ok((provider, indexed)) = receiver.recv() {
            if let Err(error) = store.replace_indexed_sessions(provider, &indexed) {
                let _ = sender.send(PickerUpdate::Warning(format!("session index: {error}")));
            }
        }
    });
}

fn spawn_provider_update(
    sender: Sender<PickerUpdate>,
    index_sender: Sender<(Provider, Vec<IndexedSession>)>,
    current_project: PathBuf,
    provider: Provider,
) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let result = registry.list_sessions(provider, None);
        match result {
            Ok(sessions) => {
                let indexed = sessions.iter().map(indexed_session).collect::<Vec<_>>();
                let entries = picker_entries(sessions, &current_project, false);
                if sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider,
                        result: Ok(entries),
                    }))
                    .is_ok()
                {
                    let _ = index_sender.send((provider, indexed));
                }
            }
            Err(error) => {
                let _ = sender.send(PickerUpdate::Discovered(DiscoveryUpdate {
                    provider,
                    result: Err(error),
                }));
            }
        }
    });
}

fn spawn_preview_updates(sender: Sender<PickerUpdate>, receiver: Receiver<Vec<PreviewKey>>) {
    thread::spawn(move || {
        let registry = AdapterRegistry::with_local_adapters();
        let mut store = Store::open_default().ok();
        while let Ok(keys) = receiver.recv() {
            if store.is_none() {
                store = Store::open_default().ok();
            }
            let mut queue = VecDeque::from(keys);
            while let Some(key) = queue.pop_front() {
                for newer in receiver.try_iter() {
                    queue = VecDeque::from(newer);
                }
                let full_read = key.continuation_after.is_some();
                let value = if key.session.provider == Provider::OpenCode && !full_read {
                    PreviewValue::Unavailable
                } else {
                    let snapshot = if full_read {
                        registry.read_session(&key.session)
                    } else {
                        registry.preview_session(&key.session)
                    };
                    snapshot.map_or(PreviewValue::Unavailable, |snapshot| {
                        if let Some(store) = &store {
                            let document = trajectory_search_document(&snapshot);
                            let _ = store.upsert_session_trajectory(
                                &snapshot.session,
                                &document.text,
                                snapshot.captured_at,
                                full_read,
                            );
                        }
                        PreviewValue::Ready {
                            continuation: key.continuation_after.and_then(|handoff_at| {
                                first_user_message_after(&snapshot, handoff_at)
                            }),
                            preview: Box::new(session_preview(&snapshot)),
                        }
                    })
                };
                if sender.send(PickerUpdate::Preview { key, value }).is_err() {
                    return;
                }
            }
        }
    });
}

fn spawn_search_updates(sender: Sender<PickerUpdate>, receiver: Receiver<SearchIndexRequest>) {
    thread::spawn(move || {
        while let Ok(mut request) = receiver.recv() {
            for newer in receiver.try_iter() {
                request = newer;
            }
            let index = InvertedIndex::build(&request.values);
            if sender
                .send(PickerUpdate::SearchIndex {
                    generation: request.generation,
                    index,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn spawn_trajectory_search_updates(
    sender: Sender<PickerUpdate>,
    receiver: Receiver<TrajectorySearchRequest>,
) {
    thread::spawn(move || {
        let mut store = None;
        while let Ok(mut request) = receiver.recv() {
            for newer in receiver.try_iter() {
                request = newer;
            }
            if store.is_none() {
                store = Store::open_default().ok();
            }
            let result = store.as_ref().map_or_else(
                || Err("local trajectory index is unavailable".to_owned()),
                |store| {
                    store
                        .search_session_trajectories(&request.query, TRAJECTORY_SEARCH_LIMIT)
                        .map_err(|error| error.to_string())
                },
            );
            if sender
                .send(PickerUpdate::TrajectorySearch {
                    generation: request.generation,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn receive_updates(
    receiver: &Receiver<PickerUpdate>,
    state: &mut PickerState,
    pending: &mut HashSet<Provider>,
    warnings: &mut Vec<String>,
    _current_project: &Path,
) -> bool {
    let mut changed = false;
    while let Ok(update) = receiver.try_recv() {
        changed = true;
        match update {
            PickerUpdate::Cached(Ok(entries)) => state.replace_all_entries(entries),
            PickerUpdate::Cached(Err(error)) | PickerUpdate::Warning(error) => {
                warnings.push(error);
            }
            PickerUpdate::Lineage(Ok(records)) => state.replace_lineage(records),
            PickerUpdate::Lineage(Err(error)) => {
                warnings.push(format!("session lineage: {error}"));
            }
            PickerUpdate::Preview { key, value } => {
                state.previews.insert(key, value, &state.preview_window);
                state.trajectory_index_changed();
            }
            PickerUpdate::SearchIndex { generation, index } => {
                if generation == state.entries_generation {
                    state.search_index = Some(index);
                }
            }
            PickerUpdate::TrajectorySearch { generation, result } => {
                if generation == state.trajectory_search_generation {
                    state.trajectory_search_pending = false;
                    match result {
                        Ok(matches) => state.replace_trajectory_matches(matches),
                        Err(error) => warnings.push(format!("trajectory search: {error}")),
                    }
                }
            }
            PickerUpdate::Discovered(update) => {
                pending.remove(&update.provider);
                match update.result {
                    Ok(entries) => state.replace_provider_entries(update.provider, entries),
                    Err(error) => warnings.push(format!("{}: {error}", update.provider)),
                }
            }
            PickerUpdate::RefreshStarted(provider) => {
                pending.insert(provider);
            }
        }
    }
    changed
}

fn indexed_session(session: &NativeSession) -> IndexedSession {
    IndexedSession {
        session: session.session.clone(),
        title: session.title.clone(),
        project_path: session.project_path.clone(),
        git_branch: session.git_branch.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        event_count: session.event_count,
    }
}

fn native_session(session: IndexedSession) -> NativeSession {
    NativeSession {
        session: session.session,
        title: session.title,
        project_path: session.project_path,
        git_branch: session.git_branch,
        created_at: session.created_at,
        updated_at: session.updated_at,
        event_count: session.event_count,
        source_path: None,
    }
}

enum TargetOutcome {
    Back,
    Cancel,
    Selected(TargetChoice),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetChoice {
    provider: Provider,
    fork: bool,
}

enum WorkspaceOutcome {
    Back,
    Cancel,
    Selected(PathBuf),
}

fn pick_workspace(
    source: &SessionRef,
    original: Option<&Path>,
    current: &Path,
) -> Result<WorkspaceOutcome> {
    let mut input = current.display().to_string();
    let mut message = String::new();
    loop {
        render_workspace(source, original, &input, &message)?;
        let Event::Key(key) = event::read().context("reading workspace picker input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(WorkspaceOutcome::Cancel);
        }
        match key.code {
            KeyCode::Esc => return Ok(WorkspaceOutcome::Back),
            KeyCode::Enter | KeyCode::Char('\r' | '\n') => {
                let path = resolve_workspace_input(&input, current);
                if !path.is_dir() {
                    message.clear();
                    message.push_str("Folder does not exist or is not a directory.");
                    continue;
                }
                return path
                    .canonicalize()
                    .map(WorkspaceOutcome::Selected)
                    .context("resolving selected workspace");
            }
            KeyCode::Tab => {
                let completion = complete_directory_input(&input, current);
                input = completion.value;
                message = completion.message;
            }
            KeyCode::Backspace => {
                input.pop();
                message.clear();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                message.clear();
            }
            KeyCode::Char(character)
                if input.chars().count() < 4096
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                input.push(character);
                message.clear();
            }
            _ => {}
        }
    }
}

struct PathCompletion {
    value: String,
    message: String,
}

fn complete_directory_input(input: &str, current: &Path) -> PathCompletion {
    let resolved = resolve_workspace_input(input, current);
    let ends_with_separator = input.ends_with('/') || input.ends_with('\\');
    if resolved.is_dir() && !ends_with_separator {
        return PathCompletion {
            value: format!("{}{MAIN_SEPARATOR}", resolved.display()),
            message: String::new(),
        };
    }
    let (parent, prefix) = if ends_with_separator {
        (resolved, String::new())
    } else {
        (
            resolved
                .parent()
                .map_or_else(|| current.to_path_buf(), Path::to_path_buf),
            resolved
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let mut matches = fs::read_dir(&parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix))
        .collect::<Vec<_>>();
    matches.sort();
    let Some(first) = matches.first() else {
        return PathCompletion {
            value: input.to_owned(),
            message: "No matching directories.".to_owned(),
        };
    };
    let common = matches
        .iter()
        .skip(1)
        .fold(first.clone(), |common, candidate| {
            common_prefix(&common, candidate)
        });
    let completed = parent.join(&common);
    let unique = matches.len() == 1;
    let value = if unique {
        format!("{}{MAIN_SEPARATOR}", completed.display())
    } else {
        completed.display().to_string()
    };
    let message = if unique || common.len() > prefix.len() {
        String::new()
    } else {
        matches.into_iter().take(5).collect::<Vec<_>>().join("  ")
    };
    PathCompletion { value, message }
}

fn resolve_workspace_input(input: &str, current: &Path) -> PathBuf {
    let path = if input == "~" {
        BaseDirs::new().map_or_else(
            || PathBuf::from(input),
            |directories| directories.home_dir().to_path_buf(),
        )
    } else if let Some(suffix) = input.strip_prefix("~/") {
        BaseDirs::new().map_or_else(
            || PathBuf::from(input),
            |directories| directories.home_dir().join(suffix),
        )
    } else {
        PathBuf::from(input)
    };
    if path.is_absolute() {
        path
    } else {
        current.join(path)
    }
}

fn common_prefix(left: &str, right: &str) -> String {
    left.chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character)
        .collect()
}

fn pick_target(source: &SessionRef, targets: &[Provider]) -> Result<TargetOutcome> {
    if targets.is_empty() {
        bail!(
            "no runnable target agents found; install one on PATH or configure an OMNI_*_BIN override"
        );
    }
    let choices = target_choices(source.provider, targets);
    let mut selected = default_target_index(source.provider, &choices);
    loop {
        render_target(source, &choices, selected)?;
        let Event::Key(key) = event::read().context("reading target picker input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(TargetOutcome::Cancel);
        }
        match key.code {
            KeyCode::Esc => return Ok(TargetOutcome::Back),
            KeyCode::Enter | KeyCode::Char('\r' | '\n') => {
                if let Some(choice) = selected.and_then(|index| choices.get(index)).copied() {
                    return Ok(TargetOutcome::Selected(choice));
                }
            }
            KeyCode::Up | KeyCode::Left => {
                selected = move_target_selection(selected, choices.len(), false);
            }
            KeyCode::Down | KeyCode::Right => {
                selected = move_target_selection(selected, choices.len(), true);
            }
            _ => {}
        }
    }
}

fn target_choices(source: Provider, targets: &[Provider]) -> Vec<TargetChoice> {
    let mut choices = Vec::with_capacity(targets.len() + usize::from(targets.contains(&source)));
    for &provider in targets {
        choices.push(TargetChoice {
            provider,
            fork: false,
        });
        if provider == source {
            choices.push(TargetChoice {
                provider,
                fork: true,
            });
        }
    }
    choices
}

fn default_target_index(source: Provider, choices: &[TargetChoice]) -> Option<usize> {
    choices
        .iter()
        .position(|choice| choice.provider == source && !choice.fork)
}

fn move_target_selection(
    selected: Option<usize>,
    target_count: usize,
    forward: bool,
) -> Option<usize> {
    if target_count == 0 {
        return None;
    }
    Some(if forward {
        selected.map_or(0, |index| (index + 1) % target_count)
    } else {
        selected.map_or(target_count - 1, |index| {
            (index + target_count - 1) % target_count
        })
    })
}

fn render_target(
    source: &SessionRef,
    choices: &[TargetChoice],
    selected: Option<usize>,
) -> Result<()> {
    let (width, _) = terminal::size().context("reading terminal size")?;
    let width = usize::from(width).max(1);
    let mut frame = Vec::new();
    queue!(frame, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        frame,
        SetAttribute(Attribute::Bold),
        Print("OmniSession  Choose target agent"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(&format!("Source: {source}"), width)),
        ResetColor,
        Print("\r\n\r\n"),
        Print("Where should this session open?"),
        Print("\r\n\r\n")
    )?;
    for (index, choice) in choices.iter().enumerate() {
        let is_selected = selected == Some(index);
        if is_selected {
            queue!(
                frame,
                SetForegroundColor(Color::Green),
                SetAttribute(Attribute::Bold)
            )?;
        }
        let action = if choice.fork {
            "Fork session"
        } else if choice.provider == source.provider {
            "Continue original session"
        } else {
            "Open continuation in this agent"
        };
        let label = if choice.fork {
            format!("{} · fork", choice.provider)
        } else {
            choice.provider.to_string()
        };
        let marker = if is_selected { "›" } else { " " };
        queue!(
            frame,
            Print(truncate(&format!("{marker} {label}"), width)),
            Print("\r\n"),
            SetForegroundColor(if is_selected {
                Color::Green
            } else {
                Color::DarkGrey
            }),
            Print(truncate(&format!("    {action}"), width)),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print("\r\n\r\n")
        )?;
    }
    if selected.is_none() {
        queue!(
            frame,
            Print("\r\n"),
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(
                "Original agent is unavailable. Choose a target with arrow keys.",
                width
            )),
            ResetColor
        )?;
    }
    queue!(
        frame,
        Print("\r\n\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "↑↓ choose  Enter open  Esc back  Ctrl-C cancel",
            width
        )),
        ResetColor
    )?;
    present_frame(&frame).context("drawing target picker")
}

fn render_workspace(
    source: &SessionRef,
    original: Option<&Path>,
    input: &str,
    message: &str,
) -> Result<()> {
    let (width, _) = terminal::size().context("reading terminal size")?;
    let width = usize::from(width).max(1);
    let original = original.map_or_else(
        || "not recorded".to_owned(),
        |path| path.display().to_string(),
    );
    let mut frame = Vec::new();
    queue!(frame, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        frame,
        SetAttribute(Attribute::Bold),
        Print("OmniSession  Choose workspace"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(&format!("Source: {source}"), width)),
        ResetColor,
        Print("\r\n\r\n"),
        SetForegroundColor(Color::Yellow),
        Print("Saved workspace is unavailable."),
        ResetColor,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(&format!("Saved: {original}"), width)),
        ResetColor,
        Print("\r\n\r\n"),
        Print("Open session from folder:"),
        Print("\r\n"),
        SetForegroundColor(Color::Cyan),
        Print(truncate(&format!("> {input}"), width)),
        ResetColor,
        Print("\r\n")
    )?;
    if !message.is_empty() {
        queue!(
            frame,
            SetForegroundColor(Color::Yellow),
            Print(truncate(message, width)),
            ResetColor,
            Print("\r\n")
        )?;
    }
    queue!(
        frame,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "Tab complete  Enter use folder  Ctrl-U clear  Esc back  Ctrl-C cancel",
            width
        )),
        ResetColor
    )?;
    present_frame(&frame).context("drawing workspace picker")
}

enum PickerAction {
    Continue,
    Cancel,
    Select,
}

fn handle_key(state: &mut PickerState, key: KeyEvent) -> PickerAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return PickerAction::Cancel;
    }
    match key.code {
        KeyCode::Esc => PickerAction::Cancel,
        KeyCode::Enter | KeyCode::Char('\r' | '\n') => PickerAction::Select,
        KeyCode::Up => {
            state.move_selection(-1);
            PickerAction::Continue
        }
        KeyCode::Down => {
            state.move_selection(1);
            PickerAction::Continue
        }
        KeyCode::PageUp => {
            state.move_selection(-10);
            PickerAction::Continue
        }
        KeyCode::PageDown => {
            state.move_selection(10);
            PickerAction::Continue
        }
        KeyCode::Tab | KeyCode::BackTab => {
            state.all_projects = !state.all_projects;
            state.reset_selection();
            PickerAction::Continue
        }
        KeyCode::Left => {
            state.cycle_provider(true);
            PickerAction::Continue
        }
        KeyCode::Right => {
            state.cycle_provider(false);
            PickerAction::Continue
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.query_changed();
            state.reset_selection();
            PickerAction::Continue
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.query.clear();
            state.query_changed();
            state.reset_selection();
            PickerAction::Continue
        }
        KeyCode::Char(character)
            if state.query.chars().count() < 256
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.query.push(character);
            state.query_changed();
            state.reset_selection();
            PickerAction::Continue
        }
        _ => PickerAction::Continue,
    }
}

#[derive(Default)]
struct PickerRenderState {
    terminal_size: Option<(usize, usize)>,
}

impl PickerRenderState {
    fn render(
        &mut self,
        state: &PickerState,
        target: Option<Provider>,
        warning_count: usize,
        pending_count: usize,
    ) -> Result<()> {
        let (width, height) = terminal::size().context("reading terminal size")?;
        let width = usize::from(width).max(1);
        let height = usize::from(height).max(1);
        let frame = picker_frame(
            state,
            target,
            warning_count,
            pending_count,
            self,
            width,
            height,
        )?;
        present_frame(&frame).context("drawing session picker")
    }
}

fn picker_frame(
    state: &PickerState,
    target: Option<Provider>,
    warning_count: usize,
    pending_count: usize,
    render_state: &mut PickerRenderState,
    width: usize,
    height: usize,
) -> Result<Vec<u8>> {
    let layout = screen_layout(width, height);
    let visible = state.visible_indices();
    let row_count = layout.list.height.saturating_sub(2).max(1);
    let (first, selected) = centered_list_window(visible.len(), state.selected, row_count);
    let mut frame = Vec::with_capacity(width.saturating_mul(height).saturating_mul(2));
    if render_state.terminal_size != Some((width, height)) {
        queue!(frame, MoveTo(0, 0), Clear(ClearType::All))?;
    }
    render_header(&mut frame, state, target, visible.len(), &layout)?;
    render_session_list(
        &mut frame,
        state,
        &visible,
        ListViewport {
            first,
            selected,
            height: row_count,
            pending_count,
        },
        layout.list,
    )?;
    if let Some(detail) = layout.detail {
        render_selected_detail(&mut frame, state, detail, layout.detail_right)?;
    }
    render_status(
        &mut frame,
        warning_count,
        pending_count,
        state.trajectory_search_pending,
        layout.status_y,
        width,
        target.is_some(),
    )?;
    render_state.terminal_size = Some((width, height));
    Ok(frame)
}

fn present_frame(frame: &[u8]) -> Result<()> {
    let mut output = io::stdout().lock();
    present_frame_to(&mut output, frame)
}

fn present_frame_to(output: &mut impl Write, frame: &[u8]) -> Result<()> {
    output
        .sync_update(|output| output.write_all(frame))
        .context("starting synchronized terminal update")?
        .context("writing synchronized terminal update")
}

fn terminal_list_row_count() -> Result<usize> {
    let (width, height) = terminal::size().context("reading terminal size")?;
    Ok(
        screen_layout(usize::from(width).max(1), usize::from(height).max(1))
            .list
            .height
            .saturating_sub(2)
            .max(1),
    )
}

fn centered_list_window(visible_count: usize, selected: usize, row_count: usize) -> (usize, usize) {
    let selected = selected.min(visible_count.saturating_sub(1));
    let first = selected
        .saturating_sub(row_count / 2)
        .min(visible_count.saturating_sub(row_count));
    (first, selected)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenLayout {
    list: Rect,
    detail: Option<Rect>,
    detail_right: bool,
    status_y: usize,
}

fn screen_layout(width: usize, height: usize) -> ScreenLayout {
    let status_y = height.saturating_sub(1);
    let body_top = 4.min(status_y);
    let body_height = status_y.saturating_sub(body_top);
    if width >= 139 && body_height >= 10 {
        let list_width = width.saturating_sub(55).clamp(84, 120);
        return ScreenLayout {
            list: Rect {
                x: 0,
                y: body_top,
                width: list_width,
                height: body_height,
            },
            detail: Some(Rect {
                x: list_width + 3,
                y: body_top,
                width: width.saturating_sub(list_width + 3),
                height: body_height,
            }),
            detail_right: true,
            status_y,
        };
    }
    if body_height >= 10 {
        let detail_height = (body_height / 2).clamp(7, 10);
        let list_height = body_height.saturating_sub(detail_height + 1);
        return ScreenLayout {
            list: Rect {
                x: 0,
                y: body_top,
                width,
                height: list_height,
            },
            detail: Some(Rect {
                x: 0,
                y: body_top + list_height + 1,
                width,
                height: detail_height,
            }),
            detail_right: false,
            status_y,
        };
    }
    ScreenLayout {
        list: Rect {
            x: 0,
            y: body_top,
            width,
            height: body_height,
        },
        detail: None,
        detail_right: false,
        status_y,
    }
}

fn render_header(
    output: &mut impl Write,
    state: &PickerState,
    target: Option<Provider>,
    match_count: usize,
    layout: &ScreenLayout,
) -> Result<()> {
    let width = if layout.detail_right {
        layout.list.width + layout.detail.map_or(0, |detail| detail.width + 3)
    } else {
        layout.list.width
    };
    let count = if match_count == 1 {
        "1 session".to_owned()
    } else {
        format!("{} sessions", grouped_number(match_count))
    };
    draw_line(
        output,
        Rect {
            x: 0,
            y: 0,
            width,
            height: 1,
        },
        &format!("OmniSession  SESSION BROWSER  ·  {count}"),
        DetailStyle::Strong,
        false,
    )?;
    let target_label = target.map_or_else(
        || "choose after source".to_owned(),
        |provider| provider.to_string(),
    );
    let scope = if state.all_projects {
        "all workspaces"
    } else {
        "current workspace"
    };
    let provider = state
        .provider()
        .map_or_else(|| "all sources".to_owned(), |provider| provider.to_string());
    draw_line(
        output,
        Rect {
            x: 0,
            y: 1,
            width,
            height: 1,
        },
        &format!("Scope  {scope} [Tab]   Source  {provider} [←/→]   Target  {target_label}"),
        DetailStyle::Accent,
        false,
    )?;
    let query = if state.query.is_empty() {
        "title, trajectory, session ID, directory, or branch".to_owned()
    } else {
        state.query.clone()
    };
    draw_line(
        output,
        Rect {
            x: 0,
            y: 2,
            width,
            height: 1,
        },
        &format!("Search › {query}"),
        if state.query.is_empty() {
            DetailStyle::Muted
        } else {
            DetailStyle::Normal
        },
        false,
    )?;
    draw_line(
        output,
        Rect {
            x: 0,
            y: 3,
            width,
            height: 1,
        },
        &"─".repeat(width),
        DetailStyle::Muted,
        false,
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
struct ListViewport {
    first: usize,
    selected: usize,
    height: usize,
    pending_count: usize,
}

fn render_session_list(
    output: &mut impl Write,
    state: &PickerState,
    visible: &[usize],
    viewport: ListViewport,
    area: Rect,
) -> Result<()> {
    if area.height == 0 {
        return Ok(());
    }
    let columns = ListColumns::for_width(area.width);
    draw_line(
        output,
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        &columns.header(),
        DetailStyle::Muted,
        false,
    )?;
    if area.height == 1 {
        return Ok(());
    }
    draw_line(
        output,
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        },
        &"─".repeat(area.width),
        DetailStyle::Muted,
        false,
    )?;
    let body_height = area.height.saturating_sub(2);
    if body_height == 0 {
        return Ok(());
    }
    let drawn_rows;
    if visible.is_empty() {
        draw_line(
            output,
            Rect {
                x: area.x,
                y: area.y + 2,
                width: area.width,
                height: 1,
            },
            empty_list_hint(state.all_projects, viewport.pending_count),
            DetailStyle::Muted,
            false,
        )?;
        drawn_rows = 1;
    } else {
        let entries = visible
            .iter()
            .skip(viewport.first)
            .take(viewport.height.min(body_height));
        let mut row_count = 0;
        for (row, entry_index) in entries.enumerate() {
            let entry = &state.entries[*entry_index].session;
            let is_selected = viewport.first + row == viewport.selected;
            let key = state.preview_key(entry);
            let preview = state.previews.get(&key);
            draw_line(
                output,
                Rect {
                    x: area.x,
                    y: area.y + 2 + row,
                    width: area.width,
                    height: 1,
                },
                &session_line(
                    entry,
                    is_selected,
                    &state.lineage.list_prefix(&entry.session),
                    &columns,
                    preview,
                ),
                if is_selected {
                    DetailStyle::Selected
                } else {
                    DetailStyle::Normal
                },
                is_selected,
            )?;
            row_count = row + 1;
        }
        drawn_rows = row_count;
    }
    erase_rows(
        output,
        Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: body_height,
        },
        drawn_rows,
    )
}

fn empty_list_hint(all_projects: bool, pending_count: usize) -> &'static str {
    if pending_count > 0 {
        "Scanning provider stores... results appear as they arrive."
    } else if all_projects {
        "No matching sessions. Clear search or change source provider."
    } else {
        "No matching sessions here. Press Tab to search all workspaces."
    }
}

fn render_selected_detail(
    output: &mut impl Write,
    state: &PickerState,
    area: Rect,
    right: bool,
) -> Result<()> {
    if right {
        for y in area.y..area.y + area.height {
            draw_line(
                output,
                Rect {
                    x: area.x - 2,
                    y,
                    width: 1,
                    height: 1,
                },
                "│",
                DetailStyle::Muted,
                false,
            )?;
        }
    } else {
        draw_line(
            output,
            Rect {
                x: area.x,
                y: area.y - 1,
                width: area.width,
                height: 1,
            },
            &"─".repeat(area.width),
            DetailStyle::Muted,
            false,
        )?;
    }
    let lines = selected_detail_lines(state, area.width, area.height);
    let visible_lines = lines.len().min(area.height);
    for (row, line) in lines.into_iter().take(visible_lines).enumerate() {
        draw_line(
            output,
            Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            },
            &line.text,
            line.style,
            false,
        )?;
    }
    erase_rows(output, area, visible_lines)
}

fn erase_rows(output: &mut impl Write, area: Rect, first_row: usize) -> Result<()> {
    for row in first_row..area.height {
        draw_line(
            output,
            Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            },
            "",
            DetailStyle::Normal,
            false,
        )?;
    }
    Ok(())
}

fn render_status(
    output: &mut impl Write,
    warning_count: usize,
    pending_count: usize,
    trajectory_search_pending: bool,
    y: usize,
    width: usize,
    fixed_target: bool,
) -> Result<()> {
    let action = if fixed_target { "resume" } else { "continue" };
    let warning = if trajectory_search_pending {
        format!("Searching indexed trajectories  ·  ↑↓ move  Enter {action}  Esc cancel")
    } else if pending_count > 0 {
        format!(
            "Refreshing {pending_count} source(s)  ·  ↑↓ move  Tab workspace  ←/→ source  Enter {action}"
        )
    } else if warning_count == 0 {
        format!("↑↓ move  PgUp/PgDn jump  Tab workspace  ←/→ source  Enter {action}  Esc cancel")
    } else {
        format!(
            "↑↓ move  Enter {action}  ·  {warning_count} provider warning(s); run `omnis doctor`"
        )
    };
    draw_line(
        output,
        Rect {
            x: 0,
            y,
            width,
            height: 1,
        },
        &warning,
        DetailStyle::Muted,
        false,
    )
}

#[derive(Clone, Copy)]
enum DetailStyle {
    Normal,
    Muted,
    Accent,
    Strong,
    Selected,
}

struct DetailLine {
    text: String,
    style: DetailStyle,
}

fn detail_line(text: impl Into<String>, style: DetailStyle) -> DetailLine {
    DetailLine {
        text: text.into(),
        style,
    }
}

fn detail_field(label: &str, value: &str, width: usize, style: DetailStyle) -> DetailLine {
    const LABEL_WIDTH: usize = 10;
    let value_width = width.saturating_sub(LABEL_WIDTH + 1);
    let value = truncate_middle(value, value_width);
    detail_line(format!("{label:<LABEL_WIDTH$} {value}"), style)
}

struct SessionLocation {
    project: String,
    workspace: String,
    directory: String,
    branch: String,
    compact_branch: String,
    current_branch: Option<String>,
    head: Option<String>,
    state: String,
}

fn session_location(
    state: &PickerState,
    entry: &PickerEntry,
    preview: Option<&SessionPreview>,
) -> SessionLocation {
    let workspace_root = preview
        .and_then(|preview| preview.workspace_root.as_deref())
        .or(entry.session.project_path.as_deref());
    let current_dir = preview
        .and_then(|preview| preview.current_dir.as_deref())
        .or(entry.session.project_path.as_deref());
    let workspace = workspace_root.map_or_else(
        || "not recorded".to_owned(),
        |path| safe_terminal_line(&path.display().to_string()),
    );
    let directory = current_dir.map_or_else(
        || "not recorded".to_owned(),
        |path| safe_terminal_line(&path.display().to_string()),
    );
    let project = workspace_root
        .or(current_dir)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map_or_else(|| "not recorded".to_owned(), safe_terminal_line);
    let head = preview
        .and_then(|preview| preview.git_head.as_deref())
        .map(safe_terminal_line);
    let recorded_branch = preview
        .and_then(|preview| preview.git_branch.as_deref())
        .or(entry.session.git_branch.as_deref());
    let current_branch = if entry.current_workspace {
        state.current_git_branch.as_deref()
    } else {
        None
    };
    let branch = recorded_branch.map_or_else(
        || {
            if head.is_some() {
                "detached HEAD".to_owned()
            } else {
                "not recorded".to_owned()
            }
        },
        safe_terminal_line,
    );
    let current_branch = current_branch
        .filter(|current| recorded_branch != Some(*current))
        .map(safe_terminal_line);
    let compact_branch = if recorded_branch.is_none() {
        current_branch
            .as_deref()
            .map_or_else(|| branch.clone(), |current| format!("{current} (current)"))
    } else {
        branch.clone()
    };
    let state = if entry.current_workspace {
        "current workspace"
    } else if entry.session.project_path.is_some() {
        "other workspace"
    } else if current_dir.is_some() {
        "recovered from trajectory"
    } else {
        "workspace not recorded"
    };
    SessionLocation {
        project,
        workspace,
        directory,
        branch,
        compact_branch,
        current_branch,
        head,
        state: state.to_owned(),
    }
}

fn selected_detail_lines(state: &PickerState, width: usize, height: usize) -> Vec<DetailLine> {
    let Some(entry) = state.selected_entry() else {
        return vec![detail_line(
            "Select a session to inspect it.",
            DetailStyle::Muted,
        )];
    };
    let key = state.preview_key(&entry.session);
    let preview = state.previews.get(&key);
    let ready_preview = match preview {
        Some(PreviewValue::Ready { preview, .. }) => Some(preview.as_ref()),
        _ => None,
    };
    let title = display_title(&entry.session, preview);
    let location = session_location(state, entry, ready_preview);
    let cache_state = if entry.cached { " · indexed" } else { "" };
    let activity = match preview {
        Some(PreviewValue::Ready { preview, .. }) if preview.message_count > 0 => {
            format!(" · {} messages", preview.message_count)
        }
        _ if entry.session.event_count > 0 => {
            format!(" · {} events", entry.session.event_count)
        }
        _ => String::new(),
    };
    let mut lines = vec![
        detail_line("SELECTED SESSION", DetailStyle::Accent),
        detail_line(title, DetailStyle::Strong),
        detail_line(
            format!(
                "{}{activity} · {}{cache_state}",
                entry.session.session.provider,
                relative_time(entry.session.updated_at),
            ),
            DetailStyle::Muted,
        ),
    ];
    append_lineage_tree(&mut lines, state, &entry.session.session, width, height);
    let workspace_height = height.saturating_sub(lines.len());
    append_workspace_details(&mut lines, &location, entry, width, workspace_height);
    lines.push(detail_line("CONVERSATION", DetailStyle::Accent));
    let conversation_height = height.saturating_sub(lines.len());
    match preview {
        Some(PreviewValue::Ready { preview, .. }) => {
            append_preview_lines(&mut lines, preview, width, conversation_height);
        }
        Some(PreviewValue::Unavailable) => {
            lines.push(detail_line("Preview unavailable", DetailStyle::Muted));
        }
        None => lines.push(detail_line("Loading selected session…", DetailStyle::Muted)),
    }
    lines
}

fn append_lineage_tree(
    lines: &mut Vec<DetailLine>,
    state: &PickerState,
    selected: &SessionRef,
    width: usize,
    height: usize,
) {
    let nodes = state.lineage.tree(selected);
    if nodes.is_empty() {
        return;
    }
    let limit = if height < 15 {
        height.saturating_sub(lines.len() + 2)
    } else {
        (height / 3).clamp(4, 12)
    };
    if limit == 0 {
        return;
    }

    lines.push(detail_line(String::new(), DetailStyle::Normal));
    let agent_count = nodes
        .iter()
        .map(|node| node.session.provider)
        .collect::<HashSet<_>>()
        .len();
    lines.push(detail_line(
        format!(
            "SESSION TREE · {} {} · {agent_count} {}",
            nodes.len(),
            if nodes.len() == 1 {
                "session"
            } else {
                "sessions"
            },
            if agent_count == 1 { "agent" } else { "agents" }
        ),
        DetailStyle::Accent,
    ));

    let selected_index = nodes.iter().position(|node| node.selected).unwrap_or(0);
    let start = selected_index
        .saturating_sub(limit / 2)
        .min(nodes.len().saturating_sub(limit));
    let end = (start + limit).min(nodes.len());
    if start > 0 {
        lines.push(detail_line(
            format!("… {start} earlier tree nodes"),
            DetailStyle::Muted,
        ));
    }
    lines.extend(
        nodes[start..end]
            .iter()
            .map(|node| lineage_tree_line(state, node, width)),
    );
    if end < nodes.len() {
        lines.push(detail_line(
            format!("… {} more tree nodes", nodes.len() - end),
            DetailStyle::Muted,
        ));
    }
}

fn lineage_tree_line(state: &PickerState, node: &LineageTreeNode, width: usize) -> DetailLine {
    let marker = if node.selected { "●" } else { "○" };
    let key = node.session.to_string();
    let Some(entry) = state
        .entry_positions
        .get(&key)
        .and_then(|index| state.entries.get(*index))
    else {
        return detail_line(
            format!(
                "{}{marker} {} · not indexed",
                node.branch,
                short_session_ref(&node.session)
            ),
            if node.selected {
                DetailStyle::Strong
            } else {
                DetailStyle::Muted
            },
        );
    };
    let preview_key = state.preview_key(&entry.session);
    let title = display_title(&entry.session, state.previews.get(&preview_key));
    let project = entry
        .session
        .project_path
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(safe_terminal_line)
        .filter(|project| project != "unknown");
    let project = if width >= 52 {
        project.map_or_else(String::new, |project| format!(" · {project}"))
    } else {
        String::new()
    };
    detail_line(
        format!(
            "{}{marker} {:<10} {title}{project}",
            node.branch, entry.session.session.provider
        ),
        if node.selected {
            DetailStyle::Strong
        } else {
            DetailStyle::Normal
        },
    )
}

fn append_workspace_details(
    lines: &mut Vec<DetailLine>,
    location: &SessionLocation,
    entry: &PickerEntry,
    width: usize,
    height: usize,
) {
    if height < 15 {
        lines.push(detail_line(
            format!("{} · {}", location.project, location.compact_branch),
            DetailStyle::Muted,
        ));
        return;
    }
    if height < 22 {
        append_compact_workspace_details(lines, location, entry, width);
        return;
    }
    append_full_workspace_details(lines, location, entry, width, height >= 32);
}

fn append_compact_workspace_details(
    lines: &mut Vec<DetailLine>,
    location: &SessionLocation,
    entry: &PickerEntry,
    width: usize,
) {
    lines.extend([
        detail_field("Directory", &location.directory, width, DetailStyle::Normal),
        detail_field(
            "Branch",
            &location.compact_branch,
            width,
            DetailStyle::Normal,
        ),
        detail_field(
            "Session",
            &entry.session.session.to_string(),
            width,
            DetailStyle::Muted,
        ),
        detail_line(String::new(), DetailStyle::Normal),
    ]);
}

fn append_full_workspace_details(
    lines: &mut Vec<DetailLine>,
    location: &SessionLocation,
    entry: &PickerEntry,
    width: usize,
    expanded: bool,
) {
    lines.push(detail_line("WORKSPACE", DetailStyle::Accent));
    lines.push(detail_field(
        "Project",
        &location.project,
        width,
        DetailStyle::Normal,
    ));
    lines.push(detail_field(
        "Directory",
        &location.directory,
        width,
        DetailStyle::Normal,
    ));
    if expanded && location.workspace != location.directory {
        lines.push(detail_field(
            "Workspace",
            &location.workspace,
            width,
            DetailStyle::Normal,
        ));
    }
    lines.push(detail_field(
        "Branch",
        &location.branch,
        width,
        DetailStyle::Normal,
    ));
    if let Some(current_branch) = &location.current_branch {
        lines.push(detail_field(
            "Current",
            current_branch,
            width,
            DetailStyle::Accent,
        ));
    }
    if expanded {
        if let Some(head) = &location.head {
            lines.push(detail_field("HEAD", head, width, DetailStyle::Muted));
        }
    }
    lines.push(detail_field(
        "Location",
        &location.state,
        width,
        DetailStyle::Muted,
    ));
    if expanded {
        lines.push(detail_field(
            "Created",
            &format_timestamp(entry.session.created_at),
            width,
            DetailStyle::Muted,
        ));
        lines.push(detail_field(
            "Updated",
            &format_timestamp(entry.session.updated_at),
            width,
            DetailStyle::Muted,
        ));
    }
    lines.push(detail_field(
        "Session",
        &entry.session.session.to_string(),
        width,
        DetailStyle::Muted,
    ));
    lines.push(detail_line(String::new(), DetailStyle::Normal));
}

fn format_timestamp(timestamp: Option<DateTime<Utc>>) -> String {
    timestamp.map_or_else(
        || "not recorded".to_owned(),
        |timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        },
    )
}

fn append_preview_lines(
    lines: &mut Vec<DetailLine>,
    preview: &SessionPreview,
    width: usize,
    height: usize,
) {
    let excerpt_lines = if height >= 24 {
        4
    } else if height >= 15 {
        2
    } else {
        1
    };
    match (&preview.first, &preview.latest) {
        (None, _) => lines.push(detail_line("No visible messages", DetailStyle::Muted)),
        (Some(first), Some(_)) if preview.message_count == 1 => {
            append_message_excerpt(lines, "ONLY MESSAGE", first, width, excerpt_lines);
        }
        (Some(first), Some(latest)) => {
            append_message_excerpt(lines, "FIRST MESSAGE", first, width, excerpt_lines);
            if height >= 15 {
                lines.push(detail_line(String::new(), DetailStyle::Normal));
            }
            append_message_excerpt(lines, "LATEST MESSAGE", latest, width, excerpt_lines);
        }
        (Some(first), None) => {
            append_message_excerpt(lines, "FIRST MESSAGE", first, width, excerpt_lines);
        }
    }
}

fn preview_title(preview: &PreviewValue) -> Option<String> {
    let PreviewValue::Ready { preview, .. } = preview else {
        return None;
    };
    preview
        .first
        .as_ref()
        .map(|message| compact_text(&message.text))
        .filter(|title| !title.is_empty())
}

fn compact_text(value: &str) -> String {
    safe_terminal_line(value)
        .replace("\\r\\n", " ")
        .replace("\\n", " ")
        .replace("\\t", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn append_message_excerpt(
    lines: &mut Vec<DetailLine>,
    label: &str,
    message: &HandoffMessage,
    width: usize,
    max_lines: usize,
) {
    lines.push(detail_line(
        format!("{label} · {}", role_label(message.role)),
        DetailStyle::Muted,
    ));
    lines.extend(
        wrap_text(&message.text, width, max_lines)
            .into_iter()
            .map(|line| detail_line(line, DetailStyle::Normal)),
    );
}

const fn role_label(role: HandoffRole) -> &'static str {
    match role {
        HandoffRole::User => "USER",
        HandoffRole::Assistant => "ASSISTANT",
    }
}

fn wrap_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let value = compact_text(value);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) <= width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            if lines.len() == max_lines {
                break;
            }
        }
        current = truncate(word, width);
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    if lines.len() == max_lines {
        let consumed = lines.join(" ");
        if consumed != value {
            let last = lines.pop().unwrap_or_default();
            lines.push(with_ellipsis(&last, width));
        }
    }
    lines
}

fn with_ellipsis(value: &str, width: usize) -> String {
    if value.ends_with('…') {
        value.to_owned()
    } else {
        truncate(&format!("{value}…"), width)
    }
}

fn draw_line(
    output: &mut impl Write,
    area: Rect,
    text: &str,
    style: DetailStyle,
    reverse: bool,
) -> Result<()> {
    let color = match style {
        DetailStyle::Muted => Color::DarkGrey,
        DetailStyle::Accent => Color::Cyan,
        DetailStyle::Selected => Color::Green,
        DetailStyle::Normal | DetailStyle::Strong => Color::Reset,
    };
    queue!(
        output,
        MoveTo(
            u16::try_from(area.x).unwrap_or(u16::MAX),
            u16::try_from(area.y).unwrap_or(u16::MAX)
        ),
        SetForegroundColor(color)
    )?;
    if matches!(style, DetailStyle::Strong | DetailStyle::Selected) {
        queue!(output, SetAttribute(Attribute::Bold))?;
    }
    if reverse {
        queue!(output, SetAttribute(Attribute::Reverse))?;
    }
    queue!(
        output,
        Print(fit_cell(text, area.width)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ListColumns {
    width: usize,
    agent: usize,
    title: usize,
    project: Option<usize>,
    age: Option<usize>,
}

impl ListColumns {
    fn for_width(width: usize) -> Self {
        if width >= 80 {
            let agent = 18;
            let available = width.saturating_sub(agent + 15);
            let title = (available * 2 / 3).clamp(24, 52);
            Self {
                width,
                agent,
                title,
                project: Some(available.saturating_sub(title)),
                age: Some(10),
            }
        } else if width >= 56 {
            let agent = 16;
            Self {
                width,
                agent,
                title: width.saturating_sub(agent + 14),
                project: None,
                age: Some(10),
            }
        } else {
            let agent = 14;
            Self {
                width,
                agent,
                title: width.saturating_sub(agent + 4),
                project: None,
                age: None,
            }
        }
    }

    fn header(self) -> String {
        self.line(" ", "AGENT", "TITLE", "PROJECT", "UPDATED")
    }

    fn line(self, marker: &str, agent: &str, title: &str, project: &str, age: &str) -> String {
        let mut line = format!(
            "{} {} {}",
            fit_cell(marker, 1),
            fit_cell(agent, self.agent),
            fit_cell(title, self.title)
        );
        if let Some(project_width) = self.project {
            line.push(' ');
            line.push_str(&fit_cell(project, project_width));
        }
        if let Some(age_width) = self.age {
            line.push(' ');
            let age = truncate(age, age_width);
            line.push_str(
                &" ".repeat(age_width.saturating_sub(UnicodeWidthStr::width(age.as_str()))),
            );
            line.push_str(&age);
        }
        fit_cell(&line, self.width)
    }
}

fn session_line(
    session: &NativeSession,
    selected: bool,
    lineage_prefix: &str,
    columns: &ListColumns,
    preview: Option<&PreviewValue>,
) -> String {
    let marker = if selected { "›" } else { " " };
    let provider = format!("{lineage_prefix}{}", session.session.provider);
    let raw_title = display_title(session, preview);
    let raw_project = session
        .project_path
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map_or_else(|| "unknown".to_owned(), safe_terminal_line);
    let age = relative_time(session.updated_at);
    columns.line(marker, &provider, &raw_title, &raw_project, &age)
}

fn display_title(session: &NativeSession, preview: Option<&PreviewValue>) -> String {
    preview
        .and_then(preview_continuation_title)
        .or_else(|| {
            session
                .title
                .as_deref()
                .map(safe_terminal_line)
                .filter(|title| {
                    let title = title.trim();
                    !title.is_empty()
                        && title != session.session.id
                        && title != session.session.to_string()
                })
        })
        .or_else(|| preview.and_then(preview_title))
        .unwrap_or_else(|| {
            if preview.is_none() {
                "Loading title…".to_owned()
            } else {
                "Untitled session".to_owned()
            }
        })
}

fn preview_continuation_title(preview: &PreviewValue) -> Option<String> {
    let PreviewValue::Ready { continuation, .. } = preview else {
        return None;
    };
    continuation
        .as_ref()
        .map(|message| compact_text(&message.text))
        .filter(|title| !title.is_empty())
}

fn short_session_ref(session: &SessionRef) -> String {
    format!(
        "{}:{}",
        session.provider,
        short_id(&safe_terminal_line(&session.id))
    )
}

fn search_text(session: &NativeSession) -> String {
    format!(
        "{} {} {} {} {}",
        session.session.provider,
        session.session.id,
        session
            .project_path
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string()),
        session.git_branch.as_deref().unwrap_or_default(),
        session
            .title
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(512)
            .collect::<String>(),
    )
    .to_lowercase()
    .chars()
    .take(2_048)
    .collect()
}

fn short_id(id: &str) -> String {
    let mut value = id.chars().take(12).collect::<String>();
    if id.chars().count() > 12 {
        value.push('…');
    }
    value
}

fn relative_time(updated_at: Option<DateTime<Utc>>) -> String {
    let Some(updated_at) = updated_at else {
        return "unknown".to_owned();
    };
    let seconds = (Utc::now() - updated_at).num_seconds().max(0);
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => updated_at.format("%Y-%m-%d").to_string(),
    }
}

fn truncate(value: &str, width: usize) -> String {
    let value = safe_terminal_line(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut used = 0;
    let mut truncated = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        used += character_width;
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

fn truncate_middle(value: &str, width: usize) -> String {
    let value = safe_terminal_line(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let content_width = width - 1;
    let prefix_width = content_width * 2 / 5;
    let suffix_width = content_width - prefix_width;
    let prefix = take_prefix_cells(&value, prefix_width);
    let suffix = take_suffix_cells(&value, suffix_width);
    format!("{prefix}…{suffix}")
}

fn take_prefix_cells(value: &str, width: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > width {
                return false;
            }
            used += character_width;
            true
        })
        .collect()
}

fn take_suffix_cells(value: &str, width: usize) -> String {
    let mut used = 0;
    let mut suffix = value
        .chars()
        .rev()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > width {
                return false;
            }
            used += character_width;
            true
        })
        .collect::<Vec<_>>();
    suffix.reverse();
    suffix.into_iter().collect()
}

fn fit_cell(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{value}{}", " ".repeat(padding))
}

fn grouped_number(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).checked_rem(3) == Some(0) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enabling session picker terminal mode")?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error).context("opening session picker");
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn filtering_combines_scope_provider_and_search() {
        let current = Path::new("/workspace/current");
        let mut state = PickerState::new(
            vec![
                session(
                    Provider::Claude,
                    "claude-current",
                    current,
                    Some("Auth refactor"),
                ),
                session(
                    Provider::Codex,
                    "codex-current",
                    current,
                    Some("Pagination"),
                ),
                session(
                    Provider::Claude,
                    "claude-other",
                    Path::new("/workspace/other"),
                    Some("Billing"),
                ),
            ],
            current,
            Some(Provider::Claude),
            false,
        );

        assert_eq!(state.visible_indices().len(), 1);
        state.all_projects = true;
        assert_eq!(state.visible_indices().len(), 2);
        state.query = "billing".to_owned();
        assert_eq!(state.visible_indices().len(), 1);
        assert_eq!(
            state
                .selected_entry()
                .expect("selected session")
                .session
                .session
                .id,
            "claude-other"
        );
        state.query = "aude-oth".to_owned();
        assert_eq!(state.visible_indices().len(), 1);
        state.query = "workspace/other".to_owned();
        assert_eq!(state.visible_indices().len(), 1);
    }

    #[test]
    fn provider_cycle_includes_all_sources() {
        let mut state = PickerState::new(Vec::new(), Path::new("/workspace"), None, false);
        assert_eq!(state.provider(), None);
        state.cycle_provider(false);
        assert_eq!(state.provider(), Some(Provider::Claude));
        state.cycle_provider(true);
        assert_eq!(state.provider(), None);
    }

    #[test]
    fn picker_keys_change_scope_and_confirm_selection() {
        let mut state = PickerState::new(
            vec![session(
                Provider::Codex,
                "session",
                Path::new("/workspace"),
                None,
            )],
            Path::new("/workspace"),
            None,
            false,
        );
        assert!(matches!(
            handle_key(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            PickerAction::Continue
        ));
        assert!(state.all_projects);
        assert!(matches!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            PickerAction::Select
        ));
    }

    #[test]
    fn wide_browser_layout_balances_list_and_detail_panes() {
        let layout = screen_layout(200, 50);

        assert!(layout.detail_right);
        assert_eq!(layout.list.width, 120);
        assert_eq!(
            layout.detail,
            Some(Rect {
                x: 123,
                y: 4,
                width: 77,
                height: 45,
            })
        );

        assert!(screen_layout(139, 30).detail_right);
        assert!(!screen_layout(138, 30).detail_right);
    }

    #[test]
    fn repeated_picker_frames_clear_only_after_terminal_resize() {
        let current = Path::new("/workspace");
        let state = PickerState::new(
            vec![session(
                Provider::Codex,
                "session",
                current,
                Some("Smooth rendering"),
            )],
            current,
            None,
            false,
        );
        let mut render_state = PickerRenderState::default();
        let mut clear = Vec::new();
        queue!(clear, Clear(ClearType::All)).expect("clear command");

        let first =
            picker_frame(&state, None, 0, 0, &mut render_state, 160, 24).expect("initial frame");
        let repeated =
            picker_frame(&state, None, 0, 0, &mut render_state, 160, 24).expect("repeated frame");
        let resized =
            picker_frame(&state, None, 0, 0, &mut render_state, 161, 24).expect("resized frame");

        assert!(contains_bytes(&first, &clear));
        assert!(!contains_bytes(&repeated, &clear));
        assert!(contains_bytes(&resized, &clear));
    }

    #[test]
    fn terminal_frame_is_presented_as_one_synchronized_update() {
        let mut output = Vec::new();

        present_frame_to(&mut output, b"complete frame").expect("present frame");

        assert_eq!(output, b"\x1b[?2026hcomplete frame\x1b[?2026l");
    }

    #[test]
    fn session_list_erases_rows_left_by_shorter_results() {
        let current = Path::new("/workspace");
        let state = PickerState::new(
            vec![session(
                Provider::Codex,
                "session",
                current,
                Some("One row"),
            )],
            current,
            None,
            false,
        );
        let visible = state.visible_indices();
        let mut output = Vec::new();

        render_session_list(
            &mut output,
            &state,
            &visible,
            ListViewport {
                first: 0,
                selected: 0,
                height: 4,
                pending_count: 0,
            },
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 6,
            },
        )
        .expect("render list");

        for row in 3..6 {
            let mut position = Vec::new();
            queue!(position, MoveTo(0, row)).expect("cursor position");
            assert!(
                contains_bytes(&output, &position),
                "row {row} was not erased"
            );
        }
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
    }

    #[test]
    fn list_columns_align_project_and_age_for_every_row() {
        let columns = ListColumns::for_width(120);
        let short_age = columns.line(" ", "codex", "Session", "project", "2m");
        let long_age = columns.line(" ", "claude", "Session", "project", "2026-07-28");
        let header = columns.header();

        assert_eq!(UnicodeWidthStr::width(short_age.as_str()), 120);
        assert_eq!(UnicodeWidthStr::width(long_age.as_str()), 120);
        assert_eq!(short_age.find("project"), long_age.find("project"));
        assert_eq!(short_age.find("project"), header.find("PROJECT"));
        assert!(short_age.ends_with("        2m"));
        assert!(long_age.ends_with("2026-07-28"));
    }

    #[test]
    fn terminal_cells_align_wide_unicode_content() {
        let cell = fit_cell("你好 session", 12);
        let truncated = fit_cell("你好你好你好", 7);

        assert_eq!(UnicodeWidthStr::width(cell.as_str()), 12);
        assert_eq!(UnicodeWidthStr::width(truncated.as_str()), 7);
        assert!(truncated.contains('…'));
    }

    #[test]
    fn middle_truncation_keeps_workspace_root_and_project_name() {
        let path = truncate_middle("/users/demo/workspaces/company/very-long-project-name", 38);

        assert!(path.starts_with("/users/demo/"));
        assert!(path.ends_with("very-long-project-name"));
        assert_eq!(UnicodeWidthStr::width(path.as_str()), 38);
    }

    #[test]
    fn selected_detail_shows_first_and_latest_messages() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            vec![session(
                Provider::Codex,
                "session",
                current,
                Some("Fix pagination"),
            )],
            current,
            None,
            false,
        );
        state.current_git_branch = Some("main".to_owned());
        let key = PreviewKey {
            session: SessionRef::new(Provider::Codex, "session"),
            updated_at: state.entries[0].session.updated_at,
            continuation_after: None,
        };
        state.previews.insert(
            key,
            PreviewValue::Ready {
                preview: Box::new(SessionPreview {
                    first: Some(HandoffMessage {
                        role: HandoffRole::User,
                        text: "Start with cursor pagination.".to_owned(),
                    }),
                    latest: Some(HandoffMessage {
                        role: HandoffRole::Assistant,
                        text: "Tests pass and the patch is ready.".to_owned(),
                    }),
                    message_count: 2,
                    workspace_root: Some(PathBuf::from("/workspace")),
                    current_dir: Some(PathBuf::from("/workspace/crates/omnis-cli")),
                    git_branch: Some("feature/session-details".to_owned()),
                    git_head: Some("0123456789abcdef".to_owned()),
                }),
                continuation: None,
            },
            &[],
        );

        let lines = selected_detail_lines(&state, 72, 40)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();

        assert!(lines.iter().any(|line| line == "WORKSPACE"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/workspace/crates/omnis-cli"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("feature/session-details"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Current") && line.contains("main"))
        );
        assert!(lines.iter().any(|line| line.contains("0123456789abcdef")));
        assert!(lines.iter().any(|line| line.contains("codex:session")));
        assert!(lines.iter().any(|line| line == "FIRST MESSAGE · USER"));
        assert!(lines.iter().any(|line| line.contains("cursor pagination")));
        assert!(
            lines
                .iter()
                .any(|line| line == "LATEST MESSAGE · ASSISTANT")
        );
        assert!(lines.iter().any(|line| line.contains("patch is ready")));
    }

    #[test]
    fn selected_detail_promotes_complete_tree_with_missing_parent() {
        let current = Path::new("/workspace");
        let selected = SessionRef::new(Provider::Codex, "selected");
        let child = SessionRef::new(Provider::OpenCode, "child");
        let missing = SessionRef::new(Provider::Claude, "missing-parent-session");
        let mut state = PickerState::new(
            vec![
                session(
                    Provider::Codex,
                    "selected",
                    current,
                    Some("Selected continuation"),
                ),
                session(
                    Provider::OpenCode,
                    "child",
                    current,
                    Some("Review continuation"),
                ),
            ],
            current,
            None,
            false,
        );
        let created_at = Utc::now();
        state.lineage.replace(vec![
            HandoffRecord {
                source: missing,
                target: selected.clone(),
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at,
            },
            HandoffRecord {
                source: selected,
                target: child,
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at: created_at + chrono::Duration::seconds(1),
            },
        ]);

        let lines = selected_detail_lines(&state, 72, 40)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();
        let tree = lines
            .iter()
            .position(|line| line == "SESSION TREE · 3 sessions · 3 agents")
            .expect("session tree");
        let workspace = lines
            .iter()
            .position(|line| line == "WORKSPACE")
            .expect("workspace");

        assert!(tree < workspace);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("claude:missing-pare… · not indexed"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("codex") && line.contains("Selected continuation"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("opencode") && line.contains("Review continuation"))
        );
    }

    #[test]
    fn preview_requests_cover_rendered_viewport_with_selected_first() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            (0..60)
                .map(|index| session(Provider::Claude, &format!("session-{index}"), current, None))
                .collect(),
            current,
            None,
            false,
        );
        state.selected = 30;
        let row_count = 17;
        let visible = state.visible_indices();
        let (first, _) = centered_list_window(visible.len(), state.selected, row_count);

        state.refresh_preview_window(row_count);
        assert!(state.due_preview_request().is_none());
        state.preview_deadline = Some(Instant::now());
        let request = state.due_preview_request().expect("preview request");

        assert_eq!(request.len(), row_count);
        assert_eq!(
            request[0].session,
            state.selected_entry().unwrap().session.session
        );
        for entry_index in visible.iter().skip(first).take(row_count) {
            let expected = &state.entries[*entry_index].session.session;
            assert!(request.iter().any(|key| &key.session == expected));
        }
        assert!(state.due_preview_request().is_none());
    }

    #[test]
    fn preview_requests_include_filtered_lineage_sessions() {
        let current = Path::new("/workspace");
        let parent = SessionRef::new(Provider::Claude, "parent");
        let child = SessionRef::new(Provider::Codex, "child");
        let mut state = PickerState::new(
            vec![
                session(Provider::Claude, "parent", current, Some("Parent context")),
                session(Provider::Codex, "child", current, Some("Matching child")),
            ],
            current,
            None,
            false,
        );
        state.lineage.replace(vec![HandoffRecord {
            source: parent.clone(),
            target: child,
            mode: omnis_ir::TransferMode::NativeMaterialization,
            created_at: Utc::now(),
        }]);
        state.query = "matching child".to_owned();

        state.refresh_preview_window(10);
        state.preview_deadline = Some(Instant::now());
        let request = state.due_preview_request().expect("preview request");

        assert_eq!(state.visible_indices().len(), 1);
        assert!(request.iter().any(|key| key.session == parent));
        assert_eq!(
            request[0].session,
            state.selected_entry().unwrap().session.session
        );
    }

    #[test]
    fn preview_cache_keeps_every_active_viewport_entry() {
        let protected = (0..PREVIEW_CACHE_CAPACITY + 2)
            .map(|index| PreviewKey {
                session: SessionRef::new(Provider::Codex, format!("session-{index}")),
                updated_at: None,
                continuation_after: None,
            })
            .collect::<Vec<_>>();
        let mut cache = PreviewCache::default();

        for key in &protected {
            cache.insert(key.clone(), PreviewValue::Unavailable, &protected);
        }
        cache.insert(
            PreviewKey {
                session: SessionRef::new(Provider::Codex, "offscreen"),
                updated_at: None,
                continuation_after: None,
            },
            PreviewValue::Unavailable,
            &protected,
        );

        assert_eq!(cache.values.len(), protected.len());
        assert!(protected.iter().all(|key| cache.contains(key)));
    }

    #[test]
    fn list_title_never_falls_back_to_session_id() {
        let session = session(
            Provider::Codex,
            "019f4e2b-2c98-7f03-8e2d-5f229240bba1",
            Path::new("/workspace"),
            None,
        );
        let columns = ListColumns::for_width(100);

        let loading = session_line(&session, false, "", &columns, None);
        let unavailable = PreviewValue::Unavailable;
        let untitled = session_line(&session, false, "", &columns, Some(&unavailable));

        assert!(loading.contains("Loading title…"));
        assert!(untitled.contains("Untitled session"));
        assert!(!loading.contains("019f4e2b"));
        assert!(!untitled.contains("019f4e2b"));
    }

    #[test]
    fn list_title_uses_first_message_preview() {
        let session = session(Provider::Codex, "session-id", Path::new("/workspace"), None);
        let preview = PreviewValue::Ready {
            preview: Box::new(SessionPreview {
                first: Some(HandoffMessage {
                    role: HandoffRole::User,
                    text: "Fix pagination without changing the API".to_owned(),
                }),
                latest: None,
                message_count: 1,
                workspace_root: None,
                current_dir: None,
                git_branch: None,
                git_head: None,
            }),
            continuation: None,
        };

        let line = session_line(
            &session,
            false,
            "",
            &ListColumns::for_width(100),
            Some(&preview),
        );

        assert!(line.contains("Fix pagination without changing the API"));
        assert!(!line.contains("session-id"));
    }

    #[test]
    fn continuation_message_replaces_import_placeholder_title() {
        let session = session(
            Provider::OpenCode,
            "target",
            Path::new("/workspace"),
            Some("Imported from codex:source"),
        );
        let preview = PreviewValue::Ready {
            preview: Box::new(SessionPreview {
                first: Some(HandoffMessage {
                    role: HandoffRole::User,
                    text: "Inherited question".to_owned(),
                }),
                latest: Some(HandoffMessage {
                    role: HandoffRole::Assistant,
                    text: "Implemented the fix".to_owned(),
                }),
                message_count: 3,
                workspace_root: None,
                current_dir: None,
                git_branch: None,
                git_head: None,
            }),
            continuation: Some(HandoffMessage {
                role: HandoffRole::User,
                text: "Fix the retry race after this fork".to_owned(),
            }),
        };

        let line = session_line(
            &session,
            false,
            "└─ ",
            &ListColumns::for_width(120),
            Some(&preview),
        );

        assert!(line.contains("Fix the retry race after this fork"));
        assert!(!line.contains("Imported from"));
    }

    #[test]
    fn search_stays_available_while_index_builds_off_thread() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            vec![
                session(Provider::Codex, "auth", current, Some("Auth refactor")),
                session(
                    Provider::Claude,
                    "billing",
                    Path::new("/workspace/billing"),
                    Some("Billing fix"),
                ),
            ],
            current,
            None,
            true,
        );
        state.query = "billing".to_owned();
        assert_eq!(state.visible_indices().len(), 1);

        state.search_index_deadline = Some(Instant::now());
        let request = state.due_search_index_request().expect("search request");
        let generation = request.generation;
        let index = InvertedIndex::build(&request.values);
        assert_eq!(generation, state.entries_generation);
        state.search_index = Some(index);

        assert_eq!(state.visible_indices().len(), 1);
        assert_eq!(state.selected_entry().expect("match").key, "claude:billing");
    }

    #[test]
    fn trajectory_matches_extend_metadata_search_results() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            vec![
                session(Provider::Codex, "auth", current, Some("Auth refactor")),
                session(Provider::Claude, "billing", current, Some("Billing fix")),
            ],
            current,
            None,
            false,
        );
        state.query = "connection pool".to_owned();
        state.query_changed();
        state.search_index = Some(InvertedIndex::build(
            &state
                .entries
                .iter()
                .map(|entry| entry.search.clone())
                .collect::<Vec<_>>(),
        ));
        state.trajectory_matches.insert("claude:billing".to_owned());

        let visible = state.visible_indices();

        assert_eq!(visible.len(), 1);
        assert_eq!(state.entries[visible[0]].key, "claude:billing");
    }

    #[test]
    fn trajectory_match_refresh_preserves_selected_session() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            (0..20)
                .map(|index| {
                    let title = format!("Needle session {index}");
                    session(
                        Provider::Codex,
                        &format!("session-{index}"),
                        current,
                        Some(&title),
                    )
                })
                .collect(),
            current,
            None,
            false,
        );
        state.query = "needle".to_owned();
        state.selected = 12;
        let selected = state
            .selected_entry()
            .expect("selected session")
            .key
            .clone();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PickerUpdate::TrajectorySearch {
                generation: state.trajectory_search_generation,
                result: Ok(Vec::new()),
            })
            .unwrap();

        assert!(receive_updates(
            &receiver,
            &mut state,
            &mut HashSet::new(),
            &mut Vec::new(),
            current,
        ));

        assert_eq!(
            state.selected_entry().expect("selected session").key,
            selected
        );
    }

    #[test]
    fn trajectory_search_is_debounced_and_generation_scoped() {
        let mut state = PickerState::new(Vec::new(), Path::new("/workspace"), None, true);
        state.query = "database lock".to_owned();
        state.query_changed();

        assert!(state.due_trajectory_search_request().is_none());
        state.trajectory_search_deadline = Some(Instant::now());
        let request = state
            .due_trajectory_search_request()
            .expect("trajectory request");

        assert_eq!(request.query, "database lock");
        assert_eq!(request.generation, state.trajectory_search_generation);
        assert!(state.trajectory_search_pending);
    }

    #[test]
    fn provider_refresh_replaces_cached_rows_without_resetting_other_providers() {
        let current = Path::new("/workspace");
        let mut state = PickerState::new(
            vec![
                session(Provider::Claude, "stale", current, None),
                session(Provider::Codex, "codex", current, None),
            ],
            current,
            None,
            false,
        );

        state.replace_provider(
            Provider::Claude,
            vec![session(Provider::Claude, "fresh", current, None)],
            current,
            false,
        );

        assert_eq!(state.entries.len(), 2);
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.key == "claude:fresh")
        );
        assert!(state.entries.iter().any(|entry| entry.key == "codex:codex"));
        assert!(
            state
                .entries
                .iter()
                .all(|entry| entry.key != "claude:stale")
        );
    }

    #[test]
    fn lineage_graph_renders_complete_branching_tree() {
        let source = SessionRef::new(Provider::Claude, "source");
        let middle = SessionRef::new(Provider::Codex, "middle");
        let target = SessionRef::new(Provider::CursorCli, "target");
        let child = SessionRef::new(Provider::Grok, "child");
        let sibling = SessionRef::new(Provider::OpenCode, "sibling");
        let created_at = Utc::now();
        let record = |source, target, seconds| HandoffRecord {
            source,
            target,
            mode: omnis_ir::TransferMode::NativeMaterialization,
            created_at: created_at + chrono::Duration::seconds(seconds),
        };
        let mut graph = LineageGraph::default();
        graph.replace(vec![
            record(source.clone(), middle.clone(), 0),
            record(middle.clone(), target.clone(), 1),
            record(target.clone(), child, 2),
            record(middle, sibling, 3),
        ]);

        assert_eq!(graph.list_prefix(&source), "┬─ ");
        assert_eq!(graph.list_prefix(&target), "   ├─ ");
        assert_eq!(
            graph.list_prefix(&SessionRef::new(Provider::Grok, "child")),
            "   │  └─ "
        );
        assert_eq!(
            graph.tree(&target),
            vec![
                LineageTreeNode {
                    session: source,
                    branch: String::new(),
                    selected: false,
                },
                LineageTreeNode {
                    session: SessionRef::new(Provider::Codex, "middle"),
                    branch: "└─ ".to_owned(),
                    selected: false,
                },
                LineageTreeNode {
                    session: target,
                    branch: "   ├─ ".to_owned(),
                    selected: true,
                },
                LineageTreeNode {
                    session: SessionRef::new(Provider::Grok, "child"),
                    branch: "   │  └─ ".to_owned(),
                    selected: false,
                },
                LineageTreeNode {
                    session: SessionRef::new(Provider::OpenCode, "sibling"),
                    branch: "   └─ ".to_owned(),
                    selected: false,
                },
            ]
        );
    }

    #[test]
    fn lineage_update_groups_sessions_without_changing_selection() {
        let current = Path::new("/workspace");
        let root = SessionRef::new(Provider::Claude, "root");
        let first_child = SessionRef::new(Provider::Codex, "first-child");
        let selected_child = SessionRef::new(Provider::OpenCode, "selected-child");
        let mut state = PickerState::new(
            vec![
                session(
                    Provider::OpenCode,
                    "selected-child",
                    current,
                    Some("Newest continuation"),
                ),
                session(
                    Provider::Grok,
                    "unrelated",
                    current,
                    Some("Unrelated session"),
                ),
                session(Provider::Claude, "root", current, Some("Original session")),
                session(
                    Provider::Codex,
                    "first-child",
                    current,
                    Some("First continuation"),
                ),
            ],
            current,
            None,
            false,
        );
        let created_at = Utc::now();

        state.replace_lineage(vec![
            HandoffRecord {
                source: root.clone(),
                target: first_child.clone(),
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at,
            },
            HandoffRecord {
                source: root,
                target: selected_child.clone(),
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at: created_at + chrono::Duration::seconds(1),
            },
        ]);

        let visible = state
            .visible_indices()
            .into_iter()
            .map(|index| state.entries[index].session.session.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            vec![
                SessionRef::new(Provider::Claude, "root"),
                first_child,
                selected_child.clone(),
                SessionRef::new(Provider::Grok, "unrelated"),
            ]
        );
        assert_eq!(
            state.selected_entry().unwrap().session.session,
            selected_child
        );
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn session_line_indents_linked_agent() {
        let session = session(
            Provider::OpenCode,
            "child",
            Path::new("/workspace"),
            Some("Linked continuation"),
        );
        let columns = ListColumns::for_width(120);

        let line = session_line(&session, false, "└─ ", &columns, None);

        assert!(line.contains("└─ opencode"));
        assert_eq!(UnicodeWidthStr::width(line.as_str()), 120);
    }

    #[test]
    fn lineage_graph_is_cycle_safe() {
        let first = SessionRef::new(Provider::Claude, "first");
        let second = SessionRef::new(Provider::Codex, "second");
        let mut graph = LineageGraph::default();
        graph.replace(vec![
            HandoffRecord {
                source: first.clone(),
                target: second.clone(),
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at: Utc::now(),
            },
            HandoffRecord {
                source: second,
                target: first.clone(),
                mode: omnis_ir::TransferMode::NativeMaterialization,
                created_at: Utc::now(),
            },
        ]);

        let tree = graph.tree(&first);

        assert_eq!(tree.len(), 2);
        assert_eq!(tree.iter().filter(|node| node.selected).count(), 1);
    }

    #[test]
    fn provider_failure_keeps_cached_snapshot() {
        let current = Path::new("/workspace");
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PickerUpdate::Cached(Ok(picker_entries(
                vec![session(
                    Provider::Claude,
                    "cached",
                    current,
                    Some("Cached session"),
                )],
                current,
                true,
            ))))
            .expect("send cache");
        sender
            .send(PickerUpdate::Discovered(DiscoveryUpdate {
                provider: Provider::Claude,
                result: Err(anyhow::anyhow!("provider unavailable")),
            }))
            .expect("send failure");

        let mut state = PickerState::new(Vec::new(), current, None, false);
        let mut pending = HashSet::from([Provider::Claude]);
        let mut warnings = Vec::new();
        assert!(receive_updates(
            &receiver,
            &mut state,
            &mut pending,
            &mut warnings,
            current,
        ));
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].key, "claude:cached");
        assert!(state.entries[0].cached);
        assert!(!pending.contains(&Provider::Claude));
    }

    #[test]
    fn provider_success_replaces_cached_snapshot() {
        let current = Path::new("/workspace");
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PickerUpdate::Cached(Ok(picker_entries(
                vec![session(
                    Provider::Claude,
                    "cached",
                    current,
                    Some("Cached session"),
                )],
                current,
                true,
            ))))
            .expect("send cache");
        sender
            .send(PickerUpdate::Discovered(DiscoveryUpdate {
                provider: Provider::Claude,
                result: Ok(picker_entries(
                    vec![session(
                        Provider::Claude,
                        "fresh",
                        current,
                        Some("Fresh session"),
                    )],
                    current,
                    false,
                )),
            }))
            .expect("send success");

        let mut state = PickerState::new(Vec::new(), current, None, false);
        let mut pending = HashSet::from([Provider::Claude]);
        let mut warnings = Vec::new();
        assert!(receive_updates(
            &receiver,
            &mut state,
            &mut pending,
            &mut warnings,
            current,
        ));
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].key, "claude:fresh");
        assert!(!state.entries[0].cached);
    }

    #[test]
    fn recent_provider_check_reuses_cached_snapshot() {
        assert!(session_cache_is_fresh(Utc::now()));
        assert!(!session_cache_is_fresh(
            Utc::now() - chrono::Duration::seconds(16)
        ));
    }

    #[test]
    fn target_selection_defaults_to_runnable_source() {
        let targets = [Provider::Claude, Provider::Codex, Provider::OpenCode];
        let choices = target_choices(Provider::Codex, &targets);

        assert_eq!(choices.len(), 4);
        assert_eq!(choices[1].provider, Provider::Codex);
        assert!(!choices[1].fork);
        assert_eq!(choices[2].provider, Provider::Codex);
        assert!(choices[2].fork);
        assert_eq!(default_target_index(Provider::Codex, &choices), Some(1));
        assert_eq!(default_target_index(Provider::Grok, &choices), None);
        assert_eq!(move_target_selection(None, choices.len(), true), Some(0));
        assert_eq!(move_target_selection(None, choices.len(), false), Some(3));
        assert_eq!(move_target_selection(Some(3), choices.len(), true), Some(0));
    }

    #[test]
    fn workspace_path_completion_finds_directories() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(temporary.path().join("alpha")).expect("create directory");
        fs::write(temporary.path().join("alphabet.txt"), "ignored").expect("create file");
        let input = temporary.path().join("alp").display().to_string();

        let completion = complete_directory_input(&input, temporary.path());

        assert_eq!(
            completion.value,
            format!(
                "{}{MAIN_SEPARATOR}",
                temporary.path().join("alpha").display()
            )
        );
        assert!(completion.message.is_empty());
    }

    #[test]
    fn workspace_branch_reader_handles_git_metadata_without_spawning_git() {
        let temporary = tempfile::tempdir().expect("temporary repository");
        let git_dir = temporary.path().join(".git");
        fs::create_dir(&git_dir).expect("git directory");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/sidebar\n").expect("git HEAD");

        assert_eq!(
            workspace_git_branch(temporary.path()).as_deref(),
            Some("feature/sidebar")
        );
    }

    fn session(provider: Provider, id: &str, project: &Path, title: Option<&str>) -> NativeSession {
        NativeSession {
            session: SessionRef::new(provider, id),
            title: title.map(str::to_owned),
            project_path: Some(project.to_path_buf()),
            git_branch: Some("main".to_owned()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            event_count: 0,
            source_path: None,
        }
    }
}
