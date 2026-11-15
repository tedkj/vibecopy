//! A job: what to copy or move, where, how, and every file's state. The whole queue lives in
//! `%LOCALAPPDATA%\com.tedkj.vibecopy\queue.json` while anything in it is unfinished, so a
//! restart (or a crash) offers it again; a paused file's part is named there, so its copy goes
//! on from where it stopped. Written whole-or-not-at-all, off the UI thread, newest wins; a file
//! that cannot be read right now is never written over, and one that cannot be parsed is kept
//! aside (`queue-<time>.broken.json`), never silently replaced.

use crate::conflict::{Choice, Policy};
use crate::paths::{self, serde_path};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Copy,
    Move,
}

/// Where a job stands in the queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Waiting for its drive (or a free slot).
    #[default]
    Queued,
    Running,
    /// By the user (or offered again after a restart).
    Paused,
    /// It cannot go on until something is done about it (the disk is full, the destination
    /// went away): `message` says what. Resume tries again.
    Stopped,
    /// Only files that wait for an answer are left.
    Waiting,
    Done,
    Cancelled,
}

impl State {
    pub fn finished(self) -> bool {
        matches!(self, State::Done | State::Cancelled)
    }
}

/// The step a file failed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    /// Looking at the source.
    Read,
    Copy,
    Verify,
    /// Putting the copy in its place (or a folder there).
    Place,
    /// A move's original could not be removed (the copy is in place).
    RemoveSource,
    /// Its folder failed.
    Folder,
}

/// How a file or folder got done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum How {
    /// Copied, checked by size and time.
    Copied,
    /// Copied, checked by SHA-256 too.
    Hashed,
    /// Moved by a rename on the same drive (nothing copied).
    Renamed,
    /// Copied to the other drive, checked by size and time, the original removed.
    Moved,
    /// The same, checked by SHA-256 too.
    MovedHashed,
    /// Found moved already after a restart: the copy there and the original gone, nothing of it
    /// checked again (RVCD4-7).
    MovedEarlier,
    /// A folder made.
    Made,
    /// A folder that was there already (its files merged in).
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemState {
    Pending,
    /// Already there; waiting for the user's answer.
    Conflict,
    Done(How),
    Skipped(String),
    Failed { stage: Stage, reason: String },
}

impl ItemState {
    pub fn settled(&self) -> bool {
        matches!(self, ItemState::Done(_) | ItemState::Skipped(_) | ItemState::Failed { .. })
    }
    pub fn failed(&self) -> bool {
        matches!(self, ItemState::Failed { .. })
    }
}

/// One file or folder of a job, in walk order (a folder before what is in it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Where it is (the short form), kept for the top items only: the others are their folder's
    /// path and their name (`run::src_of`), which saves a whole path per file.
    #[serde(default, with = "serde_path::opt", skip_serializing_if = "Option::is_none")]
    pub src: Option<PathBuf>,
    /// Where it goes, under the destination (the top name, then the rest).
    #[serde(with = "serde_path")]
    pub rel: PathBuf,
    pub dir: bool,
    pub size: u64,
    pub mtime: u64,
    pub depth: u16,
    pub state: ItemState,
    /// The copy in progress under its temporary name: a paused part, or (with `whole`) a
    /// complete copy not yet checked.
    #[serde(default, with = "serde_path::opt", skip_serializing_if = "Option::is_none")]
    pub tmp: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub whole: bool,
    /// The user's answer to its conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Choice>,
    /// Keep both: the full path it got instead of its own name.
    #[serde(default, with = "serde_path::opt", skip_serializing_if = "Option::is_none")]
    pub renamed: Option<PathBuf>,
    /// A move whose copy is in place; only the original is left to remove. It stays while a
    /// retried move copies again over its stale copy, until the new copy is in place (RVC2-1).
    #[serde(default, skip_serializing_if = "is_false")]
    pub placed: bool,
    /// With `placed`: the size and time of the original the copy was made from (read at copy
    /// time, not the count's). A retried move copies again over a copy only while it is still
    /// so, up to the replace itself (RVA12, RVC2-1); a queue saved without it keeps both files
    /// rather than guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made: Option<crate::conflict::Meta>,
    /// Bytes in the paused part (shown as progress after a pause or a restart).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub part: u64,
    /// A conflict: the size and time of the file that is there (for the question).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub there: Option<crate::conflict::Meta>,
    /// The count could not read it (a folder it could not list, or a picked item it could not
    /// look at), so what is in it is not known (FS1). A run reads it again before anything is
    /// made for it (`scan::rewalk`); it is never made empty and called done.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unwalked: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Item {
    /// Its source path, once `run` has filled it in (empty otherwise).
    pub fn path(&self) -> &Path {
        self.src.as_deref().unwrap_or(Path::new(""))
    }

    pub fn new(src: PathBuf, rel: PathBuf, dir: bool, size: u64, mtime: u64, depth: u16) -> Item {
        let src = if depth == 0 { Some(src) } else { None };
        Item { src, rel, dir, size, mtime, depth, state: ItemState::Pending, tmp: None, whole: false, answer: None, renamed: None, placed: false, made: None, part: 0, there: None, unwalked: false }
    }
}

/// What the running job is doing right now (not saved).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub phase: Phase,
    /// The file being worked on (relative, as shown).
    pub current: String,
    /// Its index in the job's items: the list marks that row (two names that show alike are
    /// still two rows; review 2, R2-COPY-5).
    pub item: Option<usize>,
    /// Bytes of it copied so far.
    pub cur_done: u64,
    /// Bytes copied through the disk in this session (the speed is measured from this).
    pub copied: u64,
    /// While counting: files and bytes found so far.
    pub found_files: u64,
    pub found_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Counting,
    Copying,
    Verifying,
    Removing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    #[serde(with = "serde_path::vec")]
    pub sources: Vec<PathBuf>,
    #[serde(with = "serde_path")]
    pub dest: PathBuf,
    pub mode: Mode,
    pub verify: bool,
    pub policy: Policy,
    pub state: State,
    /// Why it stopped (Stopped), in words.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// The sources have been walked (`items` is the whole list).
    pub scanned: bool,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(skip)]
    pub live: Live,
    /// Bumped on every change of an item (the window redraws a file list only when it moved).
    #[serde(skip)]
    pub rev: u64,
    /// Saved while it ran (a crash, or a quit whose worker did not stop in time) and read back:
    /// queue.json lags the disk by a few seconds then, so its run takes its own copies, parts and finished moves it finds as
    /// done instead of doing them again (`run`).
    #[serde(skip)]
    pub restored: bool,
    /// Bumped when items are put in between others (a retry's walk, FS1): an index taken before
    /// may name another item now (the open question checks it).
    #[serde(skip)]
    pub layout: u64,
}

impl Job {
    pub fn new(id: u64, sources: Vec<PathBuf>, dest: PathBuf, mode: Mode, verify: bool, policy: Policy) -> Job {
        Job { id, sources, dest, mode, verify, policy, state: State::Queued, message: String::new(), scanned: false, items: Vec::new(), live: Live::default(), rev: 0, restored: false, layout: 0 }
    }

    pub fn tally(&self) -> Tally {
        let mut t = Tally::default();
        for it in &self.items {
            if it.depth == 0 {
                if it.dir {
                    t.top_dirs += 1;
                } else {
                    t.top_files += 1;
                }
            }
            if it.dir {
                t.dirs += 1;
            } else {
                t.files += 1;
                t.bytes += it.size;
            }
            match &it.state {
                ItemState::Pending => t.part_bytes += it.part,
                ItemState::Conflict => t.conflicts += 1,
                ItemState::Done(h) => {
                    if !it.dir {
                        t.done += 1;
                        t.settled_bytes += it.size;
                        match h {
                            How::Hashed | How::MovedHashed => t.hashed += 1,
                            How::Renamed => t.renamed += 1,
                            How::MovedEarlier => t.unchecked += 1,
                            _ => t.quick += 1,
                        }
                    } else if *h == How::Renamed {
                        t.renamed_dirs += 1;
                    }
                }
                ItemState::Skipped(_) => {
                    if !it.dir {
                        t.skipped += 1;
                        t.settled_bytes += it.size;
                    }
                }
                ItemState::Failed { .. } => {
                    // A folder that failed counts once; its files count as failed themselves.
                    t.failed += 1;
                    if !it.dir {
                        t.failed_files += 1;
                        t.settled_bytes += it.size;
                    }
                }
            }
        }
        if !self.scanned {
            t.top_files = 0;
            t.top_dirs = 0;
        }
        t
    }
}

/// A job's numbers, counted from its items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// What the user picked: files and folders at the top.
    pub top_files: u64,
    pub top_dirs: u64,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Files done (copied or moved).
    pub done: u64,
    pub skipped: u64,
    /// Files and folders that failed.
    pub failed: u64,
    pub failed_files: u64,
    pub conflicts: u64,
    /// Bytes of files that are done, skipped or failed (for the progress line).
    pub settled_bytes: u64,
    /// Bytes in paused parts.
    pub part_bytes: u64,
    /// How the done files were checked.
    pub hashed: u64,
    pub quick: u64,
    pub renamed: u64,
    /// Moves found done after a restart, not checked again (`How::MovedEarlier`).
    pub unchecked: u64,
    pub renamed_dirs: u64,
}

impl Tally {
    /// Files not done, skipped or failed yet.
    pub fn left(&self) -> u64 {
        self.files.saturating_sub(self.done + self.skipped + self.failed_files)
    }
}

pub type Shared = Arc<Mutex<Job>>;

// ---------- the queue on disk ----------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    pub next_id: u64,
    pub jobs: Vec<Job>,
    /// Items left out as read back: they named a place outside their destination, or were
    /// damaged (`sound`, IN-VibeCopy-1). The app says so.
    #[serde(skip)]
    pub dropped: usize,
}

/// A job read back from queue.json may be acted on only as far as it names places the app
/// made (IN-VibeCopy-1: a same-user tamper, or damage). Its destination and sources are full
/// paths. Each item's `rel` is plain names (`paths::plain_rel`); its part (`tmp`) and "Keep
/// both" name (`renamed`) lie under the destination by plain names, a part with the app's part
/// name (`.vibecopy-<id>-<n>[-<k>].part`); a top item names its source: one of the job's, or
/// below one by plain names. Cancel deletes parts, a resume writes into them, a move removes its sources.
fn sound_job(j: &Job) -> bool {
    j.dest.is_absolute() && j.sources.iter().all(|s| s.is_absolute())
}

fn sound_item(j: &Job, it: &Item) -> bool {
    let under = |p: &Path| paths::below(p, &j.dest).is_some_and(|r| paths::plain_rel(&r));
    let part = |p: &Path| {
        p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(".vibecopy-")).and_then(|n| n.strip_suffix(".part")).is_some_and(|n| {
            let parts: Vec<&str> = n.split('-').collect();
            (2..=3).contains(&parts.len()) && parts.iter().all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    let src_ok = match (&it.src, it.depth) {
        (None, 0) => false,
        (None, _) => true,
        // One of the job's sources, or below one by plain names (RIN2-6): `D:\Photos\..\x` is
        // not inside `D:\Photos`, though its text begins so.
        (Some(s), _) => s.is_absolute() && j.sources.iter().any(|x| paths::same_path(s, x) || paths::below(s, x).is_some_and(|r| paths::plain_rel(&r))),
    };
    paths::plain_rel(&it.rel) && src_ok && it.tmp.as_deref().is_none_or(|t| under(t) && part(t)) && it.renamed.as_deref().is_none_or(under)
}

/// `j`'s items without the unsound ones (`sound_item`), each with what is inside it (a folder's
/// items name their places through it). How many went.
fn keep_sound(j: &mut Job) -> usize {
    let mut gone = 0;
    let mut i = 0;
    while i < j.items.len() {
        if sound_item(j, &j.items[i]) {
            i += 1;
            continue;
        }
        let end = crate::scan::subtree(&j.items, i).end;
        gone += end - i;
        j.items.drain(i..end);
    }
    gone
}

pub enum Loaded {
    /// No queue (nothing unfinished last time).
    Nothing,
    Jobs(Saved),
    /// It could not be parsed; kept aside under this name.
    Broken(PathBuf),
    /// It is there but cannot be read right now (it is never written over while so).
    Unreadable(String),
}

pub fn load(path: &Path) -> Loaded {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Nothing,
        Err(e) => return Loaded::Unreadable(crate::copy::describe(&e)),
    };
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(mut s) => {
            // Only what names places the app made is used (IN-VibeCopy-1).
            let mut dropped = 0;
            s.jobs.retain(|j| {
                let ok = sound_job(j);
                if !ok {
                    dropped += j.items.len().max(1);
                }
                ok
            });
            for j in &mut s.jobs {
                dropped += keep_sound(j);
            }
            s.dropped = dropped;
            // Unfinished work comes back paused: it is offered, never started by itself.
            for j in &mut s.jobs {
                // Only a job saved while its worker ran can be behind the disk; every other
                // state is written after its worker ended, exactly (COR-1).
                j.restored = j.state == State::Running;
                if matches!(j.state, State::Queued | State::Running | State::Waiting) {
                    j.state = State::Paused;
                }
                // A paused part's whole flag stays; a file that was mid-check is checked again.
                for it in &mut j.items {
                    if it.state == ItemState::Conflict && it.answer.is_some() {
                        it.state = ItemState::Pending;
                    }
                    // A folder that could not be listed, from a queue written before the flag
                    // (FS1): the same as one with it.
                    if it.dir && matches!(it.state, ItemState::Failed { stage: Stage::Read, .. }) {
                        it.unwalked = true;
                    }
                }
            }
            s.jobs.retain(|j| !j.state.finished());
            let top = s.jobs.iter().map(|j| j.id).max().unwrap_or(0);
            s.next_id = s.next_id.max(top + 1);
            Loaded::Jobs(s)
        }
        Err(_) => {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let aside = path.with_file_name(format!("queue-{stamp}.broken.json"));
            match std::fs::rename(path, &aside) {
                Ok(()) => Loaded::Broken(aside),
                Err(e) => Loaded::Unreadable(crate::copy::describe(&e)),
            }
        }
    }
}

/// Writes the queue off the UI thread, newest wins. `blocked`: the file was there but could not
/// be read at start: nothing is written over it until it can be read.
#[derive(Clone)]
pub struct Saver {
    path: PathBuf,
    pending: Arc<Mutex<Option<Vec<u8>>>>,
    /// Held while writing; holds the number of the newest queue written.
    write: Arc<Mutex<u64>>,
    pub blocked: Arc<std::sync::atomic::AtomicBool>,
    /// A guest's saver (RI2-5): another VibeCopy owns the queue file, so this one never writes it.
    guest: bool,
}

impl Saver {
    pub fn new(path: PathBuf) -> Saver {
        Saver { path, pending: Default::default(), write: Default::default(), blocked: Default::default(), guest: false }
    }

    /// A saver that never writes `path` (a guest's, RI2-5): the VibeCopy with the queue lock is
    /// its only writer.
    pub fn guest(path: PathBuf) -> Saver {
        Saver { guest: true, ..Saver::new(path) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The jobs worth keeping: everything unfinished. None left: the file goes.
    pub fn encode(next_id: u64, jobs: &[Job]) -> Option<Vec<u8>> {
        let keep: Vec<&Job> = jobs.iter().filter(|j| !j.state.finished()).collect();
        if keep.is_empty() {
            return None;
        }
        #[derive(Serialize)]
        struct Out<'a> {
            next_id: u64,
            jobs: Vec<&'a Job>,
        }
        serde_json::to_vec(&Out { next_id, jobs: keep }).ok()
    }

    /// Remember this queue (empty: nothing unfinished, the file is removed).
    pub fn save(&self, body: Option<Vec<u8>>) {
        *self.pending.lock().unwrap() = Some(body.unwrap_or_default());
        let me = self.clone();
        std::thread::spawn(move || me.flush());
    }

    /// Write what is pending, now (also at exit).
    pub fn flush(&self) {
        let _w = self.write.lock().unwrap();
        let Some(body) = self.pending.lock().unwrap().clone() else { return };
        let ok = self.put(&body);
        let mut p = self.pending.lock().unwrap();
        if ok && p.as_ref() == Some(&body) {
            *p = None;
        }
    }

    /// Write queue number `seq` now, on this thread, unless a newer one was written already
    /// (two writers never put an older queue over a newer one). None: nothing unfinished.
    pub fn write(&self, seq: u64, body: Option<Vec<u8>>) -> bool {
        let mut last = self.write.lock().unwrap();
        if seq <= *last {
            return true;
        }
        let ok = self.put(&body.unwrap_or_default());
        if ok {
            *last = seq;
        }
        ok
    }

    /// Whole or not at all; never over a file that could not be read at start. Only the app
    /// unblocks it, once it has read the file and taken its jobs in (a file that merely opens
    /// again still holds jobs this queue never had). A guest's never writes or removes it.
    fn put(&self, body: &[u8]) -> bool {
        if self.guest || self.blocked.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        if body.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            }
        } else {
            crate::prefs::write_atomic(&self.path, body).is_ok()
        }
    }
}

/// "12 files, 3 folders from D:\Photos"; one thing by its name: "report.pdf from D:\Docs".
pub fn title(job: &Job, t: &Tally) -> String {
    let from = common_parent(&job.sources);
    let what = if !job.scanned || t.top_files + t.top_dirs == 0 {
        if job.sources.len() == 1 {
            paths::shown_name(&paths::top_name(&job.sources[0]))
        } else {
            crate::format::plural(job.sources.len() as u64, "item", "items")
        }
    } else if t.top_files + t.top_dirs == 1 {
        paths::shown_name(&paths::top_name(&job.sources[0]))
    } else {
        match (t.top_files, t.top_dirs) {
            (f, 0) => crate::format::files(f),
            (0, d) => crate::format::folders(d),
            (f, d) => format!("{}, {}", crate::format::files(f), crate::format::folders(d)),
        }
    };
    match from {
        Some(p) => format!("{what} from {}", paths::shown(&p)),
        None => format!("{what} from {} places", job.sources.len()),
    }
}

/// What was picked, without where from: "report.pdf", "12 files, 3 folders" (toasts).
pub fn what(job: &Job, t: &Tally) -> String {
    let full = title(job, t);
    match full.rfind(" from ") {
        Some(i) => full[..i].to_string(),
        None => full,
    }
}

/// The folder every source is in, if they share one.
pub fn common_parent(sources: &[PathBuf]) -> Option<PathBuf> {
    let first = paths::short(sources.first()?).parent().map(Path::to_path_buf).unwrap_or_else(|| paths::root_of(&sources[0]));
    let all = sources.iter().all(|s| {
        let p = paths::short(s).parent().map(Path::to_path_buf).unwrap_or_else(|| paths::root_of(s));
        paths::same_path(&p, &first)
    });
    all.then_some(first)
}
