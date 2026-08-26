use super::{
    Attribute, Clear, ClearType, Color, Context, DateTime, DeleteDialog, DeletePhase,
    EnterAlternateScreen, HandoffMessage, HandoffRole, HashMap, HashSet, Hide,
    LeaveAlternateScreen, LineageTreeNode, Local, MoveTo, NativeSession, Path, PathBuf,
    PickerEntry, PickerState, PreviewValue, Print, Provider, ResetColor, Result, SessionPreview,
    SessionRef, SessionTrajectoryMatch, SetAttribute, SetBackgroundColor, SetForegroundColor, Show,
    SynchronizedUpdate, UnicodeWidthChar, UnicodeWidthStr, Utc, Write, disable_raw_mode,
    enable_raw_mode, env, execute, fs, io, queue, safe_terminal_line, terminal,
};

#[derive(Default)]
pub(super) struct PickerRenderState {
    terminal_size: Option<(usize, usize)>,
}

impl PickerRenderState {
    pub(super) fn invalidate(&mut self) {
        self.terminal_size = None;
    }

    pub(super) fn render(
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

pub(super) fn picker_frame(
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
    let (first, selected) = centered_list_window(
        state.list_row_count(visible.len()),
        state.selected_row(),
        row_count,
    );
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
        layout.status_y,
        width,
        StatusContext {
            warning_count,
            pending_count,
            trajectory_search_pending: state.trajectory_search_pending,
            trajectory_search_has_more: state.trajectory_search_has_more,
            action: if state.new_session_selected() {
                StatusAction::Start
            } else if target.is_some() {
                StatusAction::Resume
            } else {
                StatusAction::Continue
            },
            notice: state.notice.as_deref(),
            available_update: state.available_update.as_deref(),
            can_delete: state.selected_entry().is_some_and(|entry| {
                state
                    .delete_providers
                    .contains(&entry.session.session.provider)
            }),
        },
    )?;
    if let Some(dialog) = &state.delete_dialog {
        render_delete_dialog(&mut frame, dialog, width, height)?;
    } else if let Some(version) = state.update_dialog.as_deref() {
        render_update_dialog(&mut frame, version, width, height)?;
    }
    render_state.terminal_size = Some((width, height));
    Ok(frame)
}

pub(super) fn present_frame(frame: &[u8]) -> Result<()> {
    let mut output = io::stdout().lock();
    present_frame_to(&mut output, frame)
}

pub(super) fn present_frame_to(output: &mut impl Write, frame: &[u8]) -> Result<()> {
    output
        .sync_update(|output| output.write_all(frame))
        .context("starting synchronized terminal update")?
        .context("writing synchronized terminal update")
}

pub(super) fn terminal_list_row_count() -> Result<usize> {
    let (width, height) = terminal::size().context("reading terminal size")?;
    Ok(
        screen_layout(usize::from(width).max(1), usize::from(height).max(1))
            .list
            .height
            .saturating_sub(2)
            .max(1),
    )
}

pub(super) fn centered_list_window(
    visible_count: usize,
    selected: usize,
    row_count: usize,
) -> (usize, usize) {
    let selected = selected.min(visible_count.saturating_sub(1));
    let first = selected
        .saturating_sub(row_count / 2)
        .min(visible_count.saturating_sub(row_count));
    (first, selected)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Rect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ScreenLayout {
    pub(super) list: Rect,
    pub(super) detail: Option<Rect>,
    pub(super) detail_right: bool,
    pub(super) status_y: usize,
}

pub(super) fn screen_layout(width: usize, height: usize) -> ScreenLayout {
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

pub(super) fn render_header(
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
pub(super) struct ListViewport {
    pub(super) first: usize,
    pub(super) selected: usize,
    pub(super) height: usize,
    pub(super) pending_count: usize,
}

pub(super) fn render_session_list(
    output: &mut impl Write,
    state: &PickerState,
    visible: &[usize],
    viewport: ListViewport,
    area: Rect,
) -> Result<()> {
    if area.height == 0 {
        return Ok(());
    }
    let columns = ListColumns::for_width(area.width, state.all_projects);
    render_list_header(output, area, &columns)?;
    if area.height <= 2 {
        return Ok(());
    }
    let body_height = area.height - 2;
    let total_rows = state.list_row_count(visible.len());
    let mut drawn_rows = 0;
    for (row, list_index) in (viewport.first..total_rows)
        .take(viewport.height.min(body_height))
        .enumerate()
    {
        render_picker_list_row(
            output,
            state,
            visible,
            list_index,
            list_index == viewport.selected,
            &columns,
            Rect {
                x: area.x,
                y: area.y + 2 + row,
                width: area.width,
                height: 1,
            },
        )?;
        drawn_rows = row + 1;
    }
    if visible.is_empty() && drawn_rows < body_height {
        draw_line(
            output,
            Rect {
                x: area.x,
                y: area.y + 2 + drawn_rows,
                width: area.width,
                height: 1,
            },
            empty_list_hint(state.all_projects, viewport.pending_count),
            DetailStyle::Muted,
            false,
        )?;
        drawn_rows += 1;
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

pub(super) fn render_list_header(
    output: &mut impl Write,
    area: Rect,
    columns: &ListColumns,
) -> Result<()> {
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
    )
}

pub(super) fn render_picker_list_row(
    output: &mut impl Write,
    state: &PickerState,
    visible: &[usize],
    list_index: usize,
    selected: bool,
    columns: &ListColumns,
    area: Rect,
) -> Result<()> {
    if state.show_new_session() && list_index == 0 {
        return draw_line(
            output,
            area,
            &new_session_line(state, selected, columns),
            if selected {
                DetailStyle::Selected
            } else {
                DetailStyle::Accent
            },
            selected,
        );
    }
    let offset = usize::from(state.show_new_session());
    let Some(entry_index) = list_index
        .checked_sub(offset)
        .and_then(|index| visible.get(index))
    else {
        return Ok(());
    };
    let picker_entry = &state.entries[*entry_index];
    let entry = &picker_entry.session;
    let preview = state.previews.get(&state.preview_key(entry));
    draw_line(
        output,
        area,
        &session_line(
            entry,
            selected,
            &list_lineage_prefix(state, entry),
            columns,
            preview,
            state.trajectory_match(picker_entry),
        ),
        if selected {
            DetailStyle::Selected
        } else {
            DetailStyle::Normal
        },
        selected,
    )
}

pub(super) fn list_lineage_prefix(state: &PickerState, entry: &NativeSession) -> String {
    if state.query.trim().is_empty() {
        state.lineage.list_prefix(&entry.session)
    } else {
        String::new()
    }
}

pub(super) fn empty_list_hint(all_projects: bool, pending_count: usize) -> &'static str {
    if pending_count > 0 {
        "Scanning provider stores... results appear as they arrive."
    } else if all_projects {
        "No matching sessions. Clear search or change source provider."
    } else {
        "No matching sessions here. Press Tab to search all workspaces."
    }
}

pub(super) fn render_selected_detail(
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
        let line_area = Rect {
            x: area.x,
            y: area.y + row,
            width: area.width,
            height: 1,
        };
        if line.highlights.is_empty() {
            draw_line(output, line_area, &line.text, line.style, false)?;
        } else {
            draw_highlighted_line(output, line_area, &line)?;
        }
    }
    erase_rows(output, area, visible_lines)
}

pub(super) fn erase_rows(output: &mut impl Write, area: Rect, first_row: usize) -> Result<()> {
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

pub(super) fn render_status(
    output: &mut impl Write,
    y: usize,
    width: usize,
    context: StatusContext<'_>,
) -> Result<()> {
    let action = match context.action {
        StatusAction::Start => "start",
        StatusAction::Resume => "resume",
        StatusAction::Continue => "continue",
    };
    let delete_hint = if context.can_delete {
        "  Del delete"
    } else {
        ""
    };
    let warning = if let Some(notice) = context.notice {
        notice.to_owned()
    } else if context.trajectory_search_pending {
        format!("Searching indexed trajectories  ·  ↑↓ move  Enter {action}  Esc cancel")
    } else if context.pending_count > 0 {
        format!(
            "Refreshing {} source(s)  ·  ↑↓ move  Tab workspace  ←/→ source  Enter {action}",
            context.pending_count
        )
    } else if context.trajectory_search_has_more {
        format!("Top indexed trajectory matches shown  ·  ↑↓ move  Enter {action}  Esc cancel")
    } else if context.warning_count == 0 {
        format!(
            "↑↓ move  PgUp/PgDn jump  Tab workspace  ←/→ source  Enter {action}{delete_hint}  Esc cancel"
        )
    } else {
        format!(
            "↑↓ move  Enter {action}  ·  {} provider warning(s); run `omni doctor`",
            context.warning_count
        )
    };
    let version = context.available_update.map_or_else(
        || format!("v{}", env!("CARGO_PKG_VERSION")),
        |latest| format!("v{} · Ctrl+U -> v{latest}", env!("CARGO_PKG_VERSION")),
    );
    let version_width = UnicodeWidthStr::width(version.as_str()).min(width);
    let warning_width = width.saturating_sub(version_width.saturating_add(2));
    let warning = truncate(&warning, warning_width);
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
    )?;
    if width > 0 && version_width <= width {
        draw_line(
            output,
            Rect {
                x: width - version_width,
                y,
                width: version_width,
                height: 1,
            },
            &version,
            if context.available_update.is_some() {
                DetailStyle::Accent
            } else {
                DetailStyle::Muted
            },
            false,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct StatusContext<'a> {
    warning_count: usize,
    pending_count: usize,
    trajectory_search_pending: bool,
    trajectory_search_has_more: bool,
    action: StatusAction,
    notice: Option<&'a str>,
    available_update: Option<&'a str>,
    can_delete: bool,
}

pub(super) fn render_delete_dialog(
    output: &mut impl Write,
    dialog: &DeleteDialog,
    width: usize,
    height: usize,
) -> Result<()> {
    if width < 24 || height < 9 {
        return Ok(());
    }
    let dialog_width = width.saturating_sub(4).min(78);
    let inner_width = dialog_width.saturating_sub(2);
    let workspace = dialog.workspace.as_deref().map_or_else(
        || "Workspace not recorded".to_owned(),
        |path| safe_terminal_line(&path.display().to_string()),
    );
    let location = dialog.branch.as_deref().map_or_else(
        || workspace.clone(),
        |branch| format!("{workspace} · {}", safe_terminal_line(branch)),
    );
    let (status, help, status_style) = match &dialog.phase {
        DeletePhase::Confirm => (
            format!("Permanently delete from {}?", dialog.session.provider),
            "y delete   n cancel   a always this run".to_owned(),
            DetailStyle::Danger,
        ),
        DeletePhase::Deleting => (
            format!("Deleting {}…", dialog.session.provider),
            "Waiting for provider confirmation".to_owned(),
            DetailStyle::Accent,
        ),
        DeletePhase::Failed(error) => (
            format!("Delete failed: {error}"),
            "Enter or Esc to close".to_owned(),
            DetailStyle::Danger,
        ),
    };
    let border = format!("┌{}┐", "─".repeat(inner_width));
    let bottom = format!("└{}┘", "─".repeat(inner_width));
    let framed = |text: &str| format!("│{}│", fit_cell(text, inner_width));
    let lines = [
        detail_line(border, DetailStyle::Danger),
        detail_line(framed(" DELETE SESSION"), DetailStyle::Danger),
        detail_line(framed(&format!(" {}", dialog.title)), DetailStyle::Strong),
        detail_line(framed(&format!(" {}", dialog.session)), DetailStyle::Muted),
        detail_line(framed(&format!(" {location}")), DetailStyle::Muted),
        detail_line(framed(""), DetailStyle::Normal),
        detail_line(framed(&format!(" {status}")), status_style),
        detail_line(framed(&format!(" {help}")), DetailStyle::Strong),
        detail_line(bottom, DetailStyle::Danger),
    ];
    let area = Rect {
        x: (width - dialog_width) / 2,
        y: (height - lines.len()) / 2,
        width: dialog_width,
        height: lines.len(),
    };
    for (row, line) in lines.iter().enumerate() {
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
    Ok(())
}

pub(super) fn render_update_dialog(
    output: &mut impl Write,
    version: &str,
    width: usize,
    height: usize,
) -> Result<()> {
    if width < 24 || height < 8 {
        return draw_line(
            output,
            Rect {
                x: 0,
                y: height.saturating_sub(1),
                width,
                height: 1,
            },
            &format!("y/n · update v{version}"),
            DetailStyle::Accent,
            false,
        );
    }
    let dialog_width = width.saturating_sub(4).min(78);
    let inner_width = dialog_width.saturating_sub(2);
    let executable = env::current_exe().map_or_else(
        |_| "Current executable path unavailable".to_owned(),
        |path| safe_terminal_line(&path.display().to_string()),
    );
    let border = format!("┌{}┐", "─".repeat(inner_width));
    let bottom = format!("└{}┘", "─".repeat(inner_width));
    let framed = |text: &str| format!("│{}│", fit_cell(text, inner_width));
    let lines = [
        detail_line(border, DetailStyle::Accent),
        detail_line(framed(" UPDATE OMNISESSION"), DetailStyle::Accent),
        detail_line(
            framed(&format!(" v{} -> v{version}", env!("CARGO_PKG_VERSION"))),
            DetailStyle::Strong,
        ),
        detail_line(framed(&format!(" {executable}")), DetailStyle::Muted),
        detail_line(framed(""), DetailStyle::Normal),
        detail_line(
            framed(" Replace this executable with verified release?"),
            DetailStyle::Accent,
        ),
        detail_line(framed(" y update   n cancel"), DetailStyle::Strong),
        detail_line(bottom, DetailStyle::Accent),
    ];
    let area = Rect {
        x: (width - dialog_width) / 2,
        y: (height - lines.len()) / 2,
        width: dialog_width,
        height: lines.len(),
    };
    for (row, line) in lines.iter().enumerate() {
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
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum StatusAction {
    Start,
    Resume,
    Continue,
}

#[derive(Clone, Copy)]
pub(super) enum DetailStyle {
    Normal,
    Muted,
    Accent,
    Strong,
    Selected,
    Danger,
}

pub(super) struct DetailLine {
    pub(super) text: String,
    pub(super) style: DetailStyle,
    pub(super) highlights: Vec<String>,
}

pub(super) fn detail_line(text: impl Into<String>, style: DetailStyle) -> DetailLine {
    DetailLine {
        text: text.into(),
        style,
        highlights: Vec::new(),
    }
}

pub(super) fn highlighted_detail_line(
    text: impl Into<String>,
    style: DetailStyle,
    query: &str,
) -> DetailLine {
    DetailLine {
        text: text.into(),
        style,
        highlights: search_highlight_terms(query),
    }
}

pub(super) fn detail_field(
    label: &str,
    value: &str,
    width: usize,
    style: DetailStyle,
) -> DetailLine {
    const LABEL_WIDTH: usize = 10;
    let value_width = width.saturating_sub(LABEL_WIDTH + 1);
    let value = truncate_middle(value, value_width);
    detail_line(format!("{label:<LABEL_WIDTH$} {value}"), style)
}

pub(super) struct SessionLocation {
    project: String,
    workspace: String,
    directory: String,
    branch: String,
    compact_branch: String,
    current_branch: Option<String>,
    head: Option<String>,
    state: String,
}

pub(super) fn session_location(
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

pub(super) fn selected_detail_lines(
    state: &PickerState,
    width: usize,
    height: usize,
) -> Vec<DetailLine> {
    if state.new_session_selected() {
        let directory = safe_terminal_line(&state.current_project.display().to_string());
        let branch = state
            .current_git_branch
            .as_deref()
            .map_or_else(|| "not recorded".to_owned(), safe_terminal_line);
        return vec![
            detail_line("NEW SESSION", DetailStyle::Accent),
            detail_line("Start with a clean agent session.", DetailStyle::Strong),
            detail_line(String::new(), DetailStyle::Normal),
            detail_line("WORKSPACE", DetailStyle::Accent),
            detail_field("Directory", &directory, width, DetailStyle::Normal),
            detail_field("Branch", &branch, width, DetailStyle::Normal),
            detail_line(String::new(), DetailStyle::Normal),
            detail_line(
                "Press Enter to choose an installed agent.",
                DetailStyle::Muted,
            ),
        ]
        .into_iter()
        .take(height)
        .collect();
    }
    let Some(entry) = state.selected_entry() else {
        return vec![detail_line(
            "Select a session to inspect it.",
            DetailStyle::Muted,
        )];
    };
    let key = state.preview_key(&entry.session);
    let preview = state.previews.get(&key);
    let (ready_preview, preview_complete) = match preview {
        Some(PreviewValue::Ready {
            preview, complete, ..
        }) => (Some(preview.as_ref()), *complete),
        _ => (None, false),
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
                session_relative_time(&entry.session),
            ),
            DetailStyle::Muted,
        ),
    ];
    if let Some(preview) = ready_preview {
        let remaining_height = height.saturating_sub(lines.len());
        append_session_metadata(
            &mut lines,
            preview,
            preview_complete,
            entry.session.event_count,
            width,
            remaining_height,
        );
    }
    if let Some(trajectory_match) = state.trajectory_match(entry) {
        let remaining_height = height.saturating_sub(lines.len());
        append_search_match(
            &mut lines,
            trajectory_match,
            &state.query,
            width,
            remaining_height,
        );
    }
    if !state.query.trim().is_empty() {
        append_lineage_tree(&mut lines, state, &entry.session.session, width, height);
    }
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

pub(super) fn append_session_metadata(
    lines: &mut Vec<DetailLine>,
    preview: &SessionPreview,
    preview_complete: bool,
    discovered_event_count: usize,
    width: usize,
    height: usize,
) {
    if height < 2 {
        return;
    }
    let event_count = discovered_event_count.max(preview.event_count);
    let trajectory = format!(
        "{} messages · {} tools · {event_count} events",
        grouped_number(preview.message_count),
        grouped_number(preview.tool_event_count),
    );
    lines.push(detail_line(String::new(), DetailStyle::Normal));
    lines.push(detail_field(
        "Trajectory",
        &trajectory,
        width,
        DetailStyle::Normal,
    ));
    if let Some(tokens) = preview.total_tokens {
        let coverage = if preview.token_usage_is_cumulative || preview_complete {
            "total"
        } else {
            "sampled"
        };
        lines.push(detail_field(
            "Tokens",
            &format!("{} {coverage}", grouped_u64(tokens)),
            width,
            DetailStyle::Strong,
        ));
    }
    if let Some(model) = preview.model.as_deref() {
        lines.push(detail_field(
            "Model",
            &safe_terminal_line(model),
            width,
            DetailStyle::Normal,
        ));
    }
    if let Some(mode) = preview.reasoning_mode.as_deref() {
        lines.push(detail_field(
            "Reasoning",
            &safe_terminal_line(mode),
            width,
            DetailStyle::Normal,
        ));
    }
    if height >= 8 {
        if let Some(version) = preview.provider_version.as_deref() {
            lines.push(detail_field(
                "Agent ver.",
                &safe_terminal_line(version),
                width,
                DetailStyle::Muted,
            ));
        }
    }
}

pub(super) fn append_lineage_tree(
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

pub(super) fn append_search_match(
    lines: &mut Vec<DetailLine>,
    trajectory_match: &SessionTrajectoryMatch,
    query: &str,
    width: usize,
    height: usize,
) {
    if height < 3 {
        return;
    }
    let approximate_tokens = trajectory_match.indexed_byte_count.div_ceil(4);
    let coverage = if trajectory_match.complete {
        "complete index"
    } else if trajectory_match.source_complete {
        "head-tail index"
    } else {
        "preview index"
    };
    lines.push(detail_line(String::new(), DetailStyle::Normal));
    lines.push(detail_line(
        format!(
            "MATCHED TRAJECTORY · {coverage} · ~{} text tokens",
            grouped_number(approximate_tokens)
        ),
        DetailStyle::Accent,
    ));
    let excerpt_height = height.saturating_sub(2).min(4);
    lines.extend(
        wrap_text(&trajectory_match.snippet, width, excerpt_height)
            .into_iter()
            .map(|line| highlighted_detail_line(line, DetailStyle::Normal, query)),
    );
}

pub(super) fn lineage_tree_line(
    state: &PickerState,
    node: &LineageTreeNode,
    width: usize,
) -> DetailLine {
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

pub(super) fn append_workspace_details(
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
    append_full_workspace_details(lines, location, entry, width, height >= 26);
}

pub(super) fn append_compact_workspace_details(
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

pub(super) fn append_full_workspace_details(
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
            &format_timestamp(entry.session.created_at, false),
            width,
            DetailStyle::Muted,
        ));
        lines.push(detail_field(
            "Updated",
            &format_timestamp(
                entry.session.updated_at,
                entry.session.updated_at_approximate,
            ),
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

pub(super) fn format_timestamp(timestamp: Option<DateTime<Utc>>, approximate: bool) -> String {
    let formatted = timestamp.map_or_else(
        || "not recorded".to_owned(),
        |timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        },
    );
    if approximate && timestamp.is_some() {
        format!("~{formatted}")
    } else {
        formatted
    }
}

pub(super) fn append_preview_lines(
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

pub(super) fn preview_title(preview: &PreviewValue) -> Option<String> {
    let PreviewValue::Ready { preview, .. } = preview else {
        return None;
    };
    preview
        .first
        .as_ref()
        .map(|message| compact_text(&message.text))
        .filter(|title| !title.is_empty())
}

pub(super) fn compact_text(value: &str) -> String {
    safe_terminal_line(value)
        .replace("\\r\\n", " ")
        .replace("\\n", " ")
        .replace("\\t", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn append_message_excerpt(
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

pub(super) const fn role_label(role: HandoffRole) -> &'static str {
    match role {
        HandoffRole::User => "USER",
        HandoffRole::Assistant => "ASSISTANT",
    }
}

pub(super) fn wrap_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
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

pub(super) fn with_ellipsis(value: &str, width: usize) -> String {
    if value.ends_with('…') {
        value.to_owned()
    } else {
        truncate(&format!("{value}…"), width)
    }
}

pub(super) fn draw_line(
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
        DetailStyle::Danger => Color::Red,
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
    if matches!(
        style,
        DetailStyle::Strong | DetailStyle::Selected | DetailStyle::Danger
    ) {
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

pub(super) fn draw_highlighted_line(
    output: &mut impl Write,
    area: Rect,
    line: &DetailLine,
) -> Result<()> {
    let text = fit_cell(&line.text, area.width);
    let lowercase = text.to_ascii_lowercase();
    let mut highlighted = vec![false; text.len()];
    for term in &line.highlights {
        for (start, matched) in lowercase.match_indices(term) {
            highlighted[start..start + matched.len()].fill(true);
        }
    }
    queue!(
        output,
        MoveTo(
            u16::try_from(area.x).unwrap_or(u16::MAX),
            u16::try_from(area.y).unwrap_or(u16::MAX)
        ),
        SetForegroundColor(detail_style_color(line.style))
    )?;
    if detail_style_is_bold(line.style) {
        queue!(output, SetAttribute(Attribute::Bold))?;
    }
    let mut active = false;
    for (index, character) in text.char_indices() {
        let next_active = highlighted.get(index).copied().unwrap_or(false);
        if next_active != active {
            if next_active {
                queue!(
                    output,
                    SetForegroundColor(Color::Black),
                    SetBackgroundColor(Color::Yellow),
                    SetAttribute(Attribute::Bold)
                )?;
            } else {
                queue!(
                    output,
                    ResetColor,
                    SetForegroundColor(detail_style_color(line.style)),
                    SetAttribute(Attribute::Reset)
                )?;
                if detail_style_is_bold(line.style) {
                    queue!(output, SetAttribute(Attribute::Bold))?;
                }
            }
            active = next_active;
        }
        queue!(output, Print(character))?;
    }
    queue!(output, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

pub(super) fn search_highlight_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| std::cmp::Reverse(term.len()));
    terms.dedup();
    terms
}

pub(super) const fn detail_style_color(style: DetailStyle) -> Color {
    match style {
        DetailStyle::Muted => Color::DarkGrey,
        DetailStyle::Accent => Color::Cyan,
        DetailStyle::Selected => Color::Green,
        DetailStyle::Danger => Color::Red,
        DetailStyle::Normal | DetailStyle::Strong => Color::Reset,
    }
}

pub(super) const fn detail_style_is_bold(style: DetailStyle) -> bool {
    matches!(
        style,
        DetailStyle::Strong | DetailStyle::Selected | DetailStyle::Danger
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ListColumns {
    pub(super) width: usize,
    pub(super) agent: usize,
    pub(super) title: usize,
    pub(super) project: Option<usize>,
    pub(super) age: Option<usize>,
}

impl ListColumns {
    pub(super) fn for_width(width: usize, show_project: bool) -> Self {
        if width >= 80 {
            let agent = 18;
            let age = 10;
            let project = show_project.then(|| {
                let available = width.saturating_sub(agent + age + 5);
                (width / 5).clamp(16, 24).min(available.saturating_sub(24))
            });
            let title = width
                .saturating_sub(agent + age + 4)
                .saturating_sub(project.map_or(0, |project| project + 1));
            Self {
                width,
                agent,
                title,
                project,
                age: Some(age),
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

    pub(super) fn header(self) -> String {
        self.line(" ", "AGENT", "TITLE", "PROJECT", "UPDATED")
    }

    pub(super) fn line(
        self,
        marker: &str,
        agent: &str,
        title: &str,
        project: &str,
        age: &str,
    ) -> String {
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

pub(super) fn session_line(
    session: &NativeSession,
    selected: bool,
    lineage_prefix: &str,
    columns: &ListColumns,
    preview: Option<&PreviewValue>,
    trajectory_match: Option<&SessionTrajectoryMatch>,
) -> String {
    let marker = if selected { "›" } else { " " };
    let provider = format!("{lineage_prefix}{}", session.session.provider);
    let raw_title = trajectory_match.map_or_else(
        || display_title(session, preview),
        |item| format!("match · {}", compact_text(&item.snippet)),
    );
    let raw_project = session
        .project_path
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map_or_else(|| "unknown".to_owned(), safe_terminal_line);
    let age = session_relative_time(session);
    columns.line(marker, &provider, &raw_title, &raw_project, &age)
}

pub(super) fn new_session_line(
    state: &PickerState,
    selected: bool,
    columns: &ListColumns,
) -> String {
    let marker = if selected { "›" } else { " " };
    let project = state
        .current_project
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "current workspace".to_owned(), safe_terminal_line);
    columns.line(marker, "+", "NEW SESSION", &project, "")
}

pub(super) fn display_title(session: &NativeSession, preview: Option<&PreviewValue>) -> String {
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

pub(super) fn preview_continuation_title(preview: &PreviewValue) -> Option<String> {
    let PreviewValue::Ready { continuation, .. } = preview else {
        return None;
    };
    continuation
        .as_ref()
        .map(|message| compact_text(&message.text))
        .filter(|title| !title.is_empty())
}

pub(super) fn short_session_ref(session: &SessionRef) -> String {
    format!(
        "{}:{}",
        session.provider,
        short_id(&safe_terminal_line(&session.id))
    )
}

pub(super) fn search_text(session: &NativeSession) -> String {
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

pub(super) fn short_id(id: &str) -> String {
    let mut value = id.chars().take(12).collect::<String>();
    if id.chars().count() > 12 {
        value.push('…');
    }
    value
}

pub(super) fn relative_time(updated_at: Option<DateTime<Utc>>, approximate: bool) -> String {
    let Some(updated_at) = updated_at else {
        return "unknown".to_owned();
    };
    let seconds = (Utc::now() - updated_at).num_seconds().max(0);
    let age = match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => updated_at.format("%Y-%m-%d").to_string(),
    };
    if approximate { format!("~{age}") } else { age }
}

pub(super) fn session_relative_time(session: &NativeSession) -> String {
    relative_time(session.updated_at, session.updated_at_approximate)
}

pub(super) fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
}

pub(super) fn populate_approximate_updated_at(sessions: &mut [NativeSession]) {
    let mut modified_by_path = HashMap::<PathBuf, Option<DateTime<Utc>>>::new();
    for session in sessions {
        if session.updated_at.is_some() {
            continue;
        }
        let file_updated_at = session.source_path.as_ref().and_then(|path| {
            *modified_by_path
                .entry(path.clone())
                .or_insert_with(|| file_modified_at(path))
        });
        if let Some(updated_at) = file_updated_at.or(session.created_at) {
            session.updated_at = Some(updated_at);
            session.updated_at_approximate = true;
        }
    }
}

pub(super) fn truncate(value: &str, width: usize) -> String {
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

pub(super) fn truncate_middle(value: &str, width: usize) -> String {
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

pub(super) fn take_prefix_cells(value: &str, width: usize) -> String {
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

pub(super) fn take_suffix_cells(value: &str, width: usize) -> String {
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

pub(super) fn fit_cell(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{value}{}", " ".repeat(padding))
}

pub(super) fn grouped_number(value: usize) -> String {
    grouped_digits(&value.to_string())
}

pub(super) fn grouped_u64(value: u64) -> String {
    grouped_digits(&value.to_string())
}

pub(super) fn grouped_digits(digits: &str) -> String {
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).checked_rem(3) == Some(0) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter() -> Result<Self> {
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
