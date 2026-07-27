use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
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
use omnis_adapters::{AdapterRegistry, NativeSession};
use omnis_core::safe_terminal_line;
use omnis_ir::{Provider, SessionRef};

use crate::PROVIDERS;

pub struct PickerSelection {
    pub session: SessionRef,
    pub project_path: Option<PathBuf>,
    pub across_projects: bool,
}

struct PickerEntry {
    session: NativeSession,
    current_workspace: bool,
    search: String,
}

struct PickerState {
    entries: Vec<PickerEntry>,
    query: String,
    provider_index: usize,
    all_projects: bool,
    selected: usize,
}

impl PickerState {
    fn new(
        sessions: Vec<NativeSession>,
        current_project: &Path,
        initial_provider: Option<Provider>,
        all_projects: bool,
    ) -> Self {
        let entries = sessions
            .into_iter()
            .map(|session| PickerEntry {
                current_workspace: session
                    .project_path
                    .as_deref()
                    .is_some_and(|path| paths_match(path, current_project)),
                search: search_text(&session),
                session,
            })
            .collect();
        let provider_index = initial_provider
            .and_then(|provider| {
                PROVIDERS
                    .iter()
                    .position(|candidate| *candidate == provider)
            })
            .map_or(0, |index| index + 1);
        Self {
            entries,
            query: String::new(),
            provider_index,
            all_projects,
            selected: 0,
        }
    }

    fn provider(&self) -> Option<Provider> {
        self.provider_index
            .checked_sub(1)
            .and_then(|index| PROVIDERS.get(index).copied())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.all_projects || entry.current_workspace)
            .filter(|(_, entry)| {
                self.provider()
                    .is_none_or(|provider| entry.session.session.provider == provider)
            })
            .filter(|(_, entry)| query.is_empty() || entry.search.contains(&query))
            .map(|(index, _)| index)
            .collect()
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

pub fn pick_session(
    registry: &AdapterRegistry,
    current_project: &Path,
    target: Option<Provider>,
    initial_provider: Option<Provider>,
    all_projects: bool,
) -> Result<Option<PickerSelection>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "SOURCE is required without an interactive terminal; run `omnis list` or pass `provider:id`"
        );
    }

    let (sessions, warnings) = discover_sessions(registry);
    let mut state = PickerState::new(sessions, current_project, initial_provider, all_projects);
    let _terminal = TerminalGuard::enter()?;
    loop {
        render(&state, target, warnings.len())?;
        let Event::Key(key) = event::read().context("reading session picker input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match handle_key(&mut state, key) {
            PickerAction::Continue => {}
            PickerAction::Cancel => return Ok(None),
            PickerAction::Select => {
                let selection = state.selected_entry().map(|entry| PickerSelection {
                    session: entry.session.session.clone(),
                    project_path: entry.session.project_path.clone(),
                    across_projects: state.all_projects,
                });
                if selection.is_some() {
                    return Ok(selection);
                }
            }
        }
    }
}

fn discover_sessions(registry: &AdapterRegistry) -> (Vec<NativeSession>, Vec<String>) {
    let discovered = thread::scope(|scope| {
        let handles = PROVIDERS.map(|provider| {
            (
                provider,
                scope.spawn(move || registry.list_sessions(provider, None)),
            )
        });
        handles.map(|(provider, handle)| {
            (
                provider,
                handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("provider discovery panicked"))),
            )
        })
    });
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    for (provider, result) in discovered {
        match result {
            Ok(found) => {
                for session in found {
                    if seen.insert(session.session.to_string()) {
                        sessions.push(session);
                    }
                }
            }
            Err(error) => warnings.push(format!("{provider}: {error}")),
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    (sessions, warnings)
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

fn render(state: &PickerState, target: Option<Provider>, warning_count: usize) -> Result<()> {
    let (width, height) = terminal::size().context("reading terminal size")?;
    let width = usize::from(width).max(1);
    let height = usize::from(height).max(1);
    let visible = state.visible_indices();
    let list_height = height.saturating_sub(11).max(1);
    let selected = state.selected.min(visible.len().saturating_sub(1));
    let first = selected.saturating_sub(list_height.saturating_sub(1));
    let mut output = io::stdout().lock();
    queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    render_header(&mut output, state, target, width)?;
    render_session_rows(
        &mut output,
        state,
        &visible,
        first,
        selected,
        list_height,
        width,
    )?;
    render_footer(&mut output, state, warning_count, width)?;
    output.flush().context("drawing session picker")
}

fn render_header(
    output: &mut impl Write,
    state: &PickerState,
    target: Option<Provider>,
    width: usize,
) -> Result<()> {
    queue!(
        output,
        SetAttribute(Attribute::Bold),
        Print("OmniSession  Choose source session"),
        SetAttribute(Attribute::Reset),
        Print("\r\n")
    )?;
    let target = target.map_or_else(
        || "same provider".to_owned(),
        |provider| provider.to_string(),
    );
    queue!(
        output,
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            &format!("Target: {target}  Enter resume  Esc cancel"),
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
        "type to search".to_owned()
    } else {
        state.query.clone()
    };
    queue!(
        output,
        Print(truncate(&format!("Search: {query}"), width)),
        Print("\r\n\r\n")
    )?;
    Ok(())
}

fn render_session_rows(
    output: &mut impl Write,
    state: &PickerState,
    visible: &[usize],
    first: usize,
    selected: usize,
    list_height: usize,
    width: usize,
) -> Result<()> {
    if visible.is_empty() {
        let hint = if state.all_projects {
            "No matching sessions. Clear search or change source provider."
        } else {
            "No matching sessions here. Press Tab to search all workspaces."
        };
        queue!(
            output,
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(hint, width)),
            ResetColor,
            Print("\r\n")
        )?;
    } else {
        for (row, entry_index) in visible.iter().skip(first).take(list_height).enumerate() {
            let entry = &state.entries[*entry_index].session;
            let is_selected = first + row == selected;
            if is_selected {
                queue!(
                    output,
                    SetForegroundColor(Color::Green),
                    SetAttribute(Attribute::Bold)
                )?;
            }
            queue!(
                output,
                Print(session_line(entry, is_selected, width)),
                ResetColor,
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
        }
    }

    let rendered_rows = visible.len().min(list_height);
    for _ in rendered_rows..list_height {
        queue!(output, Print("\r\n"))?;
    }
    queue!(output, Print("\r\n"))?;
    Ok(())
}

fn render_footer(
    output: &mut impl Write,
    state: &PickerState,
    warning_count: usize,
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
        queue!(
            output,
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(&entry.session.session.to_string(), width)),
            Print("\r\n"),
            Print(truncate(&format!("{workspace} · {branch}"), width)),
            ResetColor,
            Print("\r\n")
        )?;
    } else {
        queue!(output, Print("\r\n\r\n"))?;
    }
    let warning = if warning_count == 0 {
        "↑↓ move  PgUp/PgDn jump  Backspace edit  Enter resume  Esc cancel".to_owned()
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

fn session_line(session: &NativeSession, selected: bool, width: usize) -> String {
    let marker = if selected { "›" } else { " " };
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
