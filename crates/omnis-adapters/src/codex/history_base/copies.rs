use std::{
    io::{self, Read},
    path::Path,
};

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

use super::{CodexAdapter, HistoryScope, PrefixRead, RolloutCatalog, Segment, read_prefix_from};
use crate::codex::{rollout_file, rollout_identity};

struct DigestReader<'a> {
    reader: Box<dyn Read>,
    hash: &'a mut Sha256,
}

impl Read for DigestReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.reader.read(buf)?;
        self.hash.update(&buf[..size]);
        Ok(size)
    }
}

impl Segment {
    pub(super) fn verified_copy(
        &self,
        home: &Path,
        catalog: &RolloutCatalog<'_>,
        remaining: &mut u64,
    ) -> Option<Self> {
        let mut selected: Option<(Self, [u8; 32])> = None;
        let mut uncertain = false;
        let mut attempts = 0;
        let id = self.position.thread_id.to_string();
        rollout_identity::visit_immutable_copies(home, &id, Some(catalog), |path| {
            if uncertain
                || path == self.path
                || rollout_identity::thread_id(path) != rollout_identity::thread_id(&self.path)
            {
                return Ok(());
            }
            attempts += 1;
            if attempts > 16 || self.position.end_byte_offset > *remaining {
                uncertain = true;
                return Ok(());
            }
            *remaining -= self.position.end_byte_offset;
            let Ok(Some((copy, hash))) = self.check_copy(path) else {
                return Ok(());
            };
            if let Some((previous, expected)) = &selected {
                if hash != *expected {
                    uncertain = true;
                    return Ok(());
                }
                if previous.path <= copy.path {
                    return Ok(());
                }
            }
            selected = Some((copy, hash));
            Ok(())
        })
        .ok()?;
        selected.filter(|_| !uncertain).map(|(copy, _)| copy)
    }

    fn check_copy(&self, path: &Path) -> Result<Option<(Self, [u8; 32])>> {
        let Some(revision) = CodexAdapter::index_file_revision(path) else {
            return Ok(None);
        };
        let visible_start = self.base.visible_start(
            path,
            HistoryScope::Visible,
            self.subagent,
            Some(self.position.end_byte_offset),
        )?;
        let copy = Self {
            path: path.to_owned(),
            visible_start: Some(visible_start),
            verified_revision: Some(revision),
            ..self.clone()
        };
        let mut hash = Sha256::new();
        let mut reader = DigestReader {
            reader: rollout_file::open(path)?,
            hash: &mut hash,
        };
        let result = read_prefix_from(&copy, &mut reader, &mut |_, _| Ok(()))?;
        ensure!(
            CodexAdapter::index_file_revision(path) == Some(revision),
            "alternate Codex source rollout changed while verifying its bookmark"
        );
        Ok(matches!(result, PrefixRead::Complete(0)).then(|| (copy, hash.finalize().into())))
    }
}
