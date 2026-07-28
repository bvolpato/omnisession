use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{MAIN_SEPARATOR, Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use crossterm::{
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
use omnis_core::safe_terminal_line;
use omnis_ir::{Provider, SessionRef};
use omnis_store::{HandoffRecord, IndexedSession, Store};

use crate::PROVIDERS;

pub struct PickerSelection {
    pub session: SessionRef,
    pub project_path: Option<PathBuf>,
    pub across_projects: bool,
    pub target: Provider,
    pub live: bool,
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
    search_index: InvertedIndex,
    query: String,
    provider_index: usize,
    all_projects: bool,
    selected: usize,
    lineage: LineageGraph,
}

impl PickerState {
    fn new(
        sessions: Vec<NativeSession>,
        current_project: &Path,
        initial_provider: Option<Provider>,
        all_projects: bool,
    ) -> Self {
        let entries: Vec<PickerEntry> = sessions
            .into_iter()
            .map(|session| picker_entry(session, current_project, true))
            .collect();
        let search_index = InvertedIndex::build(&entries);
        let provider_index = initial_provider
            .and_then(|provider| {
                PROVIDERS
                    .iter()
                    .position(|candidate| *candidate == provider)
            })
            .map_or(0, |index| index + 1);
        Self {
            entries,
            search_index,
            query: String::new(),
            provider_index,
            all_projects,
            selected: 0,
            lineage: LineageGraph::default(),
        }
    }

    fn provider(&self) -> Option<Provider> {
        self.provider_index
            .checked_sub(1)
            .and_then(|index| PROVIDERS.get(index).copied())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let candidates = self
            .search_index
            .candidates(&query)
            .unwrap_or_else(|| (0..self.entries.len()).collect());
        candidates
            .into_iter()
            .filter(|index| self.all_projects || self.entries[*index].current_workspace)
            .filter(|index| {
                self.provider().is_none_or(|provider| {
                    self.entries[*index].session.session.provider == provider
                })
            })
            .filter(|index| query.is_empty() || self.entries[*index].search.contains(&query))
            .collect()
    }

    fn replace_provider(
        &mut self,
        provider: Provider,
        sessions: Vec<NativeSession>,
        current_project: &Path,
        cached: bool,
    ) {
        let selected_key = self.selected_entry().map(|entry| entry.key.clone());
        self.entries
            .retain(|entry| entry.session.session.provider != provider);
        self.entries.extend(
            sessions
                .into_iter()
                .map(|session| picker_entry(session, current_project, cached)),
        );
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.session.updated_at));
        self.search_index = InvertedIndex::build(&self.entries);
        let visible = self.visible_indices();
        self.selected = selected_key
            .and_then(|key| {
                visible
                    .iter()
                    .position(|index| self.entries[*index].key == key)
            })
            .unwrap_or_else(|| self.selected.min(visible.len().saturating_sub(1)));
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
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
}

#[derive(Default)]
struct LineageGraph {
    parents: HashMap<SessionRef, SessionRef>,
    children: HashMap<SessionRef, Vec<SessionRef>>,
}

impl LineageGraph {
    fn replace(&mut self, records: Vec<HandoffRecord>) {
        self.parents.clear();
        self.children.clear();
        for record in records {
            self.parents.insert(record.target, record.source);
        }
        for (target, source) in &self.parents {
            self.children
                .entry(source.clone())
                .or_default()
                .push(target.clone());
        }
        for children in self.children.values_mut() {
            children.sort_by_key(ToString::to_string);
            children.dedup();
        }
    }

    fn has_parent(&self, session: &SessionRef) -> bool {
        self.parents.contains_key(session)
    }

    fn lines(&self, selected: &SessionRef, limit: usize) -> Vec<String> {
        if limit == 0
            || !self.parents.contains_key(selected) && !self.children.contains_key(selected)
        {
            return Vec::new();
        }
        let mut chain = vec![selected.clone()];
        let mut visited = HashSet::from([selected.clone()]);
        while let Some(parent) = self.parents.get(chain.last().expect("lineage chain")) {
            if !visited.insert(parent.clone()) {
                break;
            }
            chain.push(parent.clone());
        }
        chain.reverse();

        let mut lines = Vec::new();
        let chain_budget = if chain.len() > limit {
            limit - 1
        } else {
            limit
        };
        let skipped = chain.len().saturating_sub(chain_budget);
        if skipped > 0 {
            lines.push(format!("  ... {skipped} earlier"));
        }
        for (depth, session) in chain.into_iter().skip(skipped).enumerate() {
            let connector = if depth == 0 && skipped == 0 {
                ""
            } else {
                "└─ "
            };
            let marker = if session == *selected {
                "  selected"
            } else {
                ""
            };
            lines.push(format!(
                "  {}{connector}{}{marker}",
                "  ".repeat(depth),
                short_session_ref(&session)
            ));
        }
        if lines.len() < limit {
            if let Some(children) = self.children.get(selected) {
                let remaining = limit - lines.len();
                let indent = "  ".repeat(lines.len() + usize::from(skipped == 0));
                for (index, child) in children.iter().take(remaining).enumerate() {
                    let connector = if index + 1 == children.len().min(remaining) {
                        "└─"
                    } else {
                        "├─"
                    };
                    lines.push(format!("{indent}{connector} {}", short_session_ref(child)));
                }
            }
        }
        lines
    }
}

fn picker_entry(session: NativeSession, current_project: &Path, cached: bool) -> PickerEntry {
    PickerEntry {
        key: session.session.to_string(),
        current_workspace: session
            .project_path
            .as_deref()
            .is_some_and(|path| paths_match(path, current_project)),
        search: search_text(&session),
        session,
        cached,
    }
}

#[derive(Default)]
struct InvertedIndex {
    postings: HashMap<String, Vec<usize>>,
}

impl InvertedIndex {
    fn build(entries: &[PickerEntry]) -> Self {
        let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            for trigram in trigrams(&entry.search) {
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

fn trigrams(value: &str) -> HashSet<String> {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() < 3 {
        return HashSet::new();
    }
    characters
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
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
    let receiver = spawn_updates();
    let mut pending = PROVIDERS.into_iter().collect::<HashSet<_>>();
    let mut successful = HashSet::new();
    let mut warnings = Vec::new();
    let mut state = PickerState::new(Vec::new(), current_project, initial_provider, all_projects);
    render(&state, target, warnings.len(), pending.len())?;

    let mut dirty = true;
    loop {
        dirty |= receive_updates(
            &receiver,
            &mut state,
            &mut pending,
            &mut successful,
            &mut warnings,
            current_project,
        );
        if dirty {
            render(&state, target, warnings.len(), pending.len())?;
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
                let live = !entry.cached;
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
                    |target| Ok(TargetOutcome::Selected(target)),
                )?;
                let target = match selected_target {
                    TargetOutcome::Selected(target) => target,
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
                    target,
                    live,
                    workspace_override,
                }));
            }
        }
    }
}

struct DiscoveryUpdate {
    provider: Provider,
    result: anyhow::Result<Vec<NativeSession>>,
}

enum PickerUpdate {
    Cached {
        provider: Provider,
        result: Result<Vec<IndexedSession>, String>,
    },
    Lineage(Result<Vec<HandoffRecord>, String>),
    Discovered(DiscoveryUpdate),
    Warning(String),
}

fn spawn_updates() -> Receiver<PickerUpdate> {
    let (sender, receiver) = mpsc::channel();
    let (index_sender, index_receiver) = mpsc::channel::<(Provider, Vec<IndexedSession>)>();
    let cache_sender = sender.clone();
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = cache_sender.send(PickerUpdate::Warning(
                "session index is unavailable".to_owned(),
            ));
            return;
        };
        for provider in PROVIDERS {
            let result = store
                .indexed_sessions_for_provider(provider)
                .map_err(|error| error.to_string());
            if cache_sender
                .send(PickerUpdate::Cached { provider, result })
                .is_err()
            {
                return;
            }
        }
        let lineage = store.handoff_lineage().map_err(|error| error.to_string());
        let _ = cache_sender.send(PickerUpdate::Lineage(lineage));
    });
    let warning_sender = sender.clone();
    thread::spawn(move || {
        let Ok(store) = Store::open_default() else {
            let _ = warning_sender.send(PickerUpdate::Warning(
                "session index writer is unavailable".to_owned(),
            ));
            return;
        };
        while let Ok((provider, indexed)) = index_receiver.recv() {
            if let Err(error) = store.replace_indexed_sessions(provider, &indexed) {
                let _ =
                    warning_sender.send(PickerUpdate::Warning(format!("session index: {error}")));
            }
        }
    });
    for provider in PROVIDERS {
        let sender = sender.clone();
        let index_sender = index_sender.clone();
        thread::spawn(move || {
            let registry = AdapterRegistry::with_local_adapters();
            let result = registry.list_sessions(provider, None);
            match result {
                Ok(sessions) => {
                    let indexed = sessions.iter().map(indexed_session).collect::<Vec<_>>();
                    if sender
                        .send(PickerUpdate::Discovered(DiscoveryUpdate {
                            provider,
                            result: Ok(sessions),
                        }))
                        .is_err()
                    {
                        return;
                    }
                    let _ = index_sender.send((provider, indexed));
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
    receiver
}

fn receive_updates(
    receiver: &Receiver<PickerUpdate>,
    state: &mut PickerState,
    pending: &mut HashSet<Provider>,
    successful: &mut HashSet<Provider>,
    warnings: &mut Vec<String>,
    current_project: &Path,
) -> bool {
    let mut changed = false;
    while let Ok(update) = receiver.try_recv() {
        changed = true;
        match update {
            PickerUpdate::Cached {
                provider,
                result: Ok(indexed),
            } => {
                if !successful.contains(&provider) {
                    let sessions = indexed.into_iter().map(native_session).collect();
                    state.replace_provider(provider, sessions, current_project, true);
                }
            }
            PickerUpdate::Cached {
                result: Err(error), ..
            }
            | PickerUpdate::Warning(error) => {
                warnings.push(error);
            }
            PickerUpdate::Lineage(Ok(records)) => state.lineage.replace(records),
            PickerUpdate::Lineage(Err(error)) => {
                warnings.push(format!("session lineage: {error}"));
            }
            PickerUpdate::Discovered(update) => {
                pending.remove(&update.provider);
                match update.result {
                    Ok(sessions) => {
                        successful.insert(update.provider);
                        state.replace_provider(update.provider, sessions, current_project, false);
                    }
                    Err(error) => warnings.push(format!("{}: {error}", update.provider)),
                }
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
    Selected(Provider),
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
    let mut selected = default_target_index(source.provider, targets);
    loop {
        render_target(source, targets, selected)?;
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
                if let Some(target) = selected.and_then(|index| targets.get(index)).copied() {
                    return Ok(TargetOutcome::Selected(target));
                }
            }
            KeyCode::Up | KeyCode::Left => {
                selected = move_target_selection(selected, targets.len(), false);
            }
            KeyCode::Down | KeyCode::Right => {
                selected = move_target_selection(selected, targets.len(), true);
            }
            _ => {}
        }
    }
}

fn default_target_index(source: Provider, targets: &[Provider]) -> Option<usize> {
    targets.iter().position(|provider| *provider == source)
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

fn render_target(source: &SessionRef, targets: &[Provider], selected: Option<usize>) -> Result<()> {
    let (width, _) = terminal::size().context("reading terminal size")?;
    let width = usize::from(width).max(1);
    let mut output = io::stdout().lock();
    queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        output,
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
    for (index, target) in targets.iter().enumerate() {
        let is_selected = selected == Some(index);
        if is_selected {
            queue!(
                output,
                SetForegroundColor(Color::Green),
                SetAttribute(Attribute::Bold)
            )?;
        }
        let action = if *target == source.provider {
            "Continue original session"
        } else {
            "Open continuation in this agent"
        };
        let marker = if is_selected { "›" } else { " " };
        queue!(
            output,
            Print(truncate(&format!("{marker} {target}"), width)),
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
            output,
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
        output,
        Print("\r\n\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "↑↓ choose  Enter continue  Esc back  Ctrl-C cancel",
            width
        )),
        ResetColor
    )?;
    output.flush().context("drawing target picker")
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
    let mut output = io::stdout().lock();
    queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        output,
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
            output,
            SetForegroundColor(Color::Yellow),
            Print(truncate(message, width)),
            ResetColor,
            Print("\r\n")
        )?;
    }
    queue!(
        output,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "Tab complete  Enter use folder  Ctrl-U clear  Esc back  Ctrl-C cancel",
            width
        )),
        ResetColor
    )?;
    output.flush().context("drawing workspace picker")
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
            state.reset_selection();
            PickerAction::Continue
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.query.clear();
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
            state.reset_selection();
            PickerAction::Continue
        }
        _ => PickerAction::Continue,
    }
}

fn render(
    state: &PickerState,
    target: Option<Provider>,
    warning_count: usize,
    pending_count: usize,
) -> Result<()> {
    let (width, height) = terminal::size().context("reading terminal size")?;
    let width = usize::from(width).max(1);
    let height = usize::from(height).max(1);
    let visible = state.visible_indices();
    let lineage_height = state
        .selected_entry()
        .map(|entry| state.lineage.lines(&entry.session.session, 4).len())
        .filter(|height| *height > 0)
        .map_or(0, |height| height + 1);
    let list_height = height.saturating_sub(13 + lineage_height).max(1);
    let selected = state.selected.min(visible.len().saturating_sub(1));
    let first = selected.saturating_sub(list_height.saturating_sub(1));
    let mut output = io::stdout().lock();
    queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    render_header(&mut output, state, target, visible.len(), width)?;
    render_session_rows(
        &mut output,
        state,
        &visible,
        ListViewport {
            first,
            selected,
            height: list_height,
            width,
            pending_count,
        },
    )?;
    render_footer(&mut output, state, warning_count, pending_count, width)?;
    output.flush().context("drawing session picker")
}

fn render_header(
    output: &mut impl Write,
    state: &PickerState,
    target: Option<Provider>,
    match_count: usize,
    width: usize,
) -> Result<()> {
    let count = if match_count == 1 {
        "1 match".to_owned()
    } else {
        format!("{match_count} matches")
    };
    queue!(
        output,
        SetAttribute(Attribute::Bold),
        Print(truncate(
            &format!("OmniSession  Session browser  ·  {count}"),
            width
        )),
        SetAttribute(Attribute::Reset),
        Print("\r\n")
    )?;
    let enter_action = if target.is_some() {
        "Enter resume"
    } else {
        "Enter continue"
    };
    let target = target.map_or_else(
        || "choose after source".to_owned(),
        |provider| provider.to_string(),
    );
    queue!(
        output,
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            &format!("Target: {target}  {enter_action}  Esc cancel"),
            width
        )),
        ResetColor,
        Print("\r\n\r\n")
    )?;

    let scope = if state.all_projects {
        "all workspaces"
    } else {
        "current workspace"
    };
    let provider = state
        .provider()
        .map_or_else(|| "all sources".to_owned(), |provider| provider.to_string());
    queue!(
        output,
        SetForegroundColor(Color::Cyan),
        Print(truncate(
            &format!("Scope: {scope}  [Tab]    Source: {provider}  [←/→]"),
            width
        )),
        ResetColor,
        Print("\r\n")
    )?;
    let query = if state.query.is_empty() {
        "title, session ID, directory, or branch".to_owned()
    } else {
        state.query.clone()
    };
    queue!(
        output,
        Print(truncate(&format!("Search: {query}"), width)),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "  AGENT       SESSION / TITLE                     DIRECTORY          AGE",
            width
        )),
        Print("\r\n"),
        Print("─".repeat(width)),
        ResetColor,
        Print("\r\n")
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
struct ListViewport {
    first: usize,
    selected: usize,
    height: usize,
    width: usize,
    pending_count: usize,
}

fn render_session_rows(
    output: &mut impl Write,
    state: &PickerState,
    visible: &[usize],
    viewport: ListViewport,
) -> Result<()> {
    if visible.is_empty() {
        let hint = if viewport.pending_count > 0 {
            "Scanning provider stores... results appear as they arrive."
        } else if state.all_projects {
            "No matching sessions. Clear search or change source provider."
        } else {
            "No matching sessions here. Press Tab to search all workspaces."
        };
        queue!(
            output,
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(hint, viewport.width)),
            ResetColor,
            Print("\r\n")
        )?;
    } else {
        for (row, entry_index) in visible
            .iter()
            .skip(viewport.first)
            .take(viewport.height)
            .enumerate()
        {
            let entry = &state.entries[*entry_index].session;
            let is_selected = viewport.first + row == viewport.selected;
            if is_selected {
                queue!(
                    output,
                    SetForegroundColor(Color::Green),
                    SetAttribute(Attribute::Bold)
                )?;
            }
            queue!(
                output,
                Print(session_line(
                    entry,
                    is_selected,
                    state.lineage.has_parent(&entry.session),
                    viewport.width,
                )),
                ResetColor,
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
        }
    }

    let rendered_rows = visible.len().min(viewport.height);
    for _ in rendered_rows..viewport.height {
        queue!(output, Print("\r\n"))?;
    }
    queue!(output, Print("\r\n"))?;
    Ok(())
}

fn render_footer(
    output: &mut impl Write,
    state: &PickerState,
    warning_count: usize,
    pending_count: usize,
    width: usize,
) -> Result<()> {
    if let Some(entry) = state.selected_entry() {
        let workspace = entry.session.project_path.as_deref().map_or_else(
            || "unknown workspace".to_owned(),
            |path| path.display().to_string(),
        );
        let branch = entry
            .session
            .git_branch
            .as_deref()
            .map_or_else(|| "unknown branch".to_owned(), safe_terminal_line);
        let cache_state = if entry.cached { " · cached" } else { "" };
        queue!(
            output,
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(&entry.session.session.to_string(), width)),
            Print("\r\n"),
            Print(truncate(
                &format!("{workspace} · {branch}{cache_state}"),
                width
            )),
            ResetColor,
            Print("\r\n")
        )?;
        let lineage = state.lineage.lines(&entry.session.session, 4);
        if !lineage.is_empty() {
            queue!(output, Print(truncate("Lineage", width)), Print("\r\n"))?;
            for line in lineage {
                queue!(output, Print(truncate(&line, width)), Print("\r\n"))?;
            }
        }
    } else {
        queue!(output, Print("\r\n\r\n"))?;
    }
    let warning = if pending_count > 0 {
        format!(
            "Refreshing {pending_count} source(s) · ↑↓ move  Tab all  ←/→ source  Enter continue"
        )
    } else if warning_count == 0 {
        "↑↓ move  PgUp/PgDn jump  Backspace edit  Enter continue  Esc cancel".to_owned()
    } else {
        format!("↑↓ move  PgUp/PgDn jump  {warning_count} provider warning(s); run `omnis doctor`")
    };
    queue!(
        output,
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(&warning, width)),
        ResetColor
    )?;
    Ok(())
}

fn session_line(session: &NativeSession, selected: bool, inherited: bool, width: usize) -> String {
    let marker = if selected {
        "›"
    } else if inherited {
        "↳"
    } else {
        " "
    };
    let provider = session.session.provider.to_string();
    let raw_title = session
        .title
        .as_deref()
        .map(safe_terminal_line)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| short_id(&session.session.id));
    let raw_project = session
        .project_path
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map_or_else(|| "unknown".to_owned(), safe_terminal_line);
    let age = relative_time(session.updated_at);
    let project_width = 18.min(width / 4).max(8);
    let fixed_width = 2 + 12 + project_width + age.chars().count() + 2;
    let title_width = width.saturating_sub(fixed_width).max(8);
    let title = truncate(&raw_title, title_width);
    let project = truncate(&raw_project, project_width);
    truncate(
        &format!("{marker} {provider:<11} {title:<title_width$} {project:<project_width$} {age}"),
        width,
    )
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
        session.title.as_deref().unwrap_or_default(),
        session
            .project_path
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string()),
        session.git_branch.as_deref().unwrap_or_default()
    )
    .to_lowercase()
    .chars()
    .take(4096)
    .collect()
}

fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .is_ok_and(|left| right.canonicalize().is_ok_and(|right| left == right))
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
    if value.chars().count() <= width {
        return value;
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut truncated = value.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
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
    fn lineage_graph_marks_inherited_sessions_and_renders_ancestry() {
        let source = SessionRef::new(Provider::Claude, "source");
        let middle = SessionRef::new(Provider::Codex, "middle");
        let target = SessionRef::new(Provider::CursorCli, "target");
        let child = SessionRef::new(Provider::Grok, "child");
        let record = |source, target| HandoffRecord {
            source,
            target,
            mode: omnis_ir::TransferMode::NativeMaterialization,
            created_at: Utc::now(),
        };
        let mut graph = LineageGraph::default();
        graph.replace(vec![
            record(source.clone(), middle.clone()),
            record(middle.clone(), target.clone()),
            record(target.clone(), child),
        ]);

        assert!(graph.has_parent(&target));
        assert_eq!(
            graph.lines(&target, 4),
            vec![
                "  claude:source",
                "    └─ codex:middle",
                "      └─ cursor-cli:target  selected",
                "        └─ grok:child",
            ]
        );
    }

    #[test]
    fn provider_failure_keeps_cache_in_either_update_order() {
        let current = Path::new("/workspace");
        for cache_first in [true, false] {
            let (sender, receiver) = mpsc::channel();
            let cached = indexed_session(&session(
                Provider::Claude,
                "cached",
                current,
                Some("Cached session"),
            ));
            let send_cache = || {
                sender
                    .send(PickerUpdate::Cached {
                        provider: Provider::Claude,
                        result: Ok(vec![cached.clone()]),
                    })
                    .expect("send cache");
            };
            let send_failure = || {
                sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider: Provider::Claude,
                        result: Err(anyhow::anyhow!("provider unavailable")),
                    }))
                    .expect("send failure");
            };
            if cache_first {
                send_cache();
                send_failure();
            } else {
                send_failure();
                send_cache();
            }

            let mut state = PickerState::new(Vec::new(), current, None, false);
            let mut pending = HashSet::from([Provider::Claude]);
            let mut successful = HashSet::new();
            let mut warnings = Vec::new();
            assert!(receive_updates(
                &receiver,
                &mut state,
                &mut pending,
                &mut successful,
                &mut warnings,
                current,
            ));
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.entries[0].key, "claude:cached");
            assert!(state.entries[0].cached);
            assert!(!pending.contains(&Provider::Claude));
            assert!(!successful.contains(&Provider::Claude));
        }
    }

    #[test]
    fn provider_success_wins_over_cache_in_either_update_order() {
        let current = Path::new("/workspace");
        for cache_first in [true, false] {
            let (sender, receiver) = mpsc::channel();
            let cached = indexed_session(&session(
                Provider::Claude,
                "cached",
                current,
                Some("Cached session"),
            ));
            let send_cache = || {
                sender
                    .send(PickerUpdate::Cached {
                        provider: Provider::Claude,
                        result: Ok(vec![cached.clone()]),
                    })
                    .expect("send cache");
            };
            let send_success = || {
                sender
                    .send(PickerUpdate::Discovered(DiscoveryUpdate {
                        provider: Provider::Claude,
                        result: Ok(vec![session(
                            Provider::Claude,
                            "fresh",
                            current,
                            Some("Fresh session"),
                        )]),
                    }))
                    .expect("send success");
            };
            if cache_first {
                send_cache();
                send_success();
            } else {
                send_success();
                send_cache();
            }

            let mut state = PickerState::new(Vec::new(), current, None, false);
            let mut pending = HashSet::from([Provider::Claude]);
            let mut successful = HashSet::new();
            let mut warnings = Vec::new();
            assert!(receive_updates(
                &receiver,
                &mut state,
                &mut pending,
                &mut successful,
                &mut warnings,
                current,
            ));
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.entries[0].key, "claude:fresh");
            assert!(!state.entries[0].cached);
            assert!(successful.contains(&Provider::Claude));
        }
    }

    #[test]
    fn target_selection_defaults_to_runnable_source() {
        let targets = [Provider::Claude, Provider::Codex, Provider::OpenCode];

        assert_eq!(default_target_index(Provider::Codex, &targets), Some(1));
        assert_eq!(default_target_index(Provider::Grok, &targets), None);
        assert_eq!(move_target_selection(None, targets.len(), true), Some(0));
        assert_eq!(move_target_selection(None, targets.len(), false), Some(2));
        assert_eq!(move_target_selection(Some(2), targets.len(), true), Some(0));
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
