use std::{
    io::{self, Read},
    path::Path,
};

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

use super::{
    CodexAdapter, HistoryScope, Position, PrefixRead, RolloutCatalog, Segment, observed,
    read_prefix_from,
};
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
    ) -> Result<Option<Self>> {
        verified_copy(
            home,
            catalog,
            &self.path,
            self.position,
            remaining,
            |path| {
                let visible_start = self.base.visible_start(
                    path,
                    HistoryScope::Visible,
                    self.subagent,
                    Some(self.position.end_byte_offset),
                )?;
                Ok(Some(Self {
                    path: path.to_owned(),
                    visible_start: Some(visible_start),
                    ..self.clone()
                }))
            },
        )
    }

    fn check_copy(
        mut self,
        revision: omnis_ir::SourceRevision,
    ) -> Result<Option<(Self, [u8; 32])>> {
        self.verified_revision = Some(revision);
        let mut hash = Sha256::new();
        let mut reader = DigestReader {
            reader: rollout_file::open(&self.path)?,
            hash: &mut hash,
        };
        let result = read_prefix_from(&self, &mut reader, &mut |_, _| Ok(()))?;
        ensure!(
            CodexAdapter::index_file_revision(&self.path) == Some(revision),
            "alternate Codex source rollout changed while verifying its bookmark"
        );
        Ok(matches!(result, PrefixRead::Complete(0)).then(|| (self, hash.finalize().into())))
    }
}

pub(super) fn verified_copy(
    home: &Path,
    catalog: &RolloutCatalog<'_>,
    original: &Path,
    position: Position,
    remaining: &mut u64,
    mut make: impl FnMut(&Path) -> Result<Option<Segment>>,
) -> Result<Option<Segment>> {
    let Some(revision) = CodexAdapter::index_file_revision(original) else {
        return Ok(None);
    };
    let mut selected: Option<(Segment, [u8; 32])> = None;
    let mut uncertain = false;
    let mut attempts = 0;
    let id = position.thread_id.to_string();
    let visited = rollout_identity::visit_immutable_copies(home, &id, Some(catalog), |path| {
        if uncertain
            || path == original
            || rollout_identity::thread_id(path) != rollout_identity::thread_id(original)
        {
            return Ok(());
        }
        attempts += 1;
        if attempts > 16 || position.end_byte_offset > *remaining {
            uncertain = true;
            return Ok(());
        }
        *remaining -= position.end_byte_offset;
        let Some(copy_revision) = CodexAdapter::index_file_revision(path) else {
            return Ok(());
        };
        let Ok(Some(candidate)) = make(path) else {
            return Ok(());
        };
        let Ok(Some((copy, hash))) = candidate.check_copy(copy_revision) else {
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
    });
    let Some((copy, _)) = selected.filter(|_| visited.is_ok() && !uncertain) else {
        return Ok(None);
    };
    let agrees = observed::agrees(original, &copy.path, position)?;
    Ok((agrees
        && CodexAdapter::index_file_revision(original) == Some(revision)
        && CodexAdapter::index_file_revision(&copy.path) == copy.verified_revision)
        .then_some(copy))
}
