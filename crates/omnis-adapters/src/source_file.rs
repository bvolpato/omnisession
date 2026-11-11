//! Metadata used to identify a transcript and detect changes to its contents.

use std::{fs, path::Path};

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use omnis_ir::SourceRevision;
use uuid::Uuid;

/// File identity and metadata captured together for a selected provider transcript.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ObservedSourceFile {
    pub(crate) revision: SourceRevision,
    #[cfg(any(unix, windows))]
    object: [u64; 2],
    pub(crate) modified: Option<DateTime<Utc>>,
}

impl ObservedSourceFile {
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let canonical = fs::canonicalize(path).ok()?;
        #[cfg(windows)]
        {
            let file = fs::File::open(&canonical).ok()?;
            Self::from_open_file(&canonical, &file, &file.metadata().ok()?)
        }
        #[cfg(not(windows))]
        Self::from_metadata(&canonical, &fs::metadata(&canonical).ok()?)
    }

    /// Uses the handle whose cached metadata and transcript bytes the reader observes.
    pub(crate) fn from_open_file(
        canonical: &Path,
        file: &fs::File,
        metadata: &fs::Metadata,
    ) -> Option<Self> {
        #[cfg(not(windows))]
        let _ = file;
        Self::from_metadata(
            canonical,
            metadata,
            #[cfg(windows)]
            file,
        )
    }

    /// Checks that the opened transcript still has the contents observed before the read.
    pub(crate) fn ensure_handle_unchanged(self, canonical: &Path, file: &fs::File) -> Result<()> {
        let after = file
            .metadata()
            .ok()
            .and_then(|metadata| Self::from_open_file(canonical, file, &metadata));
        ensure!(
            after.is_some_and(|after| after.revision == self.revision),
            "transcript changed during indexing"
        );
        Ok(())
    }

    /// Checks that the selected path still names the unchanged open transcript.
    pub(crate) fn ensure_unchanged(
        self,
        canonical: &Path,
        selected: &Path,
        file: &fs::File,
    ) -> Result<()> {
        self.ensure_handle_unchanged(canonical, file)?;
        ensure!(
            Self::read(selected).is_some_and(|current| current.revision == self.revision),
            "transcript changed during indexing"
        );
        Ok(())
    }

    pub(crate) fn revision_in_namespace(self, path: &Path) -> SourceRevision {
        #[cfg(any(unix, windows))]
        {
            #[cfg(windows)]
            if self.object[0] == u64::MAX {
                return self.revision;
            }
            let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_os_str().as_encoded_bytes());
            let file = Uuid::new_v5(&namespace, self.object.map(u64::to_le_bytes).as_flattened());
            SourceRevision {
                file: Some(file),
                ..self.revision
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            self.revision
        }
    }

    pub(crate) fn from_metadata(
        canonical: &Path,
        metadata: &fs::Metadata,
        #[cfg(windows)] file: &fs::File,
    ) -> Option<Self> {
        let path_id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            canonical.as_os_str().as_encoded_bytes(),
        );
        #[cfg(windows)]
        let object = {
            use std::os::windows::fs::MetadataExt;
            if let Ok(info) = winapi_util::file::information(file)
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
        Some(Self {
            #[cfg(any(unix, windows))]
            object,
            modified,
            revision: SourceRevision {
                legacy_file: None,
                file: Some(file),
                contents: contents_id(file, metadata, modified),
                missing_ancestor: None,
            },
        })
    }
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

/// Observes the current revision at a selected native path without reading transcript content.
#[must_use]
pub fn source_file_revision(path: &Path) -> Option<SourceRevision> {
    ObservedSourceFile::read(path).map(|file| file.revision)
}
