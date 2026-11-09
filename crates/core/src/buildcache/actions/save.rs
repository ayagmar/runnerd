//! One GitHub Actions cache save not yet an entry (ADR 0082, 0085): what of it is on disk at each stage of its life.
//! Only `Save`'s methods change that stage. It must not touch the disk or say where a save goes: what it lets go is
//! handed back for its caller to remove, and `actions` makes the entry.

use std::collections::HashMap;

use super::MAX_BLOCKS;
use crate::buildcache::Upload;

/// A save being uploaded: an entry of the job's own scope, once its archive is in, its job finalizes it and the engine
/// lets the job write. Its scope is never named before that: an undecided job has none.
///
/// Nothing of it survives a restart: the uploads directory is emptied at start, and a job's upload sent again reaches
/// no save (`State::next_upload`). A reload leaves it as it is.
#[derive(Debug)]
pub(in crate::buildcache) struct Save {
    key: String,
    version: String,
    /// The size limit when it started: its archive, whole or the sum of its blocks, takes at most a quarter of it.
    max_size: u64,
    state: SaveState,
}

#[derive(Debug)]
enum SaveState {
    /// Taking what its job sends.
    Receiving {
        /// Its blocks so far, by their ID, until a block list joins them.
        blocks: HashMap<String, Upload>,
        /// The archive, sent whole or joined from blocks.
        archive: Option<Upload>,
    },
    /// Its job finalized it before its writes were decided: its archive waits for the decision and takes no more.
    Finalized { archive: Upload },
    /// What it sent passed a limit, or its job may not write, and is gone: its finalize answers as stored.
    Dropped,
}

/// What a save is sent.
pub(super) enum Part {
    Block(String),
    Archive,
}

/// What came of a save's job finalizing it.
pub(super) enum Finished {
    /// Its archive is whole, and the save is finalized.
    Archived,
    /// Its archive is not the size its job says: it goes.
    WrongSize,
    /// It has no archive to finalize.
    Nothing,
}

impl Save {
    pub(super) fn new(key: String, version: String, max_size: u64) -> Save {
        Save {
            key,
            version,
            max_size,
            state: SaveState::Receiving {
                blocks: HashMap::new(),
                archive: None,
            },
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    pub(super) fn version(&self) -> &str {
        &self.version
    }

    pub(super) fn max_size(&self) -> u64 {
        self.max_size
    }

    /// Whether it still takes what its job sends: neither dropped nor finalized.
    pub(super) fn is_open(&self) -> bool {
        matches!(self.state, SaveState::Receiving { .. })
    }

    pub(super) fn is_dropped(&self) -> bool {
        matches!(self.state, SaveState::Dropped)
    }

    pub(super) fn is_finalized(&self) -> bool {
        matches!(self.state, SaveState::Finalized { .. })
    }

    /// Whether a finalize of `key` and `version` is for it: one with its archive in, or one dropped, which answers as
    /// stored. One finalized already is not.
    pub(super) fn answers_finalize(&self, key: &str, version: &str) -> bool {
        self.key == key
            && self.version == version
            && match &self.state {
                SaveState::Receiving { archive, .. } => archive.is_some(),
                SaveState::Finalized { .. } => false,
                SaveState::Dropped => true,
            }
    }

    /// Its job finalized it, saying its archive has `size` bytes when it says at all. Its blocks go, and its archive
    /// too if that size is wrong; what goes is put in `gone`.
    pub(super) fn finalize(&mut self, size: Option<u64>, gone: &mut Vec<Upload>) -> Finished {
        match std::mem::replace(&mut self.state, SaveState::Dropped) {
            SaveState::Receiving { blocks, archive } => {
                gone.extend(blocks.into_values());
                match archive {
                    Some(archive) if size.is_none_or(|size| size == archive.bytes) => {
                        self.state = SaveState::Finalized { archive };
                        Finished::Archived
                    }
                    Some(archive) => {
                        gone.push(archive);
                        Finished::WrongSize
                    }
                    None => Finished::Nothing,
                }
            }
            other => {
                self.state = other;
                Finished::Nothing
            }
        }
    }

    /// Keeps what it was sent, or, when `received` is `None` or passes a limit, drops it for good. What it no longer
    /// keeps is put in `gone`.
    pub(super) fn keep(&mut self, received: Option<Upload>, part: Part, gone: &mut Vec<Upload>) {
        let SaveState::Receiving { blocks, archive } = &mut self.state else {
            gone.extend(received);
            return;
        };
        match (received, part) {
            (Some(upload), Part::Block(block)) if fits(blocks, &upload, &block, self.max_size) => {
                gone.extend(blocks.insert(block, upload));
            }
            (Some(upload), Part::Archive) => gone.extend(archive.replace(upload)),
            (received, _) => {
                gone.extend(received);
                self.drop_uploads(gone);
            }
        }
    }

    /// Takes its blocks, for a block list to join.
    pub(super) fn take_blocks(&mut self) -> HashMap<String, Upload> {
        match &mut self.state {
            SaveState::Receiving { blocks, .. } => std::mem::take(blocks),
            SaveState::Finalized { .. } | SaveState::Dropped => HashMap::new(),
        }
    }

    /// Drops it: all it has goes in `gone`, and what is sent to it from now on is read and dropped.
    pub(super) fn drop_uploads(&mut self, gone: &mut Vec<Upload>) {
        gone.extend(std::mem::replace(&mut self.state, SaveState::Dropped).into_uploads());
    }

    /// Its archive, if it was finalized.
    pub(super) fn into_archive(self) -> Option<Upload> {
        match self.state {
            SaveState::Finalized { archive } => Some(archive),
            SaveState::Receiving { .. } | SaveState::Dropped => None,
        }
    }

    /// What of it is on disk, to be removed.
    pub(in crate::buildcache) fn into_uploads(self) -> Vec<Upload> {
        self.state.into_uploads()
    }
}

impl SaveState {
    fn into_uploads(self) -> Vec<Upload> {
        match self {
            SaveState::Receiving { blocks, archive } => {
                blocks.into_values().chain(archive).collect()
            }
            SaveState::Finalized { archive } => vec![archive],
            SaveState::Dropped => Vec::new(),
        }
    }
}

/// Whether `upload` fits among `blocks` as `block`: the blocks together are the archive, and take at most a quarter
/// of the limit, as a whole archive does.
fn fits(blocks: &HashMap<String, Upload>, upload: &Upload, block: &str, max_size: u64) -> bool {
    let others: u64 = blocks
        .iter()
        .filter(|(id, _)| id.as_str() != block)
        .map(|(_, upload)| upload.bytes)
        .sum();
    (blocks.len() < MAX_BLOCKS || blocks.contains_key(block))
        && others + upload.bytes <= max_size / 4
}
