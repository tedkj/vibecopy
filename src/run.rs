//! One job, run on a worker thread: walk the sources (once), then every item in walk order.
//!
//! A file is copied to a temporary name next to its target (`.vibecopy-<job>-<n>.part`),
//! checked (size and time always, SHA-256 of both sides with Verify), and only then renamed
//! into place: a new name never replaces anything, an overwrite replaces the old file in one
//! step. So a failed or interrupted copy never leaves half a file under the real name, and an
//! overwrite that fails keeps the old file. A move on the same drive is a rename; across drives
//! the original goes only when `move_::may_delete_source` says the checked copy is in place.
//!
//! Failures stay with their file (the job carries on); what stops the whole job is what would
//! fail every file after it: a full disk, the destination or the source drive gone. Pause stops
//! inside a file (the part stays and is resumed); cancel removes the part.

use crate::conflict::{self, Action, Meta};
use crate::copy::{self, BIG, Control, Copied, Opts, Stat, describe};
use crate::disk::Disk;
use crate::job::{How, ItemState, Job, Mode, Phase, Stage};
use crate::move_::{self, Evidence};
use crate::paths::{self, long};
use crate::verify;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Every item is settled.
    Finished,
    Paused,
    Cancelled,
    /// It cannot go on (the words say why); the unsettled items wait for a resume.
    Stopped(String),
    /// Only conflicts waiting for an answer are left.
    Waiting,
}

/// What the worker tells the app while it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// A file is waiting for an answer.
    Conflict,
    /// The item list was made (worth saving now).
    Walked,
}

/// Why processing stopped short of an item's end.
enum Stop {
    Paused,
    Cancelled,
    Halt(String),
}

pub const ORIGINAL_CHANGED: &str = "The original changed while it was copied";
pub const NOT_WHOLE: &str = "The copy is not whole";
pub const TIME_DIFFERS: &str = "The copy's modified time does not match the original's";
pub const MISMATCH: &str = "The copy does not match the original (SHA-256)";
pub const SAME_FILE: &str = "Already there: it is the same file";
/// Retry of a move whose copy was changed since it was made (FEA1).
pub const COPY_CHANGED: &str = "The copy was changed since it was made, so both files were kept: the original was not removed and the copy was not replaced";
pub const FOLDER_IN_WAY: &str = "A folder of that name is already there";
pub const FILE_IN_WAY: &str = "A file of that name is in the way";
/// Another copy took this file's temporary name just then (RI5-1); Retry picks the next one.
pub const PART_BUSY: &str = "Busy elsewhere: another copy took its temporary name just then. Retry it";

struct Run<'a> {
    job: &'a Mutex<Job>,
    disk: &'a dyn Disk,
    ctl: &'a Control,
    note: &'a (dyn Fn(Note) + Sync),
    id: u64,
    dest: PathBuf,
    mode: Mode,
    verify: bool,
    /// Per source drive: does a rename reach the destination? (None: not tried yet.)
    same_volume: HashMap<String, bool>,
    parents: Vec<Option<usize>>,
    /// Per item, the group of files of this job with the same target (`scan::twins`), and each
    /// group's items.
    twins: Vec<Option<u32>>,
    groups: HashMap<u32, Vec<usize>>,
    /// The items whose place could be a "Keep both" name (their name ends in " (n)") or is one
    /// (`renamed`), by the place's `path_key` (X15R2A-1). With `groups` this finds every item
    /// of the job that goes where another one does, without a map of every item's place.
    named: BTreeMap<paths::Key, Vec<usize>>,
    /// The app was told about a conflict in this run (once is enough: it finds the rest).
    told: bool,
    /// How far a copy's time may be from its original's: 2 s everywhere (FAT and exFAT round
    /// times, and a share does not always say what is behind it). The original then/now stays
    /// exact, so this never lets a changed original pass.
    slack: u64,
    /// The job was read back from queue.json (`Job::restored`).
    restored: bool,
}

/// Run `job` until it is finished, paused, cancelled, stopped or waiting for answers.
pub fn run(job: &Mutex<Job>, disk: &dyn Disk, ctl: &Control, note: &(dyn Fn(Note) + Sync)) -> End {
    let end = run_job(job, disk, ctl, note);
    // The run ended (a crash never gets here): the queue saved from now on is what the job
    // holds, so the next run goes by the usual rules, as it would after a clean quit and a
    // restart (review RVCD4-7).
    job.lock().unwrap().restored = false;
    end
}

fn run_job(job: &Mutex<Job>, disk: &dyn Disk, ctl: &Control, note: &(dyn Fn(Note) + Sync)) -> End {
    let (id, sources, dest, mode, verify, scanned, restored) = {
        let j = job.lock().unwrap();
        (j.id, j.sources.clone(), j.dest.clone(), j.mode, j.verify, j.scanned, j.restored)
    };
    // Never into itself: a folder copied into its own subfolder would copy its copies.
    for s in &sources {
        if paths::within(&dest, s) && disk.stat(&long(s)).map(|st| st.dir).unwrap_or(false) {
            return End::Stopped(format!("The destination is inside {}, one of the sources.", paths::shown(s)));
        }
    }
    if !scanned {
        set_live(job, |l| l.phase = Phase::Counting);
        let found = |f: u64, b: u64| {
            let mut j = job.lock().unwrap();
            j.live.found_files = f;
            j.live.found_bytes = b;
        };
        let Some(items) = crate::scan::scan(disk, &sources, ctl, &found) else {
            return if ctl.cancelled() { End::Cancelled } else { End::Paused };
        };
        let mut j = job.lock().unwrap();
        j.items = items;
        j.scanned = true;
        j.rev += 1;
        drop(j);
        note(Note::Walked);
    }
    // What the count could not read and a retry put back in line: read now, before anything
    // is made for it (FS1).
    if !rewalk(job, disk, ctl, note) {
        return if ctl.cancelled() { End::Cancelled } else { End::Paused };
    }
    // The destination folder: made if it is not there yet (the drive must be).
    let dl = long(&dest);
    match disk.stat(&dl) {
        Ok(st) if st.dir => {}
        Ok(_) => return End::Stopped(format!("{} is a file, not a folder.", paths::shown(&dest))),
        Err(_) => {
            if let Err(e) = std::fs::create_dir_all(&dl) {
                return End::Stopped(format!("{} cannot be made: {}.", paths::shown(&dest), describe(&e)));
            }
        }
    }
    let (parents, twins) = {
        let j = job.lock().unwrap();
        let parents = crate::scan::parents(&j.items);
        let twins = crate::scan::twins(&j.items, &parents);
        (parents, twins)
    };
    let mut groups: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, g) in twins.iter().enumerate() {
        if let Some(g) = g {
            groups.entry(*g).or_default().push(i);
        }
    }
    let slack = crate::conflict::SAME_TIME;
    let mut r = Run { job, disk, ctl, note, id, dest, mode, verify, same_volume: HashMap::new(), parents, twins, groups, named: BTreeMap::new(), told: false, slack, restored };
    r.index_named();
    set_live(job, |l| l.phase = Phase::Copying);
    let n = job.lock().unwrap().items.len();
    for i in 0..n {
        if let Some(end) = r.braked() {
            return end;
        }
        if let Err(s) = r.item(i) {
            return r.stopped(s);
        }
    }
    // Conflicts answered while the run went on (or settled by an "apply to all").
    loop {
        if let Some(end) = r.braked() {
            return end;
        }
        let next = {
            let j = job.lock().unwrap();
            j.items.iter().position(|it| it.state == ItemState::Conflict && (it.answer.is_some() || j.policy != conflict::Policy::Ask))
        };
        let Some(i) = next else { break };
        if let Err(s) = r.item(i) {
            return r.stopped(s);
        }
    }
    if job.lock().unwrap().items.iter().any(|it| it.state == ItemState::Conflict) {
        set_live(job, |l| l.phase = Phase::Idle);
        return End::Waiting;
    }
    if mode == Mode::Move {
        r.remove_moved_folders();
    }
    set_live(job, |l| {
        l.phase = Phase::Idle;
        l.current.clear();
        l.item = None;
    });
    End::Finished
}

/// FS1: every item the count could not read (`Item::unwalked`) that is pending again (Retry
/// failed) is read again; what is in it joins the list right after it, in walk order. One that
/// still cannot be read fails again, with nothing made. False when the brakes stopped it (the
/// list is then as it was).
fn rewalk(job: &Mutex<Job>, disk: &dyn Disk, ctl: &Control, note: &(dyn Fn(Note) + Sync)) -> bool {
    let mut from = 0;
    let mut changed = false;
    loop {
        let (i, it, src) = {
            let j = job.lock().unwrap();
            let Some(k) = j.items[from..].iter().position(|it| it.unwalked && it.state == ItemState::Pending) else { break };
            let i = from + k;
            let parents = crate::scan::parents(&j.items);
            (i, j.items[i].clone(), src_of(&j, &parents, i))
        };
        set_live(job, |l| l.phase = Phase::Counting);
        let Some(walked) = crate::scan::rewalk(disk, &it, &src, ctl) else { return false };
        let n = walked.len();
        let mut j = job.lock().unwrap();
        // It had nothing under it (it was never listed): its items go right after it.
        j.items.splice(i..=i, walked);
        j.rev += 1;
        j.layout += 1;
        drop(j);
        from = i + n;
        changed = true;
    }
    if changed {
        note(Note::Walked);
    }
    true
}

fn set_live(job: &Mutex<Job>, f: impl FnOnce(&mut crate::job::Live)) {
    f(&mut job.lock().unwrap().live);
}

/// Remove the parts of a job's paused copies (a cancel or a discard).
pub fn discard_parts(job: &Mutex<Job>, disk: &dyn Disk) {
    let parts: Vec<PathBuf> = {
        let mut j = job.lock().unwrap();
        j.items
            .iter_mut()
            .filter_map(|it| {
                it.whole = false;
                it.part = 0;
                it.tmp.take()
            })
            .collect()
    };
    for p in parts {
        let _ = disk.remove_file(&long(&p));
    }
}

/// Where item `i` goes: its folder's place (which a "Keep both" may have renamed), then its
/// name. `parents` from `scan::parents`.
pub fn target_of(j: &Job, parents: &[Option<usize>], i: usize) -> PathBuf {
    let it = &j.items[i];
    if let Some(r) = &it.renamed {
        return r.clone();
    }
    match parents.get(i).copied().flatten() {
        Some(p) => target_of(j, parents, p).join(it.rel.file_name().unwrap_or_default()),
        None => j.dest.join(&it.rel),
    }
}

/// Where item `i` is: the top item's path, then the names down to it.
pub fn src_of(j: &Job, parents: &[Option<usize>], i: usize) -> PathBuf {
    let it = &j.items[i];
    if let Some(s) = &it.src {
        return s.clone();
    }
    match parents.get(i).copied().flatten() {
        Some(p) => src_of(j, parents, p).join(it.rel.file_name().unwrap_or_default()),
        None => it.rel.clone(),
    }
}

/// Where a place goes.
enum Place {
    New(PathBuf),
    Over(PathBuf),
}

impl Run<'_> {
    fn braked(&self) -> Option<End> {
        if self.ctl.cancelled() {
            discard_parts(self.job, self.disk);
            return Some(End::Cancelled);
        }
        if self.ctl.paused() {
            return Some(End::Paused);
        }
        None
    }

    fn stopped(&self, s: Stop) -> End {
        match s {
            Stop::Paused => End::Paused,
            Stop::Cancelled => {
                discard_parts(self.job, self.disk);
                End::Cancelled
            }
            Stop::Halt(why) => End::Stopped(why),
        }
    }

    fn set(&self, i: usize, state: ItemState) {
        let mut j = self.job.lock().unwrap();
        j.items[i].state = state;
        j.rev += 1;
    }

    fn update(&self, i: usize, f: impl FnOnce(&mut crate::job::Item)) {
        let mut j = self.job.lock().unwrap();
        f(&mut j.items[i]);
        j.rev += 1;
    }

    /// Where item `i` goes: its folder's place (which may have been renamed) and its name.
    fn target_of(&self, i: usize) -> PathBuf {
        let j = self.job.lock().unwrap();
        self.target_in(&j, i)
    }

    fn target_in(&self, j: &Job, i: usize) -> PathBuf {
        target_of(j, &self.parents, i)
    }

    /// (Re)build `named`: when the run starts, and after a folder's "Keep both" name moved what
    /// is in it.
    fn index_named(&mut self) {
        let mut named: BTreeMap<paths::Key, Vec<usize>> = BTreeMap::new();
        let j = self.job.lock().unwrap();
        for (k, it) in j.items.iter().enumerate() {
            if it.renamed.is_some() || conflict::counted_name(it.rel.file_name().unwrap_or(it.rel.as_os_str())) {
                named.entry(paths::Key(paths::path_key(&self.target_in(&j, k)))).or_default().push(k);
            }
        }
        drop(j);
        self.named = named;
    }

    /// Item `i` is given the "Keep both" name `place`.
    fn rename_to(&mut self, i: usize, place: &Path) {
        self.update(i, |x| x.renamed = Some(place.to_path_buf()));
        self.named.entry(paths::Key(paths::path_key(place))).or_default().push(i);
    }

    /// Another item of this job goes to `place` (its twins by name, `scan::twins`, or an item
    /// whose place is or could be a "Keep both" name, `named`), and `what` holds for it.
    fn other_at(&self, j: &Job, i: usize, place: &Path, what: impl Fn(&crate::job::Item) -> bool) -> bool {
        let group = self.twins.get(i).copied().flatten().and_then(|g| self.groups.get(&g));
        let named = self.named.get(&paths::Key(paths::path_key(place)));
        group.into_iter().chain(named).flatten().any(|&k| k != i && what(&j.items[k]) && paths::same_path(&self.target_in(j, k), place))
    }

    /// Another file of this job has its copy at `target` (X15WFS-1, X15R2A-1): done as a copy
    /// or a move, or a move whose copy is placed and whose original is left.
    fn job_copy_there(&self, i: usize, target: &Path) -> bool {
        let j = self.job.lock().unwrap();
        self.other_at(&j, i, target, |it| {
            !it.dir && (it.placed || matches!(it.state, ItemState::Done(How::Copied | How::Hashed | How::Renamed | How::Moved | How::MovedHashed | How::MovedEarlier)))
        })
    }

    /// One item, if it still needs doing. A file that settles some other way than by its part
    /// (its original gone, skipped after all, its folder failed) takes its part with it.
    fn item(&mut self, i: usize) -> Result<(), Stop> {
        let r = self.item_inner(i);
        let stale = {
            let mut j = self.job.lock().unwrap();
            let it = &mut j.items[i];
            if it.state.settled() && it.tmp.is_some() {
                it.whole = false;
                it.part = 0;
                it.tmp.take()
            } else {
                None
            }
        };
        if let Some(p) = stale {
            let _ = self.disk.remove_file(&long(&p));
        }
        r
    }

    fn item_inner(&mut self, i: usize) -> Result<(), Stop> {
        let (it, policy, parent_state) = {
            let j = self.job.lock().unwrap();
            let mut it = j.items[i].clone();
            it.src = Some(src_of(&j, &self.parents, i));
            let ps = self.parents[i].map(|p| j.items[p].state.clone());
            (it, j.policy, ps)
        };
        let todo = match &it.state {
            ItemState::Pending => true,
            ItemState::Conflict => it.answer.is_some() || policy != conflict::Policy::Ask,
            _ => false,
        };
        if !todo {
            return Ok(());
        }
        match parent_state {
            Some(ItemState::Failed { .. }) => {
                self.set(i, ItemState::Failed { stage: Stage::Folder, reason: "Its folder could not be copied".into() });
                return Ok(());
            }
            Some(ItemState::Skipped(why)) => {
                self.set(i, ItemState::Skipped(why));
                return Ok(());
            }
            _ => {}
        }
        {
            let shown = paths::clean(&it.rel.to_string_lossy());
            let mut j = self.job.lock().unwrap();
            j.live.current = shown;
            j.live.item = Some(i);
            j.live.cur_done = 0;
            j.live.phase = Phase::Copying;
        }
        let target = self.target_of(i);
        if it.dir { self.folder(i, &it, target, policy) } else { self.file(i, &it, target, policy) }
    }

    fn source_key(src: &Path) -> String {
        paths::drive_key(src)
    }

    /// A move may try a rename (same drive, or not known yet).
    fn try_rename(&self, src: &Path) -> bool {
        self.mode == Mode::Move && self.same_volume.get(&Self::source_key(src)) != Some(&false)
    }

    /// After a failure on either side: is it the whole job's problem?
    fn halt_for(&self, e: &std::io::Error, src: &Path) -> Option<String> {
        if copy::is_disk_full(e) {
            return Some(format!("{} is full. Free some space there, then resume.", paths::shown(&paths::root_of(&self.dest))));
        }
        if self.disk.stat(&long(&self.dest)).is_err() {
            return Some(format!("{} is not there any more. Connect it again, then resume.", paths::shown(&self.dest)));
        }
        if copy::is_drive_gone(e) || (copy::is_gone(e) && self.disk.stat(&long(&paths::root_of(src))).is_err()) {
            return Some(format!("{} went away. Connect it again, then resume.", paths::shown(&paths::root_of(src))));
        }
        None
    }

    fn folder(&mut self, i: usize, it: &crate::job::Item, target: PathBuf, _policy: conflict::Policy) -> Result<(), Stop> {
        let tl = long(&target);
        let there = self.disk.stat(&tl);
        // The folder onto itself: a copy makes a "(2)" beside it; a move has nothing to do.
        if there.is_ok() && (paths::same_path(it.path(), &target) || self.disk.same_file(&long(it.path()), &tl)) {
            if self.mode == Mode::Move {
                let range = {
                    let j = self.job.lock().unwrap();
                    crate::scan::subtree(&j.items, i)
                };
                let mut j = self.job.lock().unwrap();
                j.items[i].state = ItemState::Skipped(SAME_FILE.into());
                for k in range {
                    if !j.items[k].state.settled() {
                        j.items[k].state = ItemState::Skipped(SAME_FILE.into());
                    }
                }
                j.rev += 1;
                return Ok(());
            }
            let (renamed, _) = self.keep_both(i, &target);
            self.update(i, |x| x.renamed = Some(renamed.clone()));
            // What is in it goes there too.
            self.index_named();
            return self.make_folder(i, it, &renamed);
        }
        match there {
            Ok(st) if st.dir && !st.link => {
                self.set(i, ItemState::Done(How::Merged));
                Ok(())
            }
            Ok(_) => {
                self.set(i, ItemState::Failed { stage: Stage::Place, reason: FILE_IN_WAY.into() });
                Ok(())
            }
            Err(e) if copy::is_gone(&e) => {
                if self.try_rename(it.path()) {
                    match self.disk.rename(&long(it.path()), &tl) {
                        Ok(()) => {
                            self.same_volume.insert(Self::source_key(it.path()), true);
                            let range = {
                                let j = self.job.lock().unwrap();
                                crate::scan::subtree(&j.items, i)
                            };
                            let mut j = self.job.lock().unwrap();
                            j.items[i].state = ItemState::Done(How::Renamed);
                            for k in range {
                                // Everything in it went along (a skipped link too: it moved).
                                if !j.items[k].state.settled() || matches!(j.items[k].state, ItemState::Skipped(_)) {
                                    j.items[k].state = ItemState::Done(How::Renamed);
                                }
                            }
                            j.rev += 1;
                            return Ok(());
                        }
                        Err(e) if e.raw_os_error() == Some(copy::ERROR_NOT_SAME_DEVICE) => {
                            self.same_volume.insert(Self::source_key(it.path()), false);
                        }
                        Err(e) => {
                            if let Some(why) = self.halt_for(&e, it.path()) {
                                return Err(Stop::Halt(why));
                            }
                            // The folder cannot move whole (a file in it is open, say): it is
                            // made there and its files move one by one; what cannot move stays,
                            // with its folder.
                        }
                    }
                }
                self.make_folder(i, it, &target)
            }
            Err(e) => {
                if let Some(why) = self.halt_for(&e, it.path()) {
                    return Err(Stop::Halt(why));
                }
                self.set(i, ItemState::Failed { stage: Stage::Place, reason: describe(&e) });
                Ok(())
            }
        }
    }

    fn make_folder(&self, i: usize, it: &crate::job::Item, target: &Path) -> Result<(), Stop> {
        match self.disk.create_dir(&long(target)) {
            Ok(()) => {
                self.disk.copy_dir_attributes(&long(it.path()), &long(target));
                self.set(i, ItemState::Done(How::Made));
                Ok(())
            }
            Err(e) if copy::exists_error(&e) => {
                self.set(i, ItemState::Done(How::Merged));
                Ok(())
            }
            Err(e) => {
                if let Some(why) = self.halt_for(&e, it.path()) {
                    return Err(Stop::Halt(why));
                }
                self.set(i, ItemState::Failed { stage: Stage::Place, reason: describe(&e) });
                Ok(())
            }
        }
    }

    /// Item `i`'s own temporary name in `dir` (the first one `tmp_name` tries).
    fn part_name(&self, dir: &Path, i: usize) -> PathBuf {
        dir.join(format!(".vibecopy-{}-{}.part", self.id, i))
    }

    /// A free temporary name in `dir` for item `i`.
    fn tmp_name(&self, dir: &Path, i: usize) -> PathBuf {
        let mut n = 0;
        loop {
            let p = if n == 0 { self.part_name(dir, i) } else { dir.join(format!(".vibecopy-{}-{}-{}.part", self.id, i, n)) };
            if self.disk.stat(&long(&p)).is_err() {
                return p;
            }
            n += 1;
        }
    }

    fn file(&mut self, i: usize, it: &crate::job::Item, target: PathBuf, policy: conflict::Policy) -> Result<(), Stop> {
        // A move whose copy is in place already (its original could not be removed before):
        // check that copy again, then remove the original.
        if it.placed && self.mode == Mode::Move {
            return self.finish_move(i, it, &target, policy);
        }
        self.copy_file(i, it, target, policy, None)
    }

    /// Copy item `i` into place. `recopy`: a retried move copies again over its own stale copy,
    /// made from an original of this size and time (RVC2-1); it replaces the copy only while it
    /// is still as made, else both stay. The item keeps `placed` and `made` until the new copy
    /// is in place, so a copy again that does not finish is decided on the copy again next time.
    fn copy_file(&mut self, i: usize, it: &crate::job::Item, target: PathBuf, policy: conflict::Policy, recopy: Option<Meta>) -> Result<(), Stop> {
        let sl = long(it.path());
        let src_now = match self.disk.stat(&sl) {
            Ok(s) => s,
            Err(e) => {
                if let Some(why) = self.halt_for(&e, it.path()) {
                    return Err(Stop::Halt(why));
                }
                // Read back from a queue older than the disk: the move may have finished after
                // the last save (copy in place, original gone). Nothing is deleted here.
                if self.restored && self.mode == Mode::Move && copy::is_gone(&e) {
                    let mut places = vec![it.renamed.clone().unwrap_or_else(|| target.clone())];
                    // A "Keep both" name: by the rule or the answer, or because another file of
                    // the job goes to the same place (X15R2A-3: it is kept beside that one
                    // whatever the rule).
                    let shared = self.twins[i].is_some() || self.other_at(&self.job.lock().unwrap(), i, &target, |_| true);
                    if policy == conflict::Policy::KeepBoth || it.answer == Some(conflict::Choice::KeepBoth) || shared {
                        places.extend(self.keep_both(i, &target).1);
                    }
                    if places.iter().any(|p| self.in_place(it, p)) {
                        self.set(i, ItemState::Done(if self.verify { How::MovedHashed } else { How::Moved }));
                        return Ok(());
                    }
                }
                let reason = if copy::is_gone(&e) { "Not found (moved or deleted since it was picked)".to_string() } else { describe(&e) };
                self.set(i, ItemState::Failed { stage: Stage::Read, reason });
                return Ok(());
            }
        };
        // A whole, checked part that is gone while its folder answers and the target holds the
        // original's bytes: the app stopped right after renaming it into place. Compared byte
        // for byte (whatever Verify says: a file that was there before can match by size and
        // time); then it is done. Any other missing part is simply copied again.
        let part_gone = |t: &Path| {
            matches!(self.disk.stat(&long(t)), Err(e) if copy::is_gone(&e)) && t.parent().is_some_and(|d| self.disk.stat(&long(d)).is_ok())
        };
        if let Some(t) = &it.tmp
            && part_gone(t)
        {
            let placed = it.renamed.clone().unwrap_or_else(|| target.clone());
            let pl = long(&placed);
            if it.whole && self.disk.stat(&pl).is_ok_and(|s| s.size == src_now.size && s.mtime.abs_diff(src_now.mtime) <= self.slack) && self.check_with(&sl, &pl, &src_now, true)?.is_ok() {
                self.update(i, |x| {
                    x.tmp = None;
                    x.part = 0;
                    x.whole = false;
                });
                return self.own_copy_done(i, &sl, &placed, &src_now);
            }
            self.update(i, |x| {
                x.tmp = None;
                x.part = 0;
                x.whole = false;
            });
        }
        let it = &{
            let mut x = it.clone();
            x.tmp = self.job.lock().unwrap().items[i].tmp.clone();
            x
        };
        // What is at the target decides where the copy goes. An overwrite remembers the file it
        // was decided on (FS11: it is looked at again right before the replace).
        let mut decided_on: Option<Meta> = None;
        let place = match self.disk.stat(&long(&target)) {
            Err(e) if copy::is_gone(&e) => Place::New(target.clone()),
            Err(e) => {
                if let Some(why) = self.halt_for(&e, it.path()) {
                    return Err(Stop::Halt(why));
                }
                self.set(i, ItemState::Failed { stage: Stage::Place, reason: describe(&e) });
                return Ok(());
            }
            Ok(st) if st.dir => {
                self.set(i, ItemState::Failed { stage: Stage::Place, reason: FOLDER_IN_WAY.into() });
                return Ok(());
            }
            // A copy again over the job's own copy: changed since it was made, it is the user's.
            Ok(st) if recopy.is_some_and(|m| !self.as_made(&st, m)) => {
                self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: COPY_CHANGED.into() });
                return Ok(());
            }
            Ok(st) if recopy.is_some() => {
                decided_on = Some(Meta { size: st.size, mtime: st.mtime });
                Place::Over(target.clone())
            }
            Ok(st) => {
                let selfsame = paths::same_path(it.path(), &target) || self.disk.same_file(&sl, &long(&target));
                // Read back from a queue older than the disk: this may be the job's own copy,
                // put in place after the last save. Only if it holds the original's bytes
                // (compared, whatever Verify says: never size and time alone).
                if self.restored && !selfsame && self.own_copy(&sl, &target, &src_now)? {
                    return self.own_copy_done(i, &sl, &target, &src_now);
                }
                let action = if selfsame {
                    if self.mode == Mode::Move {
                        Action::Skip(SAME_FILE)
                    } else {
                        Action::KeepBoth
                    }
                } else if self.job_copy_there(i, &target) {
                    // X15WFS-1, X15R2A-1: another file of this job put its copy here (a case
                    // twin, the same name from another folder, or one kept under this name). It
                    // is never replaced, or skipped or asked about as if it were a file that was
                    // there: this one gets a "Keep both" name.
                    Action::KeepBoth
                } else {
                    // APA2: an Overwrite answer is about the file the question showed. One saved
                    // there before the answer came (hours later, or across a restart: the queue
                    // keeps the answer) is asked about again, never replaced unseen.
                    let now = Meta { size: st.size, mtime: st.mtime };
                    let answer = match (it.answer, it.there) {
                        (Some(conflict::Choice::Overwrite), Some(shown)) if shown != now => None,
                        (a, _) => a,
                    };
                    conflict::decide(policy, answer, Meta { size: src_now.size, mtime: src_now.mtime }, now)
                };
                match action {
                    Action::Skip(why) => {
                        self.set(i, ItemState::Skipped(why.into()));
                        return Ok(());
                    }
                    Action::Ask => {
                        let fresh = it.state != ItemState::Conflict;
                        self.update(i, |x| {
                            x.state = ItemState::Conflict;
                            // A stale answer (APA2) is gone with the file it was about.
                            x.answer = None;
                            x.there = Some(Meta { size: st.size, mtime: st.mtime });
                            x.size = src_now.size;
                            x.mtime = src_now.mtime;
                        });
                        if fresh && !self.told {
                            self.told = true;
                            (self.note)(Note::Conflict);
                        }
                        return Ok(());
                    }
                    Action::Overwrite => {
                        decided_on = Some(Meta { size: st.size, mtime: st.mtime });
                        Place::Over(target.clone())
                    }
                    Action::KeepBoth => {
                        let (p, taken) = self.keep_both(i, &target);
                        // The same for the "(2)" names: the first that holds the original's
                        // bytes is the copy this job made before the restart.
                        if self.restored {
                            for c in taken {
                                if !self.disk.same_file(&sl, &long(&c)) && self.own_copy(&sl, &c, &src_now)? {
                                    self.rename_to(i, &c);
                                    return self.own_copy_done(i, &sl, &c, &src_now);
                                }
                            }
                        }
                        self.rename_to(i, &p);
                        Place::New(p)
                    }
                }
            }
        };
        let final_path = match &place {
            Place::New(p) | Place::Over(p) => p.clone(),
        };

        // A move on the same drive: one rename, nothing copied.
        if self.try_rename(it.path()) {
            let r = match &place {
                Place::New(p) => self.disk.rename(&sl, &long(p)),
                Place::Over(p) => self.disk.replace(&sl, &long(p)),
            };
            match r {
                Ok(()) => {
                    self.same_volume.insert(Self::source_key(it.path()), true);
                    self.set(i, ItemState::Done(How::Renamed));
                    return Ok(());
                }
                Err(e) if e.raw_os_error() == Some(copy::ERROR_NOT_SAME_DEVICE) => {
                    self.same_volume.insert(Self::source_key(it.path()), false);
                }
                Err(e) => {
                    if let Some(why) = self.halt_for(&e, it.path()) {
                        return Err(Stop::Halt(why));
                    }
                    self.set(i, ItemState::Failed { stage: Stage::Place, reason: describe(&e) });
                    return Ok(());
                }
            }
        }

        // Copy to the temporary name (or go on with the part a pause left).
        let dir = final_path.parent().unwrap_or(Path::new("")).to_path_buf();
        let (tmp, mut resume, whole) = match &it.tmp {
            Some(t) => (t.clone(), !it.whole, it.whole),
            // Read back from a queue older than the disk: a part under this item's own name was
            // made after the last save. Resumed (Windows refuses one that does not fit, and it
            // is then copied from the start), never left behind.
            None if self.restored && self.disk.stat(&long(&self.part_name(&dir, i))).is_ok_and(|s| !s.dir) => (self.part_name(&dir, i), true, false),
            None => (self.tmp_name(&dir, i), false, false),
        };
        let tl = long(&tmp);
        let mut have_whole = whole && self.disk.stat(&tl).map(|t| t.size == src_now.size && t.mtime.abs_diff(src_now.mtime) <= self.slack).unwrap_or(false);
        if whole && !have_whole {
            let _ = self.disk.remove_file(&tl);
            resume = false;
        }
        if !have_whole {
            self.update(i, |x| {
                x.tmp = Some(tmp.clone());
                x.whole = false;
            });
            let mut fresh_tries = 0;
            loop {
                // A resumed part's bytes were copied before: the speed counts only new ones.
                let start = if resume { self.disk.stat(&tl).map(|t| t.size).unwrap_or(0) } else { 0 };
                self.job.lock().unwrap().live.cur_done = start;
                let last = std::sync::atomic::AtomicU64::new(start);
                let step = |n: u64| {
                    if n != u64::MAX {
                        let before = last.swap(n, std::sync::atomic::Ordering::Relaxed);
                        let mut j = self.job.lock().unwrap();
                        j.live.cur_done = n;
                        j.live.copied += n.saturating_sub(before);
                    }
                    self.ctl.step()
                };
                let r = self.disk.copy(&sl, &tl, Opts { unbuffered: src_now.size >= BIG, resume, flush: self.verify }, &step);
                match r {
                    Ok(Copied::Done) => {
                        self.update(i, |x| x.part = 0);
                        break;
                    }
                    Ok(Copied::Paused) => {
                        let done = self.job.lock().unwrap().live.cur_done;
                        self.update(i, |x| x.part = done);
                        return Err(Stop::Paused);
                    }
                    Ok(Copied::Cancelled) => {
                        self.update(i, |x| {
                            x.tmp = None;
                            x.part = 0;
                        });
                        return Err(Stop::Cancelled);
                    }
                    Err(e) if resume && copy::exists_error(&e) && fresh_tries == 0 => {
                        // Not a paused part after all (a crash left it): start over.
                        let _ = self.disk.remove_file(&tl);
                        resume = false;
                        fresh_tries += 1;
                    }
                    Err(e) => {
                        // A fresh copy refused because the name is there made nothing: the part
                        // is another copy's, made between `tmp_name`'s look and this create (two
                        // guests count their ids alike, RI5-1), and is left as it is.
                        let theirs = !resume && copy::exists_error(&e);
                        if !theirs {
                            let _ = self.disk.remove_file(&tl);
                        }
                        self.update(i, |x| {
                            x.tmp = None;
                            x.part = 0;
                        });
                        if let Some(why) = self.halt_for(&e, it.path()) {
                            return Err(Stop::Halt(why));
                        }
                        let reason = if theirs {
                            PART_BUSY.to_string()
                        } else if copy::is_gone(&e) && self.disk.stat(&sl).is_err() {
                            "Not found (moved or deleted since it was picked)".to_string()
                        } else {
                            describe(&e)
                        };
                        self.set(i, ItemState::Failed { stage: Stage::Copy, reason });
                        return Ok(());
                    }
                }
            }
            self.update(i, |x| x.whole = true);
            have_whole = true;
        }
        debug_assert!(have_whole);

        // Check it: size and time always, the bytes with Verify.
        set_live(self.job, |l| l.phase = Phase::Verifying);
        let checked = self.check(&sl, &tl, &src_now)?;
        if let Err(reason) = checked {
            let _ = self.disk.remove_file(&tl);
            self.update(i, |x| {
                x.tmp = None;
                x.whole = false;
                x.state = ItemState::Failed { stage: Stage::Verify, reason: reason.into() };
            });
            return Ok(());
        }
        set_live(self.job, |l| l.phase = Phase::Copying);

        // FS11: the overwrite was decided on the file that was there then (by "Overwrite if
        // older", or an answer about that file). One saved there since is decided on again,
        // never replaced unasked; the whole, checked copy stays for the new answer.
        if let (Place::Over(p), Some(was)) = (&place, decided_on) {
            match self.disk.stat(&long(p)) {
                Ok(now) if !now.dir && now.size == was.size && now.mtime == was.mtime => {}
                Err(e) if copy::is_gone(&e) => {}
                // The job's own stale copy, saved since (RVC2-1): it is the user's now.
                Ok(now) if !now.dir && recopy.is_some() => {
                    let _ = self.disk.remove_file(&tl);
                    self.update(i, |x| {
                        x.tmp = None;
                        x.whole = false;
                        x.state = ItemState::Failed { stage: Stage::RemoveSource, reason: COPY_CHANGED.into() };
                    });
                    return Ok(());
                }
                Ok(now) if !now.dir => {
                    let answer = if policy == conflict::Policy::Ask { None } else { it.answer };
                    match conflict::decide(policy, answer, Meta { size: src_now.size, mtime: src_now.mtime }, Meta { size: now.size, mtime: now.mtime }) {
                        Action::Overwrite => {}
                        Action::Skip(why) => {
                            let _ = self.disk.remove_file(&tl);
                            self.update(i, |x| {
                                x.tmp = None;
                                x.whole = false;
                                x.state = ItemState::Skipped(why.into());
                            });
                            return Ok(());
                        }
                        Action::Ask | Action::KeepBoth => {
                            let fresh = it.state != ItemState::Conflict;
                            self.update(i, |x| {
                                x.state = ItemState::Conflict;
                                x.answer = None;
                                x.there = Some(Meta { size: now.size, mtime: now.mtime });
                                x.size = src_now.size;
                                x.mtime = src_now.mtime;
                            });
                            if fresh && !self.told {
                                self.told = true;
                                (self.note)(Note::Conflict);
                            }
                            return Ok(());
                        }
                    }
                }
                other => {
                    let _ = self.disk.remove_file(&tl);
                    self.update(i, |x| {
                        x.tmp = None;
                        x.whole = false;
                    });
                    let reason = match other {
                        Err(e) => describe(&e),
                        Ok(_) => FOLDER_IN_WAY.to_string(),
                    };
                    self.set(i, ItemState::Failed { stage: Stage::Place, reason });
                    return Ok(());
                }
            }
        }

        // Into place.
        let placed = match &place {
            Place::New(p) => self.disk.rename(&tl, &long(p)),
            Place::Over(p) => self.disk.replace(&tl, &long(p)),
        };
        if let Err(e) = placed {
            let _ = self.disk.remove_file(&tl);
            self.update(i, |x| {
                x.tmp = None;
                x.whole = false;
            });
            if let Some(why) = self.halt_for(&e, it.path()) {
                return Err(Stop::Halt(why));
            }
            let reason = if copy::exists_error(&e) { "Something of that name appeared meanwhile".to_string() } else { describe(&e) };
            self.set(i, ItemState::Failed { stage: Stage::Place, reason });
            return Ok(());
        }
        self.update(i, |x| {
            x.tmp = None;
            x.whole = false;
        });
        if self.mode == Mode::Copy {
            self.set(i, ItemState::Done(if self.verify { How::Hashed } else { How::Copied }));
            return Ok(());
        }
        // A move across drives: the original goes, if the evidence allows.
        let ev = Evidence {
            copied: true,
            placed: true,
            quick: true,
            verify_on: self.verify,
            hashed: if self.verify { Some(true) } else { None },
            source_unchanged: self.disk.stat(&sl).map(|s| verify::quick(&s, &src_now)).unwrap_or(false),
        };
        self.update(i, |x| {
            x.placed = true;
            x.made = Some(Meta { size: src_now.size, mtime: src_now.mtime });
        });
        self.remove_original(i, &sl, &long(&final_path), &ev)
    }

    /// Size and time, then (Verify) the bytes. Ok(Err(reason)): it does not match.
    fn check(&self, sl: &Path, tl: &Path, src_then: &Stat) -> Result<Result<(), &'static str>, Stop> {
        self.check_with(sl, tl, src_then, self.verify)
    }

    /// The same, the bytes compared when `hash`.
    fn check_with(&self, sl: &Path, tl: &Path, src_then: &Stat, hash: bool) -> Result<Result<(), &'static str>, Stop> {
        let (s, t) = match (self.disk.stat(sl), self.disk.stat(tl)) {
            (Ok(s), Ok(t)) => (s, t),
            (Err(_), _) => return Ok(Err(ORIGINAL_CHANGED)),
            (_, Err(_)) => return Ok(Err(NOT_WHOLE)),
        };
        if !verify::quick(&s, src_then) {
            return Ok(Err(ORIGINAL_CHANGED));
        }
        if s.size != t.size {
            return Ok(Err(NOT_WHOLE));
        }
        if s.mtime.abs_diff(t.mtime) > self.slack {
            return Ok(Err(TIME_DIFFERS));
        }
        if !hash {
            return Ok(Ok(()));
        }
        // Both sides at once (two reads in flight, two cores hashing): about twice as fast as
        // one after the other, measured.
        let (a, b) = std::thread::scope(|s| {
            // The copy is read from the drive, past Windows' cache (FS7).
            let other = s.spawn(|| self.disk.hash(tl, true, self.ctl, &|_| {}));
            let a = self.disk.hash(sl, false, self.ctl, &|_| {});
            (a, other.join().unwrap_or_else(|_| Err(std::io::Error::other("the check stopped"))))
        });
        match (a, b) {
            (Ok(Some(x)), Ok(Some(y))) => Ok(if x == y { Ok(()) } else { Err(MISMATCH) }),
            (Err(_), _) => Ok(Err(ORIGINAL_CHANGED)),
            (_, Err(_)) => Ok(Err(NOT_WHOLE)),
            // The brakes, mid-hash: the whole copy stays for the resume.
            _ => Err(if self.ctl.cancelled() { Stop::Cancelled } else { Stop::Paused }),
        }
    }

    /// Item `i`'s free "Keep both" name beside `target`, and the ones already there on the way
    /// to it (`a (2).txt`, `a (3).txt`, …, in order). A name another item of the job goes to is
    /// not free either, there yet or not (X15R2A-1): no later file of the job lands on it.
    fn keep_both(&self, i: usize, target: &Path) -> (PathBuf, Vec<PathBuf>) {
        let parent = target.parent().unwrap_or(Path::new("")).to_path_buf();
        let taken = std::cell::RefCell::new(Vec::new());
        let name: OsString = conflict::keep_both_name(target.file_name().unwrap_or_default(), &|n| {
            let p = parent.join(n);
            // The job is never locked while the disk is asked (a slow share would hold the window).
            let there = self.disk.stat(&long(&p)).is_ok();
            let reserved = self.other_at(&self.job.lock().unwrap(), i, &p, |_| true);
            if there {
                taken.borrow_mut().push(p);
            }
            there || reserved
        });
        (parent.join(name), taken.into_inner())
    }

    /// A moved item's copy is at `p`: its size and time are those of the original it was made
    /// from (`made`), or without that record the item's from the count (its original is gone,
    /// there is nothing left to compare it with; RVC2-2).
    fn in_place(&self, it: &crate::job::Item, p: &Path) -> bool {
        let m = it.made.unwrap_or(Meta { size: it.size, mtime: it.mtime });
        self.disk.stat(&long(p)).is_ok_and(|t| !t.dir && self.as_made(&t, m))
    }

    /// A placed move whose original `e` says is gone: how it is done, if it is (`finish_move`).
    /// Its copy still as made was checked when it was placed; any copy there, read back from a
    /// queue older than the disk, was not (RVC3-2, RVCD4-7).
    fn moved_before(&self, e: &std::io::Error, it: &crate::job::Item, target: &Path, tl: &Path) -> Option<How> {
        if !copy::is_gone(e) || self.halt_for(e, it.path()).is_some() {
            None
        } else if self.in_place(it, target) {
            Some(if self.verify { How::MovedHashed } else { How::Moved })
        } else if self.restored && self.disk.stat(tl).is_ok_and(|t| !t.dir) {
            Some(How::MovedEarlier)
        } else {
            None
        }
    }

    /// A copy `t` is still as it was made from an original of `m`'s size and time: the same
    /// size, and a time within the slack (a same-size save is caught by its time; RVC2-3).
    fn as_made(&self, t: &Stat, m: Meta) -> bool {
        t.size == m.size && t.mtime.abs_diff(m.mtime) <= self.slack
    }

    /// `p` holds the original's bytes (size and time first, then both hashed).
    fn own_copy(&self, sl: &Path, p: &Path, src_now: &Stat) -> Result<bool, Stop> {
        Ok(self.check_with(sl, &long(p), src_now, true)?.is_ok())
    }

    /// The copy at `placed` holds the original's bytes: the item is done; a move's original then
    /// goes under the usual rule.
    fn own_copy_done(&self, i: usize, sl: &Path, placed: &Path, src_now: &Stat) -> Result<(), Stop> {
        if self.mode == Mode::Copy {
            self.set(i, ItemState::Done(if self.verify { How::Hashed } else { How::Copied }));
            return Ok(());
        }
        self.update(i, |x| {
            x.placed = true;
            x.made = Some(Meta { size: src_now.size, mtime: src_now.mtime });
        });
        let ev = Evidence { copied: true, placed: true, quick: true, verify_on: true, hashed: Some(true), source_unchanged: self.disk.stat(sl).map(|s| verify::quick(&s, src_now)).unwrap_or(false) };
        self.remove_original(i, sl, &long(placed), &ev)
    }

    fn remove_original(&self, i: usize, sl: &Path, placed: &Path, ev: &Evidence) -> Result<(), Stop> {
        if !move_::may_delete_source(ev) {
            self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: move_::kept_because(ev).into() });
            return Ok(());
        }
        // The copy in place is the original itself under another name (a share seen twice):
        // deleting "the original" would delete the only one.
        if self.disk.same_file(sl, placed) {
            self.update(i, |x| {
                x.placed = false;
                x.made = None;
                x.state = ItemState::Skipped(SAME_FILE.into());
            });
            return Ok(());
        }
        set_live(self.job, |l| l.phase = Phase::Removing);
        match self.disk.remove_file(sl) {
            Ok(()) => {
                self.update(i, |x| {
                    x.placed = false;
                    x.made = None;
                    x.state = ItemState::Done(if ev.verify_on { How::MovedHashed } else { How::Moved });
                });
                Ok(())
            }
            Err(e) => {
                self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: format!("Copied, but the original could not be removed: {}", describe(&e)) });
                Ok(())
            }
        }
    }

    /// A retried move whose copy is already in place: the copy is checked against the original
    /// again (as if just made), then the original goes. A copy that no longer matches is copied
    /// again over only while it is still as it was made (or gone); one changed since is kept,
    /// also when that happens while it is copied again (RVC2-1).
    fn finish_move(&mut self, i: usize, it: &crate::job::Item, target: &Path, policy: conflict::Policy) -> Result<(), Stop> {
        let sl = long(it.path());
        let tl = long(target);
        let src_now = match self.disk.stat(&sl) {
            Ok(s) => s,
            // The original went after its checked copy was placed (the app stopped between
            // the two steps): the move is done. Read back from a queue older than the disk, the
            // copy may be a copy again whose record of what it was made from was not saved yet
            // (the app stopped between it and the removal): one there is the move done too
            // (RVC3-2; nothing is deleted here). Nothing of that copy was checked then, and its
            // row says so (RVCD4-7).
            Err(e) => {
                if let Some(how) = self.moved_before(&e, it, target, &tl) {
                    self.update(i, |x| {
                        x.placed = false;
                        x.made = None;
                        x.state = ItemState::Done(how);
                    });
                    return Ok(());
                }
                // Its drive away: the whole job's problem, and the item stays placed, so the next
                // try decides on the copy again (RVC3-1: cleared here, a later Retry went
                // through the job's policy and could copy over an edited copy).
                if let Some(why) = self.halt_for(&e, it.path()) {
                    return Err(Stop::Halt(why));
                }
                // Only an original truly gone ends what the item knows of its copy.
                if copy::is_gone(&e) {
                    self.update(i, |x| {
                        x.placed = false;
                        x.made = None;
                    });
                }
                self.set(i, ItemState::Failed { stage: Stage::Read, reason: describe(&e) });
                return Ok(());
            }
        };
        match self.check(&sl, &tl, &src_now)? {
            Ok(()) => {
                let ev = Evidence {
                    copied: true,
                    placed: true,
                    quick: true,
                    verify_on: self.verify,
                    hashed: if self.verify { Some(true) } else { None },
                    source_unchanged: self.disk.stat(&sl).map(|s| verify::quick(&s, &src_now)).unwrap_or(false),
                };
                self.remove_original(i, &sl, &tl, &ev)
            }
            Err(_) => {
                // Decided on the copy, never on the original (RVA12, RVA13): a copy that is no
                // longer as it was made is the user's work now, not the job's to replace, so both
                // stay, whatever the original did (FEA1). Without a record of how it was made (a
                // queue saved before it was kept) both stay too, rather than guess.
                match self.disk.stat(&tl) {
                    Ok(t) if !t.dir && !it.made.is_some_and(|m| self.as_made(&t, m)) => {
                        self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: COPY_CHANGED.into() });
                        return Ok(());
                    }
                    Err(e) if !copy::is_gone(&e) => {
                        self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: format!("The copy could not be looked at ({}), so both files were kept: the original was not removed", describe(&e)) });
                        return Ok(());
                    }
                    _ => {}
                }
                // The copy is still as it was made (the original changed since), or it is gone:
                // copy it again, over the stale copy (it is this job's own) while it stays as
                // made. Never a standing Overwrite answer: a copy again that does not finish
                // leaves the item as it was, and the next try decides on the copy again (RVC2-1).
                self.copy_file(i, it, target.to_path_buf(), policy, it.made)
            }
        }
    }

    /// A finished move across drives: its source folders go, deepest first, if empty (one
    /// that still holds something skipped or failed stays, with it).
    fn remove_moved_folders(&self) {
        let dirs: Vec<(usize, PathBuf)> = {
            let j = self.job.lock().unwrap();
            j.items.iter().enumerate().filter(|(_, it)| it.dir && matches!(it.state, ItemState::Done(How::Made | How::Merged))).map(|(i, _)| (i, src_of(&j, &self.parents, i))).collect()
        };
        for (i, src) in dirs.into_iter().rev().filter(|(_, s)| removable(s)) {
            match self.disk.remove_dir(&long(&src)) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(copy::ERROR_DIR_NOT_EMPTY) || copy::is_gone(&e) => {}
                Err(e) => self.set(i, ItemState::Failed { stage: Stage::RemoveSource, reason: format!("The folder could not be removed: {}", describe(&e)) }),
            }
        }
    }
}

/// A moved folder that may be removed afterwards: not a drive or share root (`D:\`,
/// `\\server\share\`), which Windows refuses before it looks inside ("Access denied"), and
/// which is not the job's to remove anyway.
fn removable(src: &Path) -> bool {
    paths::short(src).parent().is_some()
}
