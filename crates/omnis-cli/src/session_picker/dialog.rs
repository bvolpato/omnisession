use super::{KeyCode, KeyEvent, PathBuf, PickerAction, PickerState, SessionRef};

pub(super) struct DeleteDialog {
    pub(super) pi_file: Option<crate::pi_import::ConfirmedPiFile>,
    pub(super) row_key: String,
    pub(super) session: SessionRef,
    pub(super) title: String,
    pub(super) workspace: Option<PathBuf>,
    pub(super) branch: Option<String>,
    pub(super) phase: DeletePhase,
}

impl DeleteDialog {
    pub(super) fn workspace_or(&self, fallback: &std::path::Path) -> PathBuf {
        self.workspace
            .as_deref()
            .filter(|path| path.is_dir())
            .unwrap_or(fallback)
            .to_owned()
    }
}

pub(super) enum DeletePhase {
    Confirm,
    Deleting,
    Failed(String),
}

pub(super) fn handle_dialog_key(state: &mut PickerState, key: KeyEvent) -> Option<PickerAction> {
    if let Some(dialog) = &state.delete_dialog {
        return Some(match dialog.phase {
            DeletePhase::Confirm => match key.code {
                KeyCode::Char('y' | 'Y') => PickerAction::ConfirmDelete,
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    state.delete_dialog = None;
                    PickerAction::Dismiss
                }
                _ => PickerAction::Continue,
            },
            DeletePhase::Deleting => PickerAction::Continue,
            DeletePhase::Failed(_) => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                    state.delete_dialog = None;
                    PickerAction::Dismiss
                }
                _ => PickerAction::Continue,
            },
        });
    }
    state.update_dialog.as_ref()?;
    Some(match key.code {
        KeyCode::Char('y' | 'Y') => PickerAction::Update,
        KeyCode::Char('n' | 'N') | KeyCode::Esc => {
            state.update_dialog = None;
            PickerAction::Dismiss
        }
        _ => PickerAction::Continue,
    })
}
