use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use omnis_ir::Provider;
use serde_json::Value;

use crate::{git_worktree_root, may_reach_network, workspace_paths_match};

const MAX_WORKSPACE_BYTES: u64 = 1024 * 1024;
const MAX_FOLDERS: usize = 256;
const MAX_SYMLINKS: usize = 32;

/// Reuses workspace membership checks during one pass over session metadata.
pub struct SessionWorkspaceMatcher<'a> {
    requested: &'a Path,
    directories: HashMap<OsString, bool>,
    cursor_workspaces: HashMap<OsString, bool>,
}

impl<'a> SessionWorkspaceMatcher<'a> {
    #[must_use]
    pub fn new(requested: &'a Path) -> Self {
        Self {
            requested,
            directories: HashMap::new(),
            cursor_workspaces: HashMap::new(),
        }
    }

    /// Uses the exact OS spelling so that network paths stay distinct from local paths.
    pub fn matches(&mut self, provider: Provider, recorded: &Path) -> bool {
        if provider != Provider::CursorIde && recorded.as_os_str() == self.requested.as_os_str() {
            return true;
        }
        let cache = if provider == Provider::CursorIde {
            &mut self.cursor_workspaces
        } else {
            &mut self.directories
        };
        if let Some(&matches) = cache.get(recorded.as_os_str()) {
            return matches;
        }
        let matches = session_workspace_matches(provider, recorded, self.requested);
        cache.insert(recorded.as_os_str().to_owned(), matches);
        matches
    }
}

/// Matches a session's recorded workspace against a requested project.
#[must_use]
pub fn session_workspace_matches(provider: Provider, recorded: &Path, requested: &Path) -> bool {
    if provider == Provider::CursorIde {
        cursor_workspace_matches(recorded, requested)
    } else {
        workspace_paths_match(recorded, requested)
    }
}

fn workspace_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "code-workspace")
}

struct LocalWorkspace {
    physical: PathBuf,
    folders: Option<Vec<Value>>,
}

/// Resolves a local Cursor folder or a verified saved workspace file.
///
/// # Errors
///
/// Returns an error when the recorded URI names a regular file that Cursor cannot reopen as a
/// workspace or that exceeds the local read limits.
pub fn local_cursor_workspace_path(path: &Path) -> io::Result<Option<PathBuf>> {
    Ok(read_local_workspace(path)?.map(|workspace| workspace.physical))
}

/// Returns a local folder as Cursor will name it in a workspace URI. A symlink keeps its own ID.
#[must_use]
pub fn cursor_launch_folder(path: &Path) -> Option<PathBuf> {
    let folder = normalized_local(path)?;
    #[cfg(unix)]
    let folder = if path.components().any(|part| part == Component::ParentDir) {
        let mut folder = PathBuf::new();
        for part in path.components() {
            if part == Component::ParentDir {
                let physical = resolve_local_path(&folder)?;
                if !physical.is_dir() {
                    return None;
                }
                // On Unix, leaving a linked directory goes to its target's parent.
                if fs::symlink_metadata(&folder).ok()?.is_symlink() {
                    folder = physical;
                }
                folder.pop();
            } else {
                folder.push(part.as_os_str());
            }
        }
        folder
    } else {
        folder
    };
    #[cfg(windows)]
    let folder = folder
        .to_str()
        .and_then(crate::ordinary_windows_path)
        .map_or(folder, PathBuf::from);
    resolve_local_path(&folder)?.is_dir().then_some(folder)
}

fn read_local_workspace(path: &Path) -> io::Result<Option<LocalWorkspace>> {
    let Some(physical) = resolve_local_path(path) else {
        return Ok(None);
    };
    if physical.is_dir() {
        return Ok(Some(LocalWorkspace {
            physical,
            folders: None,
        }));
    }
    if !workspace_extension(path) || !physical.is_file() {
        return Ok(None);
    }
    // Cursor checks the name of the URI it opens. The file behind a symlink can have any extension.
    let folders = (!sensitive_workspace_target(&physical))
        .then(|| read_workspace_folders(&physical))
        .flatten()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData,
            "Cursor IDE cannot verify the saved workspace file. A local workspace needs a readable folders array within OmniSession's limits. Export the chat or continue in another agent."))?;
    Ok(Some(LocalWorkspace {
        physical,
        folders: Some(folders),
    }))
}

fn sensitive_workspace_target(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "auth.json"
            | "credentials"
            | "credentials.json"
            | ".credentials.json"
            | "oauth_creds.json"
            | "application_default_credentials.json"
            | "accesstokens.json"
            | "tokens.json"
            | ".git-credentials"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".env"
    ) || name.starts_with(".env.")
        || path.ends_with(".docker/config.json")
        || path.ends_with(".kube/config")
        || (name == "hosts.yml"
            && path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "gh" || name.starts_with("gh-")))
        || path
            .ancestors()
            .skip(1)
            .filter_map(Path::file_name)
            .any(|part| part == ".ssh" || part == ".gnupg")
}

/// Matches Cursor folders normally and saved workspace files by their listed local folders.
#[must_use]
pub fn cursor_workspace_matches(recorded: &Path, requested: &Path) -> bool {
    let same = recorded.as_os_str() == requested.as_os_str();
    if same && !workspace_extension(recorded) {
        return true;
    }
    if may_reach_network(recorded) {
        return !workspace_extension(recorded) && workspace_paths_match(recorded, requested);
    }
    if may_reach_network(requested) {
        return false;
    }
    let Ok(Some(workspace)) = read_local_workspace(recorded) else {
        return false;
    };
    if same {
        return true;
    }
    let Some(requested) = resolve_local_path(requested) else {
        return false;
    };
    let Some(folders) = workspace.folders else {
        return workspace_paths_match(&workspace.physical, requested);
    };
    if !requested.is_dir() {
        return false;
    }
    let Some(base) = recorded.parent() else {
        return false;
    };
    let non_git = std::cell::OnceCell::new();
    folders
        .iter()
        .filter_map(|folder| local_folder(base, folder))
        .filter_map(|folder| resolve_local_path(&folder))
        .any(|folder| {
            folder.is_dir()
                && (workspace_paths_match(&folder, &requested)
                    || (requested.starts_with(folder)
                        && *non_git.get_or_init(|| {
                            git_worktree_root(&requested).is_ok_and(|root| root.is_none())
                        })))
        })
}

fn read_workspace_folders(path: &Path) -> Option<Vec<Value>> {
    let mut config = read_workspace_file(path)?;
    if config
        .get("remoteAuthority")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return None;
    }
    let folders = config.get_mut("folders")?.as_array_mut()?;
    (folders.len() <= MAX_FOLDERS).then(|| std::mem::take(folders))
}

fn read_workspace_file(path: &Path) -> Option<Value> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        File::from(
            open(
                path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?,
        )
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        File::options()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?
    };
    #[cfg(not(any(unix, windows)))]
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_WORKSPACE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_WORKSPACE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_WORKSPACE_BYTES {
        return None;
    }
    serde_json_lenient::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)).ok()
}

fn local_folder(base: &Path, folder: &Value) -> Option<PathBuf> {
    if let Some(name) = folder.get("name")
        && !matches!(name, Value::String(_) | Value::Null | Value::Bool(false))
        && name.as_f64() != Some(0.0)
    {
        return None;
    }
    if let Some(path) = folder.get("path").and_then(Value::as_str) {
        if path.is_empty()
            || path.contains('\0')
            || path.starts_with('\\')
            || path.starts_with("//")
        {
            return None;
        }
        let path = Path::new(path);
        // A foreign drive or URI scheme is not a local relative path.
        if !path.is_absolute()
            && path
                .components()
                .next()?
                .as_os_str()
                .to_str()?
                .contains(':')
        {
            return None;
        }
        return Some(base.join(path));
    }
    local_file_uri(folder.get("uri")?.as_str()?)
}

fn local_file_uri(uri: &str) -> Option<PathBuf> {
    let (scheme, path) = uri.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") || path.contains(['?', '#']) {
        return None;
    }
    let path = path.strip_prefix("//").unwrap_or(path);
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    let mut decoded = Vec::new();
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let digits = [bytes.next()?, bytes.next()?];
            if !digits.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            u8::from_str_radix(std::str::from_utf8(&digits).ok()?, 16).ok()?
        } else {
            byte
        });
    }
    let path = std::str::from_utf8(&decoded).ok()?;
    if path.contains('\0') || path.starts_with("//") {
        return None;
    }
    #[cfg(windows)]
    let path = path.strip_prefix('/')?;
    let path = Path::new(path);
    path.is_absolute().then(|| path.to_owned())
}

/// Normalizes local components before access. A folder can name a sibling directory with `..`.
fn normalized_local(path: &Path) -> Option<PathBuf> {
    let spelling = path.as_os_str().as_encoded_bytes();
    if !path.is_absolute() || spelling.len() > 32_768 || spelling.starts_with(b"//") {
        return None;
    }
    let mut normalized = PathBuf::new();
    for (index, component) in path.components().enumerate() {
        if index >= 256 {
            return None;
        }
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }
    (!may_reach_network(&normalized)).then_some(normalized)
}

/// Resolves local symlinks one at a time so their targets are checked before filesystem access.
fn resolve_local_path(path: &Path) -> Option<PathBuf> {
    let mut path = normalized_local(path)?;
    for _ in 0..=MAX_SYMLINKS {
        let mut prefix = PathBuf::new();
        let traverses_parent = path.components().any(|part| part == Component::ParentDir);
        let mut components = path.components();
        let mut replacement = None;
        while let Some(component) = components.next() {
            if component == Component::ParentDir {
                if !prefix.is_dir() {
                    return None;
                }
                prefix.pop();
                continue;
            }
            prefix.push(component.as_os_str());
            if traverses_parent && may_reach_network(&prefix) {
                return None;
            }
            if matches!(component, Component::Normal(_))
                && fs::symlink_metadata(&prefix).ok()?.is_symlink()
            {
                let target = fs::read_link(&prefix).ok()?;
                let next = prefix.parent()?.join(target).join(components.as_path());
                let checked = normalized_local(&next)?;
                // A Unix symlink target can itself cross another symlink before a parent component.
                #[cfg(unix)]
                let checked = if next.components().any(|part| part == Component::ParentDir) {
                    next
                } else {
                    checked
                };
                replacement = Some(checked);
                break;
            }
        }
        match replacement {
            None => return Some(prefix),
            Some(next) => path = next,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    #[cfg(unix)]
    #[test]
    fn folder_parent_uses_the_links_target_and_keeps_other_links_in_the_uri() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let checkout = root.join("checkout");
        let views = root.join("views");
        fs::create_dir_all(checkout.join("current/child")).unwrap();
        fs::create_dir_all(checkout.join("service")).unwrap();
        fs::create_dir_all(views.join("service")).unwrap();
        let active = views.join("active");
        let linked_service = checkout.join("linked-service");
        symlink(checkout.join("current"), &active).unwrap();
        symlink("service", &linked_service).unwrap();
        assert_eq!(
            cursor_launch_folder(&active.join("../service")),
            Some(checkout.join("service"))
        );
        assert_eq!(
            cursor_launch_folder(&active.join("../linked-service")),
            Some(linked_service)
        );
        assert_eq!(cursor_launch_folder(&active.join("child/..")), Some(active));
        assert_eq!(
            cursor_launch_folder(&views.join("missing/../service")),
            None
        );
        let indirect = views.join("indirect");
        symlink("active/../service", &indirect).unwrap();
        assert_eq!(
            local_cursor_workspace_path(&indirect).unwrap(),
            Some(checkout.join("service"))
        );
    }

    #[test]
    fn workspace_members_handle_relative_paths_local_uris_and_repository_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings");
        let member = root.path().join("member");
        let child = member.join("plain-child");
        let other_repo = member.join("nested-repository");
        let uri_member = root.path().join("second folder");
        for dir in [&settings, &child, &other_repo, &uri_member] {
            fs::create_dir_all(dir).unwrap();
        }
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(&other_repo)
                .status()
                .unwrap()
                .success()
        );
        let uri_path = uri_member
            .display()
            .to_string()
            .replace('\\', "/")
            .replace(' ', "%20");
        let uri = format!("file:///{}", uri_path.trim_start_matches('/'));
        let config = settings.join("team.code-workspace");
        fs::write(
            &config,
            format!(
                r#"// A shared workspace configuration
            {{"folders":[{{"path":"../member"}}, {{"uri":"{uri}"}},
              {{"uri":"vscode-remote://host/unread"}}, {{"name":true,"path":"."}},],}}"#
            ),
        )
        .unwrap();
        for requested in [&member, &uri_member, &child] {
            assert!(
                cursor_workspace_matches(&config, requested),
                "{}",
                requested.display()
            );
        }
        for requested in [&settings, &other_repo] {
            assert!(
                !cursor_workspace_matches(&config, requested),
                "{}",
                requested.display()
            );
        }
        let mut matcher = SessionWorkspaceMatcher::new(&member);
        assert!(matcher.matches(Provider::CursorIde, &config));
        assert!(!matcher.matches(Provider::CursorCli, &config));
        assert!(!cursor_workspace_matches(&member, &child));
    }

    #[test]
    fn workspace_files_require_a_bounded_valid_local_folder_list() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("team.code-workspace");
        for bytes in [
            b"{bad json}".to_vec(),
            json!({"folders":[{"path":"."}], "remoteAuthority":"example"}).to_string().into_bytes(),
            json!({"folders":vec![json!({"path":"."}); MAX_FOLDERS + 1]}).to_string().into_bytes(),
            [br#"{"folders":[{"path":"."}]}"#.to_vec(), vec![b' '; 1024 * 1024]].concat(),
            json!({"folders":[{"uri":"file://other-host/root"},{"uri":"https://host/path"},{"uri":"file:///%+1"}]}).to_string().into_bytes(),
        ] {
            fs::write(&config, bytes).unwrap();
            assert!(!cursor_workspace_matches(&config, root.path()));
        }
        fs::remove_file(&config).unwrap();
        fs::create_dir(&config).unwrap();
        assert!(cursor_workspace_matches(&config, &config));
        assert!(!cursor_workspace_matches(&config, root.path()));
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_must_be_a_readable_local_workspace_before_reopening_it() {
        let root = tempfile::tempdir().unwrap();
        let uri = root.path().join("selected.code-workspace");
        let project = root.path().join("auth");
        fs::create_dir(&project).unwrap();
        let settings = project.join("shared.json");
        std::os::unix::fs::symlink(&settings, &uri).unwrap();
        for data in ["{}", r#"{"folders":null}"#, r#"{"folders":{}}"#, "{"] {
            fs::write(&settings, data).unwrap();
            assert!(super::local_cursor_workspace_path(&uri).is_err());
            assert!(!cursor_workspace_matches(&uri, &uri));
            assert!(!SessionWorkspaceMatcher::new(&uri).matches(Provider::CursorIde, &uri));
        }
        fs::write(&settings, r#"{"folders":[]}"#).unwrap();
        assert!(super::local_cursor_workspace_path(&uri).unwrap().is_some());
        assert!(cursor_workspace_matches(&uri, &uri));
        assert!(!cursor_workspace_matches(&uri, root.path()));
        fs::remove_file(&uri).unwrap();
        let provider = root.path().join(".codex");
        fs::create_dir(&provider).unwrap();
        let private = provider.join("auth.json");
        fs::rename(&settings, &private).unwrap();
        std::os::unix::fs::symlink(private, &uri).unwrap();
        assert!(super::local_cursor_workspace_path(&uri).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_workspace_files_resolve_relative_folders_from_the_saved_uri() {
        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical");
        let lexical = root.path().join("lexical");
        let file = physical.join("settings.code-workspace");
        let alias = lexical.join("open.code-workspace");
        for dir in [&physical, &lexical] {
            fs::create_dir_all(dir.join("member")).unwrap();
        }
        fs::write(&file, r#"{"folders":[{"path":"member"}]}"#).unwrap();
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        assert!(cursor_workspace_matches(&alias, &lexical.join("member")));
        assert!(!cursor_workspace_matches(&alias, &physical.join("member")));
        assert!(cursor_workspace_matches(&file, &physical.join("member")));
        fs::remove_file(&alias).unwrap();
        let other = physical.join("other.json");
        fs::rename(&file, &other).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        assert!(cursor_workspace_matches(&alias, &lexical.join("member")));
        assert_eq!(
            super::local_cursor_workspace_path(&alias).unwrap().unwrap(),
            std::fs::canonicalize(&other).unwrap()
        );
    }
}
