//! The app's brain, on the UI thread: the queue, the workers that run its jobs, the dialogs,
//! the toasts. The window is a view: it is filled from here and every click lands here.
//!
//! Workers report through one channel and a wake-up (`Env::wake`); `poll` takes in what came.
//! A worker updates its job's items under the job's lock; the window reads them on a beat
//! (250 ms) while anything runs. This file: the queue and the workers. `view` fills the window,
//! `form` the new job dialog, `ask` the conflict dialog, `files` a job's file list.

mod about;
mod ask;
mod files;
mod form;
mod view;

use crate::clipboard::Clip;
use crate::conflict::Policy;
use crate::copy::Control;
use crate::disk::Disk;
use crate::job::{self, ItemState, Job, Loaded, Mode, Saver, Shared, State};
use crate::queue::{self, Do, Slot};
use crate::run::{self, End, Note};
use crate::taskbar::Bar;
use crate::{AppWindow, Prefs, ToastKind, paths, prefs};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub use about::{About, Fetcher};
pub use files::FileModel;

/// What the app asks of Windows (the tests answer with a fake).
pub trait Shell {
    /// A folder picker; `done` (on any thread) hears the choice.
    fn pick_folder(&self, owner: Option<isize>, start: Option<PathBuf>, done: Box<dyn FnOnce(Option<PathBuf>) + Send>);
    /// Explorer at a folder.
    fn reveal(&self, path: &Path) -> Result<(), String>;
    /// The clipboard's files (read when the user pastes, never otherwise); `done` on any thread.
    fn read_clipboard(&self, done: Box<dyn FnOnce(Result<Option<Clip>, String>) + Send>);
    /// The taskbar button's progress.
    fn taskbar(&self, owner: Option<isize>, bar: Bar, done: u64, total: u64);
    /// Leave the event loop (the app exits).
    fn quit(&self);
}

pub struct Env {
    /// %LOCALAPPDATA%\com.tedkj.vibecopy (queue.json).
    pub local: PathBuf,
    pub disk: Arc<dyn Disk>,
    pub shell: Rc<dyn Shell>,
    /// Called by workers (any thread) when they have sent something: get `poll` to run.
    pub wake: Arc<dyn Fn() + Send + Sync>,
    /// Seconds, steadily rising (speeds are measured against it).
    pub clock: Rc<dyn Fn() -> f64>,
    /// About's link and its install check (`about`); the tests hand in fakes.
    pub about: About,
}

pub(crate) enum Msg {
    Ended { id: u64, end: End },
    Note(Note),
    /// The new job dialog's walk of its sources.
    Counted { seq: u64, items: Option<Vec<job::Item>> },
    /// The new job dialog's look at the destination.
    Probed { seq: u64, probe: form::Probe },
    Picked { seq: u64, path: Option<PathBuf> },
    Clip { result: Result<Option<Clip>, String> },
    /// Open destination: whether it is there (Err: it could not be looked at, and why).
    Opened { seq: u64, path: PathBuf, there: Result<bool, String> },
    /// A job's parts were removed.
    Discarded,
    /// These renumbered jobs' parts took their new id (worth saving; they may start now).
    Renamed(Vec<u64>),
}

/// A speed measured over the last few seconds of a running job.
#[derive(Default)]
pub(crate) struct Meter {
    samples: VecDeque<(f64, u64)>,
}

/// How far back a speed looks.
const WINDOW: f64 = 4.0;

impl Meter {
    pub(crate) fn sample(&mut self, t: f64, copied: u64) {
        if self.samples.back().is_some_and(|&(_, c)| c > copied) {
            self.samples.clear();
        }
        self.samples.push_back((t, copied));
        while self.samples.len() > 2 && self.samples[1].0 <= t - WINDOW {
            self.samples.pop_front();
        }
    }
    /// Bytes per second, once there is a second of it.
    pub(crate) fn speed(&self) -> Option<f64> {
        let (&(t0, c0), &(t1, c1)) = (self.samples.front()?, self.samples.back()?);
        (t1 - t0 >= 1.0).then(|| (c1 - c0) as f64 / (t1 - t0))
    }
    pub(crate) fn clear(&mut self) {
        self.samples.clear();
    }
}

pub(crate) struct Entry {
    pub shared: Shared,
    pub id: u64,
    pub drive: String,
    /// Its worker's brakes, while one runs.
    pub ctl: Option<Arc<Control>>,
    pub cancel_asked: bool,
    pub meter: Meter,
    /// Offered from last time (the note above the list).
    pub restored: bool,
    /// Its parts are taking its new id on a worker: it does not start until that is done, or a
    /// resumed copy and the renames would both write its parts (review X15CON-13).
    pub renaming: bool,
}

impl Entry {
    pub(crate) fn state(&self) -> State {
        self.shared.lock().unwrap().state
    }
}

/// A drop's mode as `drop_arrived` took it: its drag's, with the New job dialog that drag was
/// seen over (RRF4-1). `main.rs` only carries it to `dropped`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DropMode {
    mode: Mode,
    /// That dialog's `Form::id`; none when the drag came in over no dialog, or none was seen.
    over: Option<u64>,
}

/// A dialog's question waiting for its answer.
pub(crate) enum Confirm {
    Quit,
}

/// A toast's action.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ToastDo {
    None,
    /// Show this job's files.
    Show(u64),
}

/// What the saving thread writes next: the jobs (shared with their workers), marked dirty by
/// every change. It encodes them itself, so a queue of 100 000 files is never serialized on
/// the UI thread.
#[derive(Default)]
pub(crate) struct Saving {
    next: Mutex<SaveNext>,
    cv: Condvar,
    /// Numbers each encoded queue; the saver never writes an older one over a newer one.
    seq: AtomicU64,
}

#[derive(Default)]
struct SaveNext {
    dirty: bool,
    stop: bool,
    next_id: u64,
    jobs: Vec<Shared>,
}

/// A guest's first job id (RI2-5): far above any the owner's queue reaches, so a guest's part
/// names (`.vibecopy-<id>-<n>.part`) never meet a part the owner resumes. It fits the window's
/// `int`.
pub const GUEST_IDS: u64 = 1_000_000_000;

/// How long a start waits for a queue lock that a program not known to be VibeCopy holds
/// (RI4-1), before it runs as a guest.
const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// A part a guest may remove at its quit (RI4-2): one named after `id`, a guest's own job id
/// (`.vibecopy-<id>-<n>.part`), so never a part of the owner's jobs, whose ids stay below
/// `GUEST_IDS`.
pub(crate) fn guest_part(id: u64, part: &Path) -> bool {
    id >= GUEST_IDS && part.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&format!(".vibecopy-{id}-")) && n.ends_with(".part"))
}

/// The queue file's body: every unfinished job, each copied under its own lock and serialized
/// after it is let go, so the window's refresh and the copy's progress, which take the same
/// lock, never wait for the JSON of a long item list (review X15CON-12).
fn encode(next_id: u64, jobs: &[Shared]) -> Option<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for sh in jobs {
        let j = {
            let g = sh.lock().unwrap();
            if g.state.finished() {
                continue;
            }
            g.clone()
        };
        if let Ok(b) = serde_json::to_vec(&j) {
            parts.push(b);
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut out = format!("{{\"next_id\":{next_id},\"jobs\":[").into_bytes();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(p);
    }
    out.extend_from_slice(b"]}");
    Some(out)
}

pub struct App {
    pub ui: AppWindow,
    pub(crate) env: Env,
    pub(crate) me: Weak<App>,
    pub prefs: prefs::Store<Prefs>,
    pub(crate) entries: RefCell<Vec<Entry>>,
    next_id: Cell<u64>,
    pub saver: Saver,
    saving: Arc<Saving>,
    save_thread: RefCell<Option<std::thread::JoinHandle<()>>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub(crate) view: view::Models,
    /// The job whose files are open (page 1).
    pub(crate) detail: Cell<Option<u64>>,
    pub(crate) form: RefCell<Option<form::Form>>,
    /// The mode of the drag over the window and the dialog it came in over, set when it came in
    /// (`drag_entered`; RRF2-1, RRF3-1, RRF4-1).
    drag: Cell<Option<DropMode>>,
    /// The conflict on screen: job, item and the job's `layout` then (an index from before a
    /// retry's walk may name another item now, FS1).
    pub(crate) asking: Cell<Option<(u64, usize, u64)>>,
    pub(crate) confirm: RefCell<Option<Confirm>>,
    pub(crate) toasts: RefCell<Vec<(i32, String, ToastKind, ToastDo)>>,
    toast_seq: Cell<i32>,
    /// Toasts that go by themselves, each with the number of its current timer: a timer that
    /// ends behind a dialog (there toasts are inert: seen, not reachable) does nothing, and
    /// every one of them gets its full time again once the dialog closes, a new timer that
    /// voids the earlier ones.
    toast_timed: RefCell<HashMap<i32, u32>>,
    toast_gen: Cell<u32>,
    pub(crate) note: RefCell<String>,
    note_timer: RefCell<Option<slint::Timer>>,
    /// While anything runs: the beat that refreshes the window.
    beat: slint::Timer,
    last_save: Cell<f64>,
    /// Numbers the window's looks at folders (never reset: a stale answer never matches).
    seq: Cell<u64>,
    /// Workers and looks still out (the tests' settling).
    pub(crate) out: Cell<u32>,
    pub(crate) open_wait: Cell<u64>,
    pub(crate) taskbar_last: Cell<(Bar, u64)>,
    quitting: Cell<bool>,
    /// The toast that says queue.json cannot be read (it goes once it is read).
    unread_toast: Cell<Option<i32>>,
    /// The queue lock, held until the saving ends (RI2-5): this VibeCopy owns queue.json.
    queue_lock: RefCell<Option<crate::instance::Lock>>,
    /// The queue lock was not taken (why): this one is a guest. Its copies run, but it never
    /// reads or writes queue.json.
    guest: Option<crate::instance::Guest>,
    /// About's install check (`about`): its number (an answer for another one does nothing),
    /// while it runs where its answer comes from, its rows and the poll for its answer.
    check_seq: Cell<u64>,
    check_rx: RefCell<Option<Receiver<(u64, crate::check::Rows)>>>,
    check_rows: Rc<slint::VecModel<crate::CheckRow>>,
    check_timer: slint::Timer,
}

/// A part to rename: its job, the item, its name now and its new one.
type Rename = (Shared, usize, PathBuf, PathBuf);

impl App {
    pub fn new(ui: AppWindow, env: Env, prefs: prefs::Store<Prefs>) -> Rc<App> {
        let (tx, rx) = channel();
        // One writer of queue.json (RI2-5): the VibeCopy that has the queue lock. Any other runs
        // as a guest (a second instance under a squat, or elevated beside a normal one), and
        // knows why (RI4-1).
        let (queue_lock, guest) = match crate::instance::take_queue(&env.local, QUEUE_WAIT) {
            Ok(l) => (Some(l), None),
            Err(why) => (None, Some(why)),
        };
        let saver = if guest.is_some() { Saver::guest(env.local.join("queue.json")) } else { Saver::new(env.local.join("queue.json")) };
        let saving = Arc::new(Saving::default());
        let save_thread = {
            let (saving, saver) = (saving.clone(), saver.clone());
            std::thread::spawn(move || {
                loop {
                    let (seq, next_id, jobs) = {
                        let mut n = saving.next.lock().unwrap();
                        while !n.dirty && !n.stop {
                            n = saving.cv.wait(n).unwrap();
                        }
                        if !n.dirty {
                            return;
                        }
                        n.dirty = false;
                        (saving.seq.fetch_add(1, Ordering::SeqCst) + 1, n.next_id, n.jobs.clone())
                    };
                    saver.write(seq, encode(next_id, &jobs));
                }
            })
        };
        let app = Rc::new_cyclic(|me| App {
            ui,
            env,
            me: me.clone(),
            prefs,
            entries: RefCell::default(),
            next_id: Cell::new(if guest.is_some() { GUEST_IDS } else { 1 }),
            saver,
            saving,
            save_thread: RefCell::new(Some(save_thread)),
            tx,
            rx,
            view: view::Models::default(),
            detail: Cell::new(None),
            form: RefCell::default(),
            drag: Cell::new(None),
            asking: Cell::new(None),
            confirm: RefCell::default(),
            toasts: RefCell::default(),
            toast_seq: Cell::new(0),
            toast_timed: RefCell::default(),
            toast_gen: Cell::new(0),
            note: RefCell::default(),
            note_timer: RefCell::default(),
            beat: slint::Timer::default(),
            last_save: Cell::new(f64::NEG_INFINITY),
            seq: Cell::new(0),
            out: Cell::new(0),
            open_wait: Cell::new(0),
            taskbar_last: Cell::new((Bar::None, u64::MAX)),
            quitting: Cell::new(false),
            unread_toast: Cell::new(None),
            queue_lock: RefCell::new(queue_lock),
            guest,
            check_seq: Cell::new(0),
            check_rx: RefCell::default(),
            check_rows: Rc::new(slint::VecModel::from(about::rows_of(&crate::check::Rows::idle()))),
            check_timer: slint::Timer::default(),
        });
        app.wire();
        app
    }

    pub(crate) fn next_seq(&self) -> u64 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }

    pub(crate) fn now(&self) -> f64 {
        (self.env.clock)()
    }

    pub(crate) fn sender(&self) -> (Sender<Msg>, Arc<dyn Fn() + Send + Sync>) {
        (self.tx.clone(), self.env.wake.clone())
    }

    /// Start: the queue from last time comes back paused, offered.
    pub fn boot(&self) {
        if self.prefs.broken() {
            self.toast("prefs.json could not be read: the settings are back to their defaults, and the old file will be kept as prefs.json.broken.", ToastKind::Error);
        }
        if let Some(why) = &self.guest {
            // "Another VibeCopy" only when its holder is one (RI4-1).
            let first = match why {
                crate::instance::Guest::Another => "Another VibeCopy has the saved queue.".to_string(),
                crate::instance::Guest::Held(names) if names.is_empty() => "The saved queue's lock file is in use.".to_string(),
                crate::instance::Guest::Held(names) => format!("The saved queue's lock file is in use by {}.", names.join(", ")),
                crate::instance::Guest::Failed(e) => format!("The saved queue cannot be used ({}).", crate::copy::describe(e)),
            };
            self.toast(format!("{first} Copies you start here still run, but they are not saved: if this window closes or crashes, they are not offered again."), ToastKind::Error);
            self.refresh();
            return;
        }
        match job::load(self.saver.path()) {
            Loaded::Nothing => {}
            Loaded::Jobs(saved) => self.take_saved(saved),
            Loaded::Broken(aside) => self.said_broken(&aside),
            Loaded::Unreadable(why) => {
                self.saver.blocked.store(true, Ordering::SeqCst);
                self.toast(format!("The saved queue cannot be read right now ({why}). It is left as it is; nothing is saved over it until it can be read."), ToastKind::Error);
                self.unread_toast.set(Some(self.toast_seq.get()));
                // Tried again in a moment (a lock at start is often brief), and at every save.
                let me = self.me.clone();
                slint::Timer::single_shot(std::time::Duration::from_secs(2), move || {
                    if let Some(app) = me.upgrade()
                        && app.retry_queue()
                    {
                        app.save_now();
                        app.refresh();
                    }
                });
            }
        }
        self.refresh();
    }

    fn said_broken(&self, aside: &Path) {
        self.toast(format!("The saved queue was damaged and could not be read. It was kept as {}; the queue starts empty.", paths::shown(aside)), ToastKind::Error);
    }

    /// Jobs from queue.json into the queue, offered (paused). An id already taken (a job added
    /// while the file could not be read) gets a new one, never used by either queue; its parts
    /// are renamed to it, so they still resume.
    fn take_saved(&self, saved: job::Saved) {
        if saved.dropped > 0 {
            // IN-VibeCopy-1: never used, and never dropped unsaid.
            self.toast(
                format!("The saved queue named places outside its destinations, or was damaged: {} left out. Check the copies it offers.", crate::format::plural(saved.dropped as u64, "item", "items")),
                ToastKind::Error,
            );
        }
        let mut next = self.next_id.get().max(saved.next_id).max(1);
        let mut renames: Vec<Rename> = Vec::new();
        let mut e = self.entries.borrow_mut();
        for mut j in saved.jobs {
            let old = j.id;
            if e.iter().any(|x| x.id == j.id) {
                j.id = next;
                next += 1;
            }
            let sh = Arc::new(Mutex::new(j));
            let j = sh.lock().unwrap();
            let renames_before = renames.len();
            if j.id != old {
                // Its parts are named after the old id, which a job of this session has now
                // (and may use in the same folder, COR-2): they take the new one.
                let from = format!(".vibecopy-{old}-");
                for (k, it) in j.items.iter().enumerate() {
                    if let Some(t) = &it.tmp
                        && let Some(rest) = t.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(&from))
                    {
                        renames.push((sh.clone(), k, t.clone(), t.with_file_name(format!(".vibecopy-{}-{rest}", j.id))));
                    }
                }
            }
            e.push(Entry { id: j.id, drive: paths::drive_key(&j.dest), shared: sh.clone(), ctl: None, cancel_asked: false, meter: Meter::default(), restored: true, renaming: renames.len() > renames_before });
        }
        self.next_id.set(next);
        if renames.is_empty() {
            return;
        }
        // On a worker per job (the destination may be a share that does not answer: one that
        // hangs holds back its own job only, review X15RFAMA-2). A part that cannot take its new
        // name is let go (copied from the start), never used under a name that is not its job's.
        // The renames come grouped by job, in order.
        let mut groups: Vec<(u64, Vec<Rename>)> = Vec::new();
        for r in renames {
            let id = r.0.lock().unwrap().id;
            match groups.last_mut() {
                Some((g, v)) if *g == id => v.push(r),
                _ => groups.push((id, vec![r])),
            }
        }
        for (id, renames) in groups {
            let disk = self.env.disk.clone();
            let (tx, wake) = self.sender();
            self.out.set(self.out.get() + 1);
            std::thread::spawn(move || {
                for (sh, k, from, to) in renames {
                    let ok = disk.rename(&paths::long(&from), &paths::long(&to)).is_ok();
                    let mut j = sh.lock().unwrap();
                    let it = &mut j.items[k];
                    if ok {
                        it.tmp = Some(to);
                    } else {
                        it.tmp = None;
                        it.whole = false;
                        it.part = 0;
                    }
                }
                let _ = tx.send(Msg::Renamed(vec![id]));
                wake();
            });
        }
    }

    /// A queue file that could not be read at start, read again: its jobs are taken in first,
    /// and only then may the queue be written (true: it may now). Still unreadable: nothing is
    /// written, ever, over jobs this queue does not have (COA-2).
    fn retry_queue(&self) -> bool {
        if !self.saver.blocked.load(Ordering::SeqCst) {
            return false;
        }
        match job::load(self.saver.path()) {
            Loaded::Unreadable(_) => return false,
            Loaded::Nothing => {}
            Loaded::Jobs(saved) => self.take_saved(saved),
            Loaded::Broken(aside) => self.said_broken(&aside),
        }
        self.saver.blocked.store(false, Ordering::SeqCst);
        if let Some(t) = self.unread_toast.take() {
            self.dismiss_toast(t);
        }
        true
    }

    // ---------- jobs in ----------

    /// Arguments from the command line or a second start (Send To, `--to`).
    pub fn open_args(&self, args: &[OsString]) {
        match crate::cli::parse(args) {
            // A refused request: its toast is its only record, kept as a "Not added" is (RRF5-1).
            // An unknown option never gets here from this build's own starts (`main` exits 2 on
            // it first, RH-L2), only from a message something else left in the inbox.
            Err(e) => self.toast(format!("{NOT_ADDED} {e}"), ToastKind::Error),
            Ok(a) if a.sources.is_empty() => {}
            Ok(a) => match a.dest {
                // Under the Quit question a New job dialog would wait where Quit drops it unsaid,
                // and its start has already ended: left out, and said, as a drop is (RRF4-3).
                None if self.ui.get_confirm_open() => self.toast(left_out(&a.sources, "sent", "the Quit question"), ToastKind::Info),
                // A second Send To while the New job dialog is open joins it, as a drop does,
                // never replacing the first one's files unsaid (RI7-6), but only in its own mode
                // (`join_form`, RIN2-1).
                None if self.form.borrow().is_some() => self.join_form(a.sources, a.mode),
                None => self.new_job(a.sources, a.mode),
                // Exact (a trailing dot or space is part of a name); a name text cannot hold is
                // refused, never rounded to a lookalike (R2-COPY-1, R2-COPY-2).
                Some(d) => match d.to_str().ok_or_else(|| paths::unreadable(&d)).and_then(paths::normalize) {
                    Err(e) => self.toast(format!("{NOT_ADDED} {e}"), ToastKind::Error),
                    Ok(dest) => {
                        let p = self.prefs.view();
                        self.add_job(a.sources, dest, a.mode, a.verify || p.verify, Policy::from_index(p.policy), None);
                    }
                },
            },
        }
    }

    /// Second starts' messages (`instance::pending`), each taken (`instance::accept`) and then
    /// opened. A quitting window takes none (RI5-4): each start that sent one withdraws it and
    /// runs on its own. False: quitting.
    pub fn inbox(&self, msgs: &[crate::instance::Message]) -> bool {
        if self.quitting.get() {
            return false;
        }
        for m in msgs {
            if crate::instance::accept(m) {
                self.open_args(&m.args);
            }
        }
        true
    }

    /// Files that ask for `mode` while the New job dialog is open (a Send To, a paste whose
    /// clipboard read was still on its way when a Send To opened it, or a drop) join it only
    /// when it does the same, copy or move: their files are moved exactly when they asked to
    /// move (RIN2-1, RF2-1, RRF2-1). Asking the other, they are left out, and said.
    fn join_form(&self, sources: Vec<PathBuf>, mode: Mode) {
        if mode == self.form_mode() {
            self.form_add_sources(sources);
        } else {
            self.toast(not_merged(&sources, mode), ToastKind::Info);
        }
    }

    /// Files dragged over the window (each one's `HoveredFile`, all sent at once as the drag comes
    /// in): the drag's mode, the New job dialog's when it shows, else copy (RRF2-1), bound to that
    /// very dialog (RRF4-1). Set at every one, so a drag whose end never came (its source died,
    /// or its files could not be read at the drop) leaves no mode to the next (RRF3-1).
    pub fn drag_entered(&self) {
        let over = self.form.borrow().as_ref().map(|f| f.id);
        self.drag.set(Some(match over {
            Some(_) => DropMode { mode: self.form_mode(), over },
            None => DropMode::default(),
        }));
    }

    /// The drag left without a drop.
    pub fn drag_left(&self) {
        self.drag.set(None);
    }

    /// A drop came in (its first `DroppedFile`): its mode, its drag's (none seen: copy), taken
    /// now, so a drag that comes in before `main.rs` acts on this drop cannot change it (RRF3-1).
    pub fn drop_arrived(&self) -> DropMode {
        self.drag.take().unwrap_or_default()
    }

    /// Files dropped on the window, in the mode `drop_arrived` gave them. `main.rs` acts on a drop
    /// a turn of the event loop later, once its batch is in, so a Send To may have opened the
    /// dialog in between: a drop joins it only in its own mode, the one the user dropped into
    /// (RRF2-1). That mode counts only in the dialog its drag was seen over, still open; in any
    /// other (a drop with no hover of its own after a drag whose end never came, a dialog that
    /// closed and another that opened) the drop is a copy (RRF4-1). Another dialog on top, even
    /// over a New job dialog: left out, and said (RRF2-2, RRF3-6).
    pub fn dropped(&self, paths: Vec<PathBuf>, drop: DropMode) {
        if paths.is_empty() {
            return;
        }
        let open = self.form.borrow().as_ref().map(|f| f.id);
        if let Some(over) = self.dialog_over() {
            self.toast(left_out(&paths, "dropped", &over), ToastKind::Info);
        } else if open.is_some() {
            self.join_form(paths, if drop.over == open { drop.mode } else { Mode::Copy });
        } else {
            self.new_job(paths, Mode::Copy);
        }
    }

    /// Paste (the button or Ctrl+V): the clipboard is read now, on a worker. Neither reaches here
    /// while a dialog is open (the button is under its scrim, the keys go to the dialog).
    pub fn paste(&self) {
        if self.dialog_open() {
            return;
        }
        let (tx, wake) = self.sender();
        self.out.set(self.out.get() + 1);
        self.env.shell.read_clipboard(Box::new(move |result| {
            let _ = tx.send(Msg::Clip { result });
            wake();
        }));
    }

    fn pasted(&self, result: Result<Option<Clip>, String>) {
        match result {
            Err(e) => self.toast(e, ToastKind::Error),
            Ok(None) => self.toast("The clipboard holds no files. Copy or cut them in Explorer first, then paste here.", ToastKind::Info),
            Ok(Some(clip)) => {
                // `paste` reads nothing while a dialog is open, so a dialog here opened while the
                // read was on its way: a Send To's New job dialog, which the paste joins only in
                // its own mode (RF2-1), or another on top (the conflict question opens by itself
                // while a job runs), which leaves it out, said (RRF2-2, RRF3-6).
                let mode = if clip.cut { Mode::Move } else { Mode::Copy };
                if let Some(over) = self.dialog_over() {
                    self.toast(left_out(&clip.files, "pasted", &over), ToastKind::Info);
                } else if self.form.borrow().is_some() {
                    self.join_form(clip.files, mode);
                } else {
                    self.new_job(clip.files, mode);
                }
            }
        }
    }

    /// A job into the queue (its items when the dialog already walked them).
    pub(crate) fn add_job(&self, sources: Vec<PathBuf>, dest: PathBuf, mode: Mode, verify: bool, policy: Policy, walked: Option<Vec<job::Item>>) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let sources: Vec<PathBuf> = sources.iter().map(|s| paths::short(s)).collect();
        let mut j = Job::new(id, sources, dest.clone(), mode, verify, policy);
        if let Some(items) = walked {
            j.items = items;
            j.scanned = true;
        }
        self.entries.borrow_mut().push(Entry { id, drive: paths::drive_key(&dest), shared: Arc::new(Mutex::new(j)), ctl: None, cancel_asked: false, meter: Meter::default(), restored: false, renaming: false });
        self.remember_dest(&dest);
        self.save_now();
        self.schedule();
        self.refresh();
        id
    }

    fn remember_dest(&self, dest: &Path) {
        // Kept exactly (picked again, it is the folder): never the cleaned display form, and
        // never a lossy one (a name text cannot hold is refused before it gets here; R2-COPY-2).
        let Some(d) = paths::short(dest).to_str().map(str::to_string) else { return };
        self.change_prefs(move |p| {
            p.recent.retain(|r| !paths::same_path(Path::new(r), Path::new(&d)));
            p.recent.insert(0, d.clone());
            p.recent.truncate(crate::RECENT);
        });
    }

    /// One change to the prefs: kept by the store until written, and put onto the file as read
    /// right before each write (another instance's changes stay; nothing is written while the
    /// file cannot be read). Absolute, never a toggle: it may go onto more than one read. A
    /// prefs.json found garbled since start (a write kept it aside and wrote these prefs in its
    /// place) is told at the next change (family sweep 2, C44).
    pub(crate) fn change_prefs(&self, f: impl Fn(&mut crate::Prefs) + Send + Sync + 'static) {
        self.prefs.change(Arc::new(f));
        if self.prefs.take_kept_aside() {
            self.toast("prefs.json was damaged: it was kept as prefs.json.broken, and the settings were saved afresh.", ToastKind::Error);
        }
    }

    /// The theme button (the controller's, so a damaged prefs.json is told here too).
    pub(crate) fn toggle_theme(&self) {
        use slint::ComponentHandle;
        let theme = self.ui.global::<crate::Theme>();
        let dark = !theme.get_dark();
        theme.set_dark(dark);
        let t = if dark { "dark" } else { "light" };
        self.change_prefs(move |p| p.theme = Some(t.into()));
    }

    // ---------- the queue ----------

    pub(crate) fn index_of(&self, id: u64) -> Option<usize> {
        self.entries.borrow().iter().position(|e| e.id == id)
    }

    pub(crate) fn id_at(&self, i: i32) -> Option<u64> {
        self.entries.borrow().get(usize::try_from(i).ok()?).map(|e| e.id)
    }

    pub(crate) fn shared(&self, id: u64) -> Option<Shared> {
        self.entries.borrow().iter().find(|e| e.id == id).map(|e| e.shared.clone())
    }

    fn set_state(&self, id: u64, s: State) {
        if let Some(sh) = self.shared(id) {
            let mut j = sh.lock().unwrap();
            j.state = s;
            if s != State::Stopped {
                j.message.clear();
            }
        }
    }

    /// Start whatever may start now.
    pub(crate) fn schedule(&self) {
        if self.quitting.get() {
            return;
        }
        let slots: Vec<Slot> = self.entries.borrow().iter().filter(|e| !e.renaming).map(|e| Slot { id: e.id, drive: e.drive.clone(), state: e.state() }).collect();
        for id in queue::startable(&slots) {
            self.start_worker(id);
        }
    }

    fn start_worker(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        let ctl = Arc::new(Control::default());
        {
            let mut e = self.entries.borrow_mut();
            let Some(en) = e.iter_mut().find(|e| e.id == id) else { return };
            en.ctl = Some(ctl.clone());
            en.cancel_asked = false;
            en.restored = false;
            en.meter.clear();
        }
        {
            let mut j = sh.lock().unwrap();
            j.state = State::Running;
            j.message.clear();
        }
        self.out.set(self.out.get() + 1);
        let (tx, wake) = self.sender();
        let disk = self.env.disk.clone();
        std::thread::spawn(move || {
            let note_tx = Mutex::new(tx.clone());
            let note_wake = wake.clone();
            let note = move |n: Note| {
                let _ = note_tx.lock().unwrap().send(Msg::Note(n));
                note_wake();
            };
            let end = run::run(&sh, &*disk, &ctl, &note);
            let _ = tx.send(Msg::Ended { id, end });
            wake();
        });
        self.save_now();
    }

    pub fn pause(&self, id: u64) {
        let Some(i) = self.index_of(id) else { return };
        let s = self.entries.borrow()[i].state();
        match queue::pause(s) {
            Do::PauseWorker => {
                if let Some(c) = &self.entries.borrow()[i].ctl {
                    c.pause();
                }
            }
            Do::Set(st) => self.set_state(id, st),
            _ => {}
        }
        self.save_now();
        self.refresh();
    }

    pub fn resume(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        let (s, left) = {
            let j = sh.lock().unwrap();
            (j.state, answers_left(&j))
        };
        if let Do::Set(st) = queue::resume(s, left) {
            self.set_state(id, st);
        }
        let renaming = self.entries.borrow_mut().iter_mut().find(|e| e.id == id).map(|e| {
            e.restored = false;
            e.renaming
        });
        if renaming == Some(true) {
            // Said, not left as a silent "queued" (review X15RFAMA-2).
            let name = {
                let j = sh.lock().unwrap();
                crate::format::elide_middle(&job::what(&j, &j.tally()), 48)
            };
            self.toast(format!("{name} starts once its paused copies have taken their new names (their drive has not answered yet)."), ToastKind::Info);
        }
        self.schedule();
        self.save_now();
        self.refresh();
        self.maybe_ask();
    }

    pub fn cancel(&self, id: u64) {
        let Some(i) = self.index_of(id) else { return };
        let s = self.entries.borrow()[i].state();
        match queue::cancel(s) {
            Do::CancelWorker => {
                let mut e = self.entries.borrow_mut();
                e[i].cancel_asked = true;
                if let Some(c) = &e[i].ctl {
                    c.cancel();
                }
            }
            Do::Set(st) => {
                self.set_state(id, st);
                self.discard_parts(id);
            }
            _ => {}
        }
        if self.asking.get().is_some_and(|(j, _, _)| j == id) {
            self.close_ask();
        }
        self.save_now();
        self.schedule();
        self.refresh();
    }

    /// A job's paused parts go (on a worker: their drive may be slow).
    fn discard_parts(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        let disk = self.env.disk.clone();
        let (tx, wake) = self.sender();
        self.out.set(self.out.get() + 1);
        std::thread::spawn(move || {
            run::discard_parts(&sh, &*disk);
            let _ = tx.send(Msg::Discarded);
            wake();
        });
    }

    pub fn pause_all(&self) {
        let ids: Vec<u64> = self.entries.borrow().iter().map(|e| e.id).collect();
        for id in ids {
            self.pause(id);
        }
    }

    pub fn resume_all(&self) {
        let ids: Vec<u64> = self.entries.borrow().iter().map(|e| e.id).collect();
        for id in ids {
            let s = self.shared(id).map(|s| s.lock().unwrap().state);
            if matches!(s, Some(State::Paused | State::Stopped)) {
                self.resume(id);
            }
        }
    }

    /// Retry failed: the failed files of a job that is not running go again.
    pub fn retry(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        {
            let mut j = sh.lock().unwrap();
            let Do::Set(next) = queue::retry(j.state) else { return };
            let mut any = false;
            for it in &mut j.items {
                if it.state.failed() {
                    it.state = ItemState::Pending;
                    any = true;
                }
            }
            if !any {
                return;
            }
            j.rev += 1;
            j.state = next;
            j.message.clear();
        }
        self.schedule();
        self.save_now();
        self.refresh();
    }

    /// Up or down in the queue.
    pub fn shift(&self, id: u64, delta: i32) {
        let mut order: Vec<u64> = self.entries.borrow().iter().map(|e| e.id).collect();
        if !queue::shift(&mut order, id, delta) {
            return;
        }
        self.entries.borrow_mut().sort_by_key(|e| order.iter().position(|x| *x == e.id).unwrap_or(usize::MAX));
        self.save_now();
        self.schedule();
        self.refresh();
    }

    /// A finished job off the list.
    pub fn remove(&self, id: u64) {
        let finished = self.shared(id).is_some_and(|s| s.lock().unwrap().state.finished());
        if !finished {
            return;
        }
        self.entries.borrow_mut().retain(|e| e.id != id);
        if self.detail.get() == Some(id) {
            self.close_detail();
        }
        self.refresh();
    }

    pub fn clear_finished(&self) {
        let gone: Vec<u64> = self.entries.borrow().iter().filter(|e| e.state().finished()).map(|e| e.id).collect();
        for id in gone {
            self.remove(id);
        }
    }

    /// The note's Resume: every job offered from last time.
    pub fn resume_restored(&self) {
        let ids: Vec<u64> = self.entries.borrow().iter().filter(|e| e.restored).map(|e| e.id).collect();
        for id in ids {
            self.resume(id);
        }
    }

    /// The note's Discard: those jobs go, with their parts (what was copied stays).
    pub fn discard_restored(&self) {
        let ids: Vec<u64> = self.entries.borrow().iter().filter(|e| e.restored).map(|e| e.id).collect();
        for id in &ids {
            self.set_state(*id, State::Cancelled);
            self.discard_parts(*id);
        }
        self.entries.borrow_mut().retain(|e| !ids.contains(&e.id));
        self.save_now();
        self.refresh();
    }

    pub(crate) fn running_count(&self) -> usize {
        self.entries.borrow().iter().filter(|e| e.ctl.is_some()).count()
    }

    // ---------- what comes back ----------

    /// Take in what the workers sent, start what may start, and show the result.
    pub fn poll(&self) {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            match m {
                Msg::Ended { id, end } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.ended(id, end);
                }
                Msg::Note(note) => match note {
                    Note::Conflict => self.maybe_ask(),
                    Note::Walked => self.save_now(),
                },
                Msg::Counted { seq, items } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.form_counted(seq, items);
                }
                Msg::Probed { seq, probe } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.form_probed(seq, probe);
                }
                Msg::Picked { seq, path } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.form_picked(seq, path);
                }
                Msg::Clip { result } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.pasted(result);
                }
                Msg::Opened { seq, path, there } => {
                    self.out.set(self.out.get().saturating_sub(1));
                    self.opened(seq, &path, there);
                }
                Msg::Discarded => self.out.set(self.out.get().saturating_sub(1)),
                Msg::Renamed(ids) => {
                    self.out.set(self.out.get().saturating_sub(1));
                    for e in self.entries.borrow_mut().iter_mut().filter(|e| ids.contains(&e.id)) {
                        e.renaming = false;
                    }
                    self.save_now();
                    // A job resumed while its parts were renamed starts now.
                    self.schedule();
                }
            }
        }
        if changed || self.running_count() > 0 {
            self.refresh();
        }
        if self.running_count() > 0 && self.asking.get().is_none() {
            self.maybe_ask();
        }
        // While copies run, the queue is saved every few seconds (which files are done).
        if self.running_count() > 0 && self.now() - self.last_save.get() > 5.0 {
            self.save_now();
        }
    }

    fn ended(&self, id: u64, end: End) {
        let (cancel_asked, sh) = {
            let mut e = self.entries.borrow_mut();
            let Some(en) = e.iter_mut().find(|e| e.id == id) else { return };
            en.ctl = None;
            en.meter.clear();
            (std::mem::take(&mut en.cancel_asked), en.shared.clone())
        };
        let (mut state, message) = queue::ended(&end, cancel_asked);
        let (title, tally, dest, mode) = {
            let mut j = sh.lock().unwrap();
            if state == State::Waiting && !answers_left(&j) {
                state = State::Queued;
            }
            j.state = state;
            j.message = message;
            j.live.phase = job::Phase::Idle;
            j.live.current.clear();
            j.live.item = None;
            let t = j.tally();
            (crate::format::elide_middle(&job::what(&j, &t), 48), t, j.dest.clone(), j.mode)
        };
        if state == State::Cancelled && end != End::Cancelled {
            // The worker stopped some other way when the cancel came: its parts go now.
            self.discard_parts(id);
        }
        // A question about a job that is finished now goes with it.
        if state.finished() && self.asking.get().is_some_and(|(j, _, _)| j == id) {
            self.close_ask();
        }
        match state {
            State::Done if !self.quitting.get() => {
                if tally.failed > 0 {
                    self.toast_do(format!("{} failed in {}. The rest is done.", crate::format::plural(tally.failed, "file", "files"), title), ToastKind::Error, ToastDo::Show(id));
                } else {
                    let verb = if mode == Mode::Move { "Moved" } else { "Copied" };
                    let checked = if tally.hashed > 0 && tally.unchecked == 0 { ", verified" } else { "" };
                    let skipped = if tally.skipped > 0 { format!(" ({} skipped)", tally.skipped) } else { String::new() };
                    self.toast(format!("{verb} {} to {}{checked}{skipped}.", crate::format::files(tally.done), crate::format::elide_middle(&folder_name(&dest), 40)), ToastKind::Ok);
                }
            }
            State::Stopped => {
                let why = sh.lock().unwrap().message.clone();
                self.toast_do(format!("{title} stopped: {why}"), ToastKind::Error, ToastDo::Show(id));
            }
            _ => {}
        }
        self.save_now();
        self.schedule();
        self.refresh();
        self.maybe_ask();
    }

    // ---------- saving ----------

    /// The queue to disk, by the saving thread (it encodes and writes; newest wins).
    pub(crate) fn save_now(&self) {
        // A queue file unreadable at start: its jobs join first (every caller refreshes after).
        self.retry_queue();
        self.last_save.set(self.now());
        let jobs: Vec<Shared> = self.entries.borrow().iter().map(|e| e.shared.clone()).collect();
        let mut n = self.saving.next.lock().unwrap();
        n.jobs = jobs;
        n.next_id = self.next_id.get();
        n.dirty = true;
        self.saving.cv.notify_one();
    }

    /// The queue to disk now, on this thread (quitting; the tests' restarts).
    pub fn save_sync(&self) {
        self.retry_queue();
        let jobs: Vec<Shared> = self.entries.borrow().iter().map(|e| e.shared.clone()).collect();
        let seq = self.saving.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.saver.write(seq, encode(self.next_id.get(), &jobs));
    }

    // ---------- quitting ----------

    /// The window's close button. Copies running: asked first (they pause and wait for the
    /// next start). A guest's unfinished copies end with it (nothing of them is saved, RI4-2):
    /// asked first whether they run or not.
    pub fn close_requested(&self) {
        let guest = self.guest.is_some();
        let n = if guest { self.entries.borrow().iter().filter(|e| !e.state().finished()).count() } else { self.running_count() };
        if n > 0 {
            if self.dialog_open() && self.confirm.borrow().is_some() {
                return;
            }
            let title = match (n == 1, guest) {
                (true, false) => "A copy is running",
                (false, false) => "Copies are running",
                (true, true) => "A copy is not done",
                (false, true) => "Copies are not done",
            };
            self.ask_confirm(
                Confirm::Quit,
                title,
                match (n == 1, guest) {
                    (true, false) => "Quit now and it pauses where it is, inside the file. VibeCopy offers to resume it when it starts again.".into(),
                    (false, false) => "Quit now and they pause where they are, inside the file. VibeCopy offers to resume them when it starts again.".into(),
                    (true, true) => "Quit now and it ends here: this window's copies are not saved, so it is not offered again, and its partly copied files are removed. The files it copied whole stay.".into(),
                    (false, true) => "Quit now and they end here: this window's copies are not saved, so they are not offered again, and their partly copied files are removed. The files they copied whole stay.".into(),
                },
                "Quit",
                "Keep copying",
                false,
            );
            return;
        }
        self.quit();
    }

    /// Pause what runs, wait for the workers to stop (they stop inside the file, within a
    /// chunk), write the queue, leave.
    pub fn quit(&self) {
        self.quitting.set(true);
        let _ = slint::ComponentHandle::hide(&self.ui);
        for e in self.entries.borrow().iter() {
            if let Some(c) = &e.ctl {
                c.pause();
            }
        }
        let t0 = std::time::Instant::now();
        while self.running_count() > 0 && t0.elapsed().as_secs() < 15 {
            self.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if self.guest.is_some() {
            self.drop_guest_parts();
        }
        self.save_sync();
        self.prefs.finish();
        self.env.shell.quit();
    }

    /// A guest's quit (RI4-2): nothing of its jobs is saved, so the parts of its unfinished ones
    /// would stay in their destinations for good, in no queue; they go now, as a discarded
    /// offer's do. Only its own (`guest_part`), never a part of the owner's jobs. Waited for up
    /// to 10 s (a destination that does not answer keeps what it has).
    fn drop_guest_parts(&self) {
        let parts: Vec<PathBuf> = self
            .entries
            .borrow()
            .iter()
            .filter(|e| !e.state().finished())
            .flat_map(|e| {
                let mut j = e.shared.lock().unwrap();
                let id = j.id;
                j.items.iter_mut().filter_map(|it| it.tmp.take_if(|t| guest_part(id, t))).collect::<Vec<_>>()
            })
            .collect();
        if parts.is_empty() {
            return;
        }
        let disk = self.env.disk.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for p in parts {
                let _ = disk.remove_file(&paths::long(&p));
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
    }

    /// Leaving (`main`, after the event loop): the queue written, the prefs too, then the saving
    /// ended and the queue lock let go (`stop_saving`). The caller drops the instance event only
    /// after this (RI4-1), so the lock is never held without the event: a start that finds the
    /// event gone finds the lock free and owns the queue, never a guest under a false "another
    /// VibeCopy".
    pub fn finish(&self) {
        self.save_sync();
        self.prefs.finish();
        self.stop_saving();
    }

    // ---------- toasts and notes ----------

    pub(crate) fn toast(&self, text: impl Into<String>, kind: ToastKind) {
        self.toast_do(text, kind, ToastDo::None);
    }

    pub(crate) fn toast_do(&self, text: impl Into<String>, kind: ToastKind, act: ToastDo) {
        let id = self.toast_seq.get() + 1;
        self.toast_seq.set(id);
        let text = paths::clean(&text.into());
        let class = kept(&text, kind, act);
        {
            let mut t = self.toasts.borrow_mut();
            match class {
                // Each request has its own record, however alike two read, and a repeat never
                // moves the one shown (RRF6-3).
                Some(Kept::NotAdded) => {}
                // The same problem again: the one kept already says it, in its place.
                Some(Kept::Problem) if t.iter().any(|x| x.1 == text) => return,
                _ => t.retain(|x| x.1 != text || kept(&x.1, x.2, x.3).is_some()),
            }
            t.push((id, text, kind, act));
            // At most 3 on screen, so the column always fits and each one can be dismissed
            // (RRF5-1). A toast that is its news's only record is never dropped by the cap
            // (RRF3-4, RRF4-4, RRF6-4): each kind of them (`Kept`) has one place, shown one at a
            // time (`shown_toasts`). The rest are the newest: the oldest good news goes first,
            // then the oldest job error (its row keeps it, with Details). Never the newest.
            let places = [Kept::NotAdded, Kept::Problem].iter().filter(|&&k| t.iter().any(|x| kept(&x.1, x.2, x.3) == Some(k))).count();
            let room = 3 - places;
            let capped = |x: &(i32, String, ToastKind, ToastDo)| kept(&x.1, x.2, x.3).is_none();
            while t.iter().filter(|x| capped(x)).count() > room {
                let older = &t[..t.len() - 1];
                let oldest = |error: bool| older.iter().position(|x| capped(x) && (x.2 == ToastKind::Error) == error);
                match oldest(false).or_else(|| oldest(true)) {
                    Some(i) => {
                        t.remove(i);
                    }
                    None => break,
                }
            }
        }
        // Good news goes by itself; a problem stays until it is dismissed. A kept one counts its
        // time only once it shows (`time_not_added`).
        if kind != ToastKind::Error && class.is_none() {
            self.toast_timer(id);
        }
        self.time_not_added();
        self.refresh_view();
    }

    /// The toast that shows in `k`'s place: the oldest, so they show in the order they came.
    /// Under the Quit question the newest "Not added" shows instead (RRF6-2): one that came in
    /// under it is named there before Quit can end the app, as it comes; the rest are counted.
    fn head(&self, t: &[(i32, String, ToastKind, ToastDo)], k: Kept) -> Option<i32> {
        let mut of = t.iter().filter(|x| kept(&x.1, x.2, x.3) == Some(k));
        let x = if k == Kept::NotAdded && self.ui.get_confirm_open() { of.next_back() } else { of.next() };
        x.map(|x| x.0)
    }

    /// What the toast column shows: every toast but the kept ones waiting behind the one shown in
    /// their place (`head`). That one says how many there are; dismissing it shows the next.
    pub(crate) fn shown_toasts(&self) -> Vec<(i32, String, ToastKind, ToastDo)> {
        let t = self.toasts.borrow();
        let place = |k: Kept| (self.head(&t, k), t.iter().filter(|x| kept(&x.1, x.2, x.3) == Some(k)).count());
        let (not_added, problems) = (place(Kept::NotAdded), place(Kept::Problem));
        t.iter()
            .filter_map(|x| {
                let (head, n, what) = match kept(&x.1, x.2, x.3) {
                    None => return Some(x.clone()),
                    Some(Kept::NotAdded) => (not_added.0, not_added.1, "requests not added"),
                    Some(Kept::Problem) => (problems.0, problems.1, "problems"),
                };
                (head == Some(x.0)).then(|| match n > 1 {
                    true => (x.0, format!("{} (1 of {n} {what}: dismiss it to see the next.)", x.1), x.2, x.3),
                    false => x.clone(),
                })
            })
            .collect()
    }

    /// A kept toast shown, if it goes by itself, counts its time from when it shows: those
    /// waiting behind it have not been seen yet, so none goes before it shows.
    fn time_not_added(&self) {
        let first = {
            let t = self.toasts.borrow();
            self.head(&t, Kept::NotAdded).and_then(|id| t.iter().find(|x| x.0 == id).map(|x| (x.0, x.2)))
        };
        if let Some((id, kind)) = first
            && kind != ToastKind::Error
            && !self.toast_timed.borrow().contains_key(&id)
        {
            self.toast_timer(id);
        }
    }

    fn toast_timer(&self, id: i32) {
        // Numbered across all toasts, so a timer of one whose time was taken back (`dialog_closed`)
        // never matches its next one.
        let n = self.toast_gen.get() + 1;
        self.toast_gen.set(n);
        self.toast_timed.borrow_mut().insert(id, n);
        let me = self.me.clone();
        slint::Timer::single_shot(std::time::Duration::from_secs(6), move || {
            if let Some(app) = me.upgrade() {
                let current = app.toast_timed.borrow().get(&id) == Some(&n);
                // Inert behind a dialog: it waits (`dialog_closed` gives it its full time again).
                if current && !app.dialog_open() {
                    app.dismiss_toast(id);
                }
            }
        });
    }

    /// A dialog closed: every toast shown that goes by itself gets its full time again, whether
    /// its time ran out behind the dialog or was still counting. A "Not added" shown under the
    /// Quit question that now waits behind an older one (`head`) gets its time once it shows.
    pub(crate) fn dialog_closed(&self) {
        // The Quit question may have closed over another dialog: the "Not added" place shows the
        // oldest again.
        self.refresh_view();
        if self.dialog_open() {
            return;
        }
        let shown: Vec<i32> = self.shown_toasts().iter().map(|t| t.0).collect();
        self.toast_timed.borrow_mut().retain(|id, _| shown.contains(id));
        let timed: Vec<i32> = self.toast_timed.borrow().keys().copied().collect();
        for id in timed {
            self.toast_timer(id);
        }
        self.time_not_added();
        self.maybe_ask();
    }

    pub(crate) fn dismiss_toast(&self, id: i32) {
        self.toasts.borrow_mut().retain(|t| t.0 != id);
        self.toast_timed.borrow_mut().remove(&id);
        self.time_not_added();
        self.refresh_view();
    }

    pub(crate) fn toast_action(&self, id: i32) {
        let act = self.toasts.borrow().iter().find(|t| t.0 == id).map(|t| t.3);
        self.dismiss_toast(id);
        if let Some(ToastDo::Show(job)) = act {
            self.open_detail(job);
        }
    }

    /// A line in the status bar that goes after a while.
    pub(crate) fn say(&self, text: impl Into<String>) {
        *self.note.borrow_mut() = paths::clean(&text.into());
        let me = self.me.clone();
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_secs(10), move || {
            if let Some(app) = me.upgrade() {
                app.note.borrow_mut().clear();
                app.refresh_view();
            }
        });
        *self.note_timer.borrow_mut() = Some(t);
        self.refresh_view();
    }

    // ---------- open destination ----------

    /// Is it there? asked on a worker (a NAS that is off takes ~20 s to say no); Explorer opens
    /// when the answer comes, the newest click's only.
    pub fn reveal(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        let dest = sh.lock().unwrap().dest.clone();
        // What Explorer cannot open is said now, not handed over to open another folder (C35).
        if let Some(why) = paths::explorer_refuses(&dest) {
            self.open_wait.set(self.next_seq());
            self.toast(why, ToastKind::Error);
            return;
        }
        let seq = self.next_seq();
        self.open_wait.set(seq);
        self.out.set(self.out.get() + 1);
        let (tx, wake) = self.sender();
        std::thread::spawn(move || {
            // Not found is "not there"; a look that was refused is unknown, and said (C23).
            let there = match crate::copy::stat(&paths::long(&dest)) {
                Ok(s) => Ok(s.dir),
                Err(e) if crate::copy::is_gone(&e) || crate::copy::is_drive_gone(&e) => Ok(false),
                Err(e) => Err(crate::copy::describe(&e)),
            };
            let _ = tx.send(Msg::Opened { seq, path: dest, there });
            wake();
        });
    }

    fn opened(&self, seq: u64, path: &Path, there: Result<bool, String>) {
        if self.open_wait.get() != seq {
            return;
        }
        if let Err(why) = there {
            self.toast(format!("Could not look at {}: {why}.", paths::shown(path)), ToastKind::Error);
        } else if there == Ok(false) {
            self.toast(format!("{} is not there (yet).", paths::shown(path)), ToastKind::Error);
        } else if let Err(e) = self.env.shell.reveal(path) {
            self.toast(e, ToastKind::Error);
        }
    }

    // ---------- for tests ----------

    /// Wait (really) until every worker and look has answered. Tests only: the app itself
    /// never blocks the UI thread on a worker.
    pub fn settle(&self) {
        let t0 = std::time::Instant::now();
        loop {
            self.poll();
            if self.out.get() == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                self.poll();
                if self.out.get() == 0 {
                    return;
                }
            }
            assert!(t0.elapsed().as_secs() < 60, "workers did not finish ({} out)", self.out.get());
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }

    /// Another VibeCopy owns queue.json: this one never reads or writes it (RI2-5).
    pub fn is_guest(&self) -> bool {
        self.guest.is_some()
    }

    pub fn job_ids(&self) -> Vec<u64> {
        self.entries.borrow().iter().map(|e| e.id).collect()
    }

    pub fn job(&self, id: u64) -> Option<Job> {
        self.shared(id).map(|s| s.lock().unwrap().clone())
    }

    pub fn control(&self, id: u64) -> Option<Arc<Control>> {
        self.entries.borrow().iter().find(|e| e.id == id).and_then(|e| e.ctl.clone())
    }
}

/// A folder by its name ("Backup"), a drive by its root ("E:\\").
pub(crate) fn folder_name(p: &Path) -> String {
    match paths::short(p).file_name() {
        Some(n) => paths::shown_name(n),
        None => paths::shown(&paths::root_of(p)),
    }
}

/// What a Send To that asks the other mode than the open New job dialog is told (RIN2-1): its
/// files, by the first one's name, were not added, and why.
pub(crate) fn not_merged(sources: &[PathBuf], mode: Mode) -> String {
    let (what, was, them) = named(sources);
    let (asked, dialog) = if mode == Mode::Move { ("moved", "copies") } else { ("copied", "moves") };
    format!("{NOT_ADDED} {what} {was} sent to be {asked}, and the open New job dialog {dialog}. Finish or close that dialog, then send {them} again.")
}

/// How a request left out or refused begins (`not_merged`, `left_out`, `open_args`): the toast cap
/// never drops one.
const NOT_ADDED: &str = "Not added:";

/// A toast that is its news's only record, so the cap never drops it (`toast_do`). Each kind has
/// one place in the column, and shows its toasts there one at a time (`head`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kept {
    /// A request refused or left out.
    NotAdded,
    /// A problem nothing else keeps (RRF6-4): a paste that could not read the clipboard, a
    /// warning at start, what Open destination found. A job's error is not one: its row and
    /// Details keep it (`ToastDo::Show`).
    Problem,
}

fn kept(text: &str, kind: ToastKind, act: ToastDo) -> Option<Kept> {
    if text.starts_with(NOT_ADDED) {
        Some(Kept::NotAdded)
    } else if kind == ToastKind::Error && act == ToastDo::None {
        Some(Kept::Problem)
    } else {
        None
    }
}

/// A paste or drop left out because `dialog` opened while it was on its way (RRF2-2), or a Send
/// To under the Quit question (RRF4-3).
pub(crate) fn left_out(sources: &[PathBuf], how: &str, dialog: &str) -> String {
    let (what, was, them) = named(sources);
    let again = match how {
        "pasted" => "paste",
        "sent" => "send",
        _ => "drop",
    };
    format!("{NOT_ADDED} {what} {was} {how} while {dialog} was open. Close it, then {again} {them} again.")
}

/// Files as a toast names them: the first, and how many more.
fn named(sources: &[PathBuf]) -> (String, &'static str, &'static str) {
    let first = sources.first().map(|s| folder_name(s)).unwrap_or_default();
    match sources.len() {
        0 | 1 => (first, "was", "it"),
        n => (format!("{first} and {} more", crate::format::count(n as u64 - 1)), "were", "them"),
    }
}

/// A job still has conflicts nobody answered (and no "apply to all" decides them).
pub(crate) fn answers_left(j: &Job) -> bool {
    j.policy == Policy::Ask && j.items.iter().any(|it| it.state == ItemState::Conflict && it.answer.is_none())
}

impl App {
    /// The saving thread finishes what it was asked, then ends, and the queue lock goes with it
    /// (nothing of this app writes queue.json any more). Dropping the app does this; tests do it
    /// before they remove their folder (a late save put queue.json back; C52).
    pub fn stop_saving(&self) {
        if let Ok(mut n) = self.saving.next.lock() {
            n.stop = true;
        }
        self.saving.cv.notify_one();
        if let Some(t) = self.save_thread.borrow_mut().take() {
            let _ = t.join();
        }
        self.queue_lock.borrow_mut().take();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_saving();
    }
}
