use std::{fs, path::Path};

use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use omnis_ir::{CanonicalSnapshot, EventKind, Provider};

/// Returns whether a stored key has the form of a native local Cursor workspace ID.
#[must_use]
pub fn is_cursor_workspace_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Computes the ID Cursor opens for a local folder or saved workspace file.
///
/// # Errors
/// Returns an error if the launch path or its filesystem identity is unavailable.
pub fn cursor_workspace_id(workspace: &Path) -> Result<String> {
    ensure!(
        workspace.is_absolute() && !omnis_core::may_reach_network(workspace),
        "Cursor IDE workspace must be a local absolute path"
    );
    let metadata = fs::metadata(workspace).context("reading Cursor IDE workspace metadata")?;
    let path = workspace
        .to_str()
        .context("Cursor IDE workspace path must be UTF-8")?;
    #[cfg(windows)]
    let path = {
        let ordinary = if path.starts_with(r"\\?\") {
            omnis_core::ordinary_windows_path(path)
                .context("Cursor IDE workspace requires an ordinary drive path")?
        } else {
            path.to_owned()
        };
        let mut normalized = ordinary.replace('/', "\\");
        if normalized.as_bytes().get(1) == Some(&b':') {
            normalized[..1].make_ascii_lowercase();
        }
        normalized
    };
    let mut digest = Md5::new();
    if metadata.is_file() {
        #[cfg(not(target_os = "linux"))]
        let path = path.to_lowercase();
        digest.update(path.as_bytes());
    } else {
        ensure!(
            metadata.is_dir(),
            "Cursor IDE workspace is not a folder or a regular file"
        );
        digest.update(path.as_bytes());
        let salt = folder_salt(&metadata)?;
        if salt != "0" {
            digest.update(salt.as_bytes());
        }
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps, clippy::cast_precision_loss)]
fn folder_salt(metadata: &fs::Metadata) -> Result<String> {
    use std::os::unix::fs::MetadataExt;

    // Node exposes the inode as a JavaScript Number, even when it exceeds 53 bits.
    Ok((metadata.ino() as f64).to_string())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn folder_salt(metadata: &fs::Metadata) -> Result<String> {
    let created = metadata.created()?.duration_since(std::time::UNIX_EPOCH)?;
    #[cfg(target_os = "macos")]
    let millis = node_birth_millis(created).round();
    #[cfg(target_os = "windows")]
    let millis = node_birth_millis(created).floor();
    Ok(millis.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn folder_salt(_metadata: &fs::Metadata) -> Result<String> {
    anyhow::bail!("the native Cursor IDE folder identity is unknown on this operating system")
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
#[allow(clippy::cast_precision_loss)]
fn node_birth_millis(created: std::time::Duration) -> f64 {
    // Node builds fs.Stats.birthtimeMs from seconds and nanoseconds before the platform rounds it.
    created.as_secs() as f64 * 1_000.0 + f64::from(created.subsec_nanos()) / 1_000_000.0
}

/// Checks the launch path against the workspace ID in the native composer header.
/// Old headers without a native-format ID return `Ok(false)`.
///
/// # Errors
/// Returns an error if a known native workspace cannot be verified at the launch path.
pub fn validate_cursor_workspace_identity(
    snapshot: &CanonicalSnapshot,
    workspace: &Path,
) -> Result<bool> {
    ensure!(
        snapshot.session.provider == Provider::CursorIde,
        "expected a Cursor IDE session"
    );
    ensure!(
        workspace.is_absolute() && !omnis_core::may_reach_network(workspace),
        "Cursor IDE workspace must be a local absolute path"
    );
    let Some(header) = snapshot
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ProviderEvent
                && event.source.provider == Provider::CursorIde
                && event.source.raw_record_type.as_deref() == Some("composerHeader")
        })
        .map(|event| &event.payload["metadata"])
    else {
        return Ok(false);
    };
    ensure!(
        header["workspace_unavailable"] != true,
        "Cursor IDE did not record an unambiguous local workspace for this chat; use `omni export` to recover the chat history"
    );
    if let Some(kind) = header["workspace_kind"].as_str() {
        ensure!(
            (kind == "folder" && workspace.is_dir())
                || (kind == "workspace" && workspace.is_file()),
            "the launch path no longer has the type recorded by Cursor IDE; use `omni export` to recover the chat history"
        );
    }
    let Some(id) = header["workspace_id"]
        .as_str()
        .filter(|id| is_cursor_workspace_id(id))
    else {
        return Ok(false);
    };
    let actual = cursor_workspace_id(workspace).context("cannot verify the recorded Cursor IDE workspace; use `omni export` to recover the chat history")?;
    ensure!(
        actual == id,
        "the launch path has a different Cursor IDE workspace ID than the original chat; use `omni export` to recover the chat history"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::node_birth_millis;
    use std::time::Duration;

    #[test]
    fn cursor_workspace_birth_time_rounds_to_nearest_millisecond() {
        for (nanos, macos, windows) in [
            (49_534_497, "1769474943050", "1769474943049"),
            (49_499_999, "1769474943050", "1769474943049"),
            (49_499_800, "1769474943049", "1769474943049"),
            (49_999_999, "1769474943050", "1769474943050"),
        ] {
            let millis = node_birth_millis(Duration::new(1_769_474_943, nanos));
            assert_eq!(millis.round().to_string(), macos);
            assert_eq!(millis.floor().to_string(), windows);
        }
    }
}
