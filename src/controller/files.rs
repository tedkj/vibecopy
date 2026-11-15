//! A job's file list for the window, made lazily: the list view asks for the rows it shows and
//! each is formatted from the job's items then. A job of 100 000 files costs an index per file,
//! not 100 000 rows of text. Failures first, then what waits for an answer, what is still to
//! do, what was skipped, what is done; walk order within each.

use crate::FileRow;
use crate::format;
use crate::job::{How, ItemState, Job, Phase, Shared};
use slint::{Model, ModelNotify, ModelTracker, SharedString};
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct FileModel {
    job: RefCell<Option<Shared>>,
    order: RefCell<Vec<u32>>,
    rev: Cell<u64>,
    notify: ModelNotify,
}

/// The group a file's state sorts into (lower first).
pub fn group(s: &ItemState) -> u8 {
    match s {
        ItemState::Failed { .. } => 0,
        ItemState::Conflict => 1,
        ItemState::Pending => 2,
        ItemState::Skipped(_) => 3,
        ItemState::Done(_) => 4,
    }
}

/// Items in the order the list shows them: a stable partition (no comparator, so any names).
pub fn ordered(j: &Job) -> Vec<u32> {
    let mut buckets: [Vec<u32>; 5] = Default::default();
    for (i, it) in j.items.iter().enumerate() {
        buckets[group(&it.state) as usize].push(i as u32);
    }
    buckets.concat()
}

/// A move found done after a restart (`How::MovedEarlier`): only what was seen is said.
pub const MOVED_EARLIER: &str = "moved earlier: the copy is there and the original is gone (not checked again after the restart)";

/// One item as a row.
pub fn row(j: &Job, i: usize) -> FileRow {
    let it = &j.items[i];
    let running = j.state == crate::job::State::Running;
    let current = running && it.state == ItemState::Pending && j.live.item == Some(i);
    let (state, word, note): (i32, &str, String) = match &it.state {
        ItemState::Pending if current => (2, if j.live.phase == Phase::Verifying { "Verifying" } else { "Copying" }, String::new()),
        ItemState::Pending if it.tmp.is_some() => (0, "Paused", "part copied, goes on from there".into()),
        ItemState::Pending => (0, "Waiting", String::new()),
        ItemState::Conflict => (5, "Asks", "already there; waiting for your answer".into()),
        ItemState::Done(h) => (
            1,
            match h {
                How::Copied => "Copied",
                How::Hashed => "Verified",
                How::Renamed => "Moved",
                How::Moved => "Moved",
                How::MovedHashed => "Moved",
                How::MovedEarlier => "Moved",
                How::Made => "Made",
                How::Merged => "Merged",
            },
            match h {
                How::Hashed => "SHA-256 matches".into(),
                How::Renamed => "renamed on the same drive".into(),
                How::MovedHashed => "copied, SHA-256 matches, original removed".into(),
                How::Moved => "copied, size and time match, original removed".into(),
                How::MovedEarlier => MOVED_EARLIER.into(),
                How::Merged => "the folder was there; files merged in".into(),
                _ => String::new(),
            },
        ),
        ItemState::Skipped(why) => (3, "Skipped", why.clone()),
        ItemState::Failed { reason, .. } => (4, "Failed", reason.clone()),
    };
    let mut note = note;
    if let Some(r) = &it.renamed
        && it.state.settled()
    {
        let n = crate::paths::shown_name(r.file_name().unwrap_or_default());
        note = if note.is_empty() { format!("kept both: copied as {n}") } else { format!("{note}; copied as {n}") };
    }
    let mut name = crate::paths::clean(&it.rel.to_string_lossy());
    if it.dir {
        name.push('\\');
    }
    FileRow { state, word: word.into(), name: name.into(), size: if it.dir { SharedString::new() } else { format::bytes(it.size).into() }, note: note.into() }
}

impl FileModel {
    /// Show this job (None: nothing).
    pub fn show(&self, job: Option<Shared>) {
        *self.job.borrow_mut() = job;
        self.rev.set(u64::MAX);
        self.update(true);
    }

    /// Catch up with the job (only when its items changed, unless `force`).
    pub fn update(&self, force: bool) {
        let order = match self.job.borrow().as_ref() {
            None => Vec::new(),
            Some(sh) => {
                let j = sh.lock().unwrap();
                if !force && j.rev == self.rev.get() {
                    return;
                }
                self.rev.set(j.rev);
                ordered(&j)
            }
        };
        *self.order.borrow_mut() = order;
        self.notify.reset();
    }
}

impl Model for FileModel {
    type Data = FileRow;
    fn row_count(&self) -> usize {
        self.order.borrow().len()
    }
    fn row_data(&self, row: usize) -> Option<FileRow> {
        let i = *self.order.borrow().get(row)? as usize;
        let job = self.job.borrow();
        let j = job.as_ref()?.lock().unwrap();
        (i < j.items.len()).then(|| self::row(&j, i))
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
