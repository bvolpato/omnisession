//! Metadata used to identify a transcript and detect changes to its contents.

use std::{fs, path::Path};

use chrono::{DateTime, Utc};
use omnis_ir::SourceRevision;
use uuid::Uuid;

/// Observes a selected native file without reading transcript content.
#[must_use]
pub fn source_file_revision(path: &Path) -> Option<SourceRevision> {
    source_file_revision_with_size(path).map(|(revision, _)| revision)
}

pub(crate) fn source_file_revision_with_size(path: &Path) -> Option<(SourceRevision, u64)> {
    let canonical = fs::canonicalize(path).ok()?;
    #[cfg(windows)]
    let file = fs::File::open(&canonical).ok()?;
    #[cfg(windows)]
    let metadata = file.metadata().ok()?;
    #[cfg(not(windows))]
    let metadata = fs::metadata(&canonical).ok()?;
    let path_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        canonical.as_os_str().as_encoded_bytes(),
    );
    #[cfg(windows)]
    let object = {
        use std::os::windows::fs::MetadataExt;
        if let Ok(info) = winapi_util::file::information(&file)
            && info.file_index() != 0
        {
            [info.volume_serial_number(), info.file_index()]
        } else {
            // Some network filesystems expose a creation time but no file index.
            [u64::MAX, metadata.creation_time()]
        }
    };
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    let object = {
        use std::os::unix::fs::MetadataExt;
        [metadata.dev(), metadata.ino()]
    };
    #[cfg(any(unix, windows))]
    let file = Uuid::new_v5(&path_id, object.map(u64::to_le_bytes).as_flattened());
    #[cfg(not(any(unix, windows)))]
    let file = path_id;
    let modified = metadata.modified().ok().map(DateTime::<Utc>::from);
    Some((
        SourceRevision {
            file: Some(file),
            contents: contents_id(file, &metadata, modified),
            missing_ancestor: None,
        },
        metadata.len(),
    ))
}

// Keep the content identity stable across the shared readers of this native file.
fn contents_id(
    file: Uuid,
    metadata: &fs::Metadata,
    modified: Option<DateTime<Utc>>,
) -> Option<Uuid> {
    let modified = modified?;
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        (metadata.ctime(), metadata.ctime_nsec())
    };
    #[cfg(not(unix))]
    let changed = {
        let created = DateTime::<Utc>::from(metadata.created().ok()?);
        (
            created.timestamp(),
            i64::from(created.timestamp_subsec_nanos()),
        )
    };
    let fields = [
        metadata.len().to_le_bytes(),
        modified.timestamp().to_le_bytes(),
        u64::from(modified.timestamp_subsec_nanos()).to_le_bytes(),
        changed.0.to_le_bytes(),
        changed.1.to_le_bytes(),
    ];
    Some(Uuid::new_v5(&file, fields.as_flattened()))
}
