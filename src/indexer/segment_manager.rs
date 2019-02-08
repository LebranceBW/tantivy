use super::segment_register::SegmentRegister;
use core::SegmentId;
use core::SegmentMeta;
use core::META_FILEPATH;
use error::TantivyError;
use indexer::delete_queue::DeleteCursor;
use indexer::SegmentEntry;
use std::collections::hash_set::HashSet;
use std::fmt::{self, Debug, Formatter};
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::{RwLockReadGuard, RwLockWriteGuard};
use Result as TantivyResult;

#[derive(Default)]
struct SegmentRegisters {
    uncommitted: SegmentRegister,
    committed: SegmentRegister,
    // soft commits can advance committed segment to a future delete
    // opstamp.
    //
    // In that case the same `SegmentId` can appear in both `committed`
    // and in `committed_in_the_future`.
    //
    // TODO: which one should be considered for merges?
    committed_in_the_future: SegmentRegister
}

/// The segment manager stores the list of segments
/// as well as their state.
///
/// It guarantees the atomicity of the
/// changes (merges especially)
#[derive(Default)]
pub struct SegmentManager {
    registers: RwLock<SegmentRegisters>,
}

impl Debug for SegmentManager {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        let lock = self.read();
        write!(
            f,
            "{{ uncommitted: {:?}, committed: {:?} }}",
            lock.uncommitted, lock.committed
        )
    }
}

pub fn get_mergeable_segments(
    in_merge_segment_ids: &HashSet<SegmentId>,
    segment_manager: &SegmentManager,
) -> (Vec<SegmentMeta>, Vec<SegmentMeta>) {
    let registers_lock = segment_manager.read();
    (
        registers_lock
            .committed
            .get_mergeable_segments(in_merge_segment_ids),
        registers_lock
            .uncommitted
            .get_mergeable_segments(in_merge_segment_ids),
    )
}

impl SegmentManager {
    pub fn from_segments(
        segment_metas: Vec<SegmentMeta>,
        delete_cursor: &DeleteCursor,
    ) -> SegmentManager {
        SegmentManager {
            registers: RwLock::new(SegmentRegisters {
                uncommitted: SegmentRegister::default(),
                committed: SegmentRegister::new(segment_metas, delete_cursor),
                committed_in_the_future: SegmentRegister::default()
            }),
        }
    }

    /// Returns all of the segment entries (committed or uncommitted)
    pub fn segment_entries(&self) -> Vec<SegmentEntry> {
        let registers_lock = self.read();
        let mut segment_entries = registers_lock.uncommitted.segment_entries();
        segment_entries.extend(registers_lock.committed.segment_entries());
        segment_entries
    }

    /// List the files that are useful to the index.
    ///
    /// This does not include lock files, or files that are obsolete
    /// but have not yet been deleted by the garbage collector.
    pub fn list_files(&self) -> HashSet<PathBuf> {
        let mut files = HashSet::new();
        files.insert(META_FILEPATH.clone());
        for segment_meta in SegmentMeta::all() {
            files.extend(segment_meta.list_files());
        }
        files
    }

    // Lock poisoning should never happen :
    // The lock is acquired and released within this class,
    // and the operations cannot panic.
    fn read(&self) -> RwLockReadGuard<SegmentRegisters> {
        self.registers
            .read()
            .expect("Failed to acquire read lock on SegmentManager.")
    }

    fn write(&self) -> RwLockWriteGuard<SegmentRegisters> {
        self.registers
            .write()
            .expect("Failed to acquire write lock on SegmentManager.")
    }

    /// Deletes all empty segments
    fn remove_empty_segments(&self) {
        let mut registers_lock = self.write();
        registers_lock
            .committed
            .segment_entries()
            .iter()
            .filter(|segment| segment.meta().num_docs() == 0)
            .for_each(|segment| {
                registers_lock
                    .committed
                    .remove_segment(&segment.segment_id())
            });
    }

    pub fn commit(&self, segment_entries: Vec<SegmentEntry>) {
        let mut registers_lock = self.write();
        registers_lock.committed.clear();
        registers_lock.committed_in_the_future.clear();
        registers_lock.uncommitted.clear();
        for segment_entry in segment_entries {
            registers_lock.committed.register_segment_entry(segment_entry);
        }
    }

    pub fn soft_commit(&self, segment_entries: Vec<SegmentEntry>) {
        let mut registers_lock = self.write();
        for segment_entry in segment_entries {
            let segment_id = segment_entry.segment_id();
            if let Some(committed_segment_entry) = registers_lock.committed.get(&segment_id) {
                // this is a committed segment.
                if committed_segment_entry.meta().delete_opstamp() == segment_entry.meta().delete_opstamp() {
                    // Actually, there was no change made to the segment...No need to do anything.
                    continue;
                }
                // Our `segment_entry` is a commited in which *future* deletes (as in, sent after the last
                // commit)
                // Let's append it to a dedicated register for that.
                registers_lock.committed_in_the_future.register_segment_entry(segment_entry);
                // TODO make sure we use `committed_in_the_future` segments,
                // when we `commit`, to avoid replaying deletes several times.

            } else if registers_lock.uncommitted.get(&segment_id).is_some() {
                // This will override our previous entry.
                registers_lock.uncommitted.register_segment_entry(segment_entry);
            }
        }
    }

    /// Marks a list of segments as in merge.
    ///
    /// Returns an error if some segments are missing, or if
    /// the `segment_ids` are not either all committed or all
    /// uncommitted.
    pub fn start_merge(&self, segment_ids: &[SegmentId]) -> TantivyResult<Vec<SegmentEntry>> {
        let registers_lock = self.read();
        let mut segment_entries = vec![];
        if registers_lock.uncommitted.contains_all(segment_ids) {
            for segment_id in segment_ids {
                let segment_entry = registers_lock.uncommitted
                    .get(segment_id)
                    .expect("Segment id not found {}. Should never happen because of the contains all if-block.");
                segment_entries.push(segment_entry);
            }
        } else if registers_lock.committed.contains_all(segment_ids) {
            for segment_id in segment_ids {
                let segment_entry = registers_lock.committed
                    .get(segment_id)
                    .expect("Segment id not found {}. Should never happen because of the contains all if-block.");
                segment_entries.push(segment_entry);
            }
        } else {
            let error_msg = "Merge operation sent for segments that are not \
                             all uncommited or commited."
                .to_string();
            return Err(TantivyError::InvalidArgument(error_msg));
        }
        Ok(segment_entries)
    }

    pub fn add_segment(&self, segment_entry: SegmentEntry) {
        let mut registers_lock = self.write();
        registers_lock.uncommitted.register_segment_entry(segment_entry);
    }

    pub fn end_merge(
        &self,
        before_merge_segment_ids: &[SegmentId],
        after_merge_segment_entry: SegmentEntry,
    ) {
        let mut registers_lock = self.write();
        let target_register: &mut SegmentRegister = {
            if registers_lock
                .uncommitted
                .contains_all(before_merge_segment_ids)
            {
                &mut registers_lock.uncommitted
            } else if registers_lock
                .committed
                .contains_all(before_merge_segment_ids)
            {
                &mut registers_lock.committed
            } else {
                warn!("couldn't find segment in SegmentManager");
                return;
            }
        };
        for segment_id in before_merge_segment_ids {
            target_register.remove_segment(segment_id);
        }
        target_register.register_segment_entry(after_merge_segment_entry);
    }

    pub fn committed_segment_metas(&self) -> Vec<SegmentMeta> {
        self.remove_empty_segments();
        let registers_lock = self.read();
        registers_lock.committed.segment_metas()
    }
}
