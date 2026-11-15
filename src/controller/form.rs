//! The new job dialog: what was dropped or pasted, where it goes (typed, browsed or a recent
//! destination), copy or move, Verify, the conflict rule. The sources are walked and the
//! destination looked at on workers (a share that does not answer takes ~20 s to say so),
//! newest answer wins; meanwhile the dialog says "Checking…" and Start still works (the job
//! copes with whatever it finds).

use super::{App, Msg};
use crate::conflict::Policy;
use crate::copy::Control;
use crate::format;
use crate::job::{self, ItemState, Mode};
use crate::paths;
use crate::NewJob;
use slint::{ComponentHandle, SharedString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a worker found at the destination.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// What was asked about: the path as the field says, and for typed text ending in blanks
    /// the path without them (`paths::typed`).
    pub asked: PathBuf,
    pub trimmed: Option<PathBuf>,
    /// The folder meant (`asked`, or `trimmed` when `asked` is not there as typed).
    pub dest: PathBuf,
    /// The drive or share answers.
    pub root_ok: bool,
    pub exists: bool,
    pub is_dir: bool,
    /// Free bytes on its volume (the nearest folder that exists).
    pub free: Option<u64>,
    /// The drive or the folder could not be looked at (refused, not "not found"): what and why.
    pub unknown: Option<String>,
}

pub(crate) struct Form {
    /// This opening's own number (`next_seq`, never used again): a drag's mode goes only to the
    /// dialog it was seen over (RRF4-1).
    pub id: u64,
    pub sources: Vec<PathBuf>,
    /// The walk of the sources (None while it runs).
    pub walked: Option<Vec<job::Item>>,
    pub walk_seq: u64,
    walk_ctl: Arc<Control>,
    pub probe: Option<Probe>,
    pub probe_seq: u64,
    /// The destination last asked about.
    pub probe_for: Option<(PathBuf, Option<PathBuf>)>,
    pub pick_seq: u64,
    /// The text the app put into the field (a pick, a recent one): while the field holds it,
    /// it is exact; anything else was typed (decision R2-COPY-1).
    pub set: Option<String>,
    /// A pick refused (its name cannot be text), with the field text it left: said while the
    /// field still holds that.
    pub refused: Option<(String, String)>,
}

impl Form {
    fn typed(&self, text: &str) -> bool {
        self.set.as_deref() != Some(text)
    }
}

/// The paths the field's text may mean: exact for text the app put there, `paths::typed` for
/// typed text.
fn wanted(text: &str, typed: bool) -> Result<(PathBuf, Option<PathBuf>), String> {
    if typed { paths::typed(text) } else { paths::normalize(text).map(|p| (p, None)) }
}

/// The probe's answer for these paths, if it has come.
fn answer<'a>(probe: Option<&'a Probe>, asked: &Path, trimmed: &Option<PathBuf>) -> Option<&'a Probe> {
    probe.filter(|p| paths::same_path(&p.asked, asked) && p.trimmed == *trimmed)
}

impl Drop for Form {
    fn drop(&mut self) {
        // A closed dialog's walk stops.
        self.walk_ctl.cancel();
    }
}

/// Look at a destination (on a worker). Typed text ending in blanks (`trimmed`): the folder with
/// the blanks only when it is there as typed (decision R2-COPY-1).
pub fn probe(asked: &Path, trimmed: Option<&Path>) -> Probe {
    let dest = match trimmed {
        Some(t) if crate::copy::stat(&paths::long(asked)).is_err() => t,
        _ => asked,
    };
    Probe { asked: asked.to_path_buf(), trimmed: trimmed.map(Path::to_path_buf), ..look(dest) }
}

fn look(dest: &Path) -> Probe {
    let root = paths::root_of(dest);
    let root_st = crate::copy::stat(&paths::long(&root));
    let root_ok = root_st.as_ref().map(|s| s.dir).unwrap_or(false);
    let st = crate::copy::stat(&paths::long(dest));
    // Unknown is its own answer (C23): only "not found" means "not there" or "will be made".
    let gone = |e: &std::io::Error| crate::copy::is_gone(e) || crate::copy::is_drive_gone(e);
    let unknown = match (&root_st, &st) {
        (Err(e), _) if !gone(e) => Some(format!("{}: {}", paths::shown(&root), crate::copy::describe(e))),
        (Ok(_), Err(e)) if !crate::copy::is_gone(e) => Some(format!("{}: {}", paths::shown(dest), crate::copy::describe(e))),
        _ => None,
    };
    let (exists, is_dir) = match &st {
        Ok(s) => (true, s.dir),
        Err(_) => (false, false),
    };
    let free = if root_ok { dest.ancestors().find(|a| crate::copy::stat(&paths::long(a)).map(|s| s.dir).unwrap_or(false)).and_then(|a| crate::copy::free_space(&paths::long(a)).ok()) } else { None };
    Probe { asked: dest.to_path_buf(), trimmed: None, dest: dest.to_path_buf(), root_ok, exists, is_dir, free, unknown }
}

/// The dialog's verdict: a line, its kind (0 neutral, 1 ok, 3 warn, 4 error), and whether
/// Start may go. `typed`: the text was typed, not put there by a pick (decision R2-COPY-1).
pub fn check(sources: &[PathBuf], walked: Option<&[job::Item]>, dest_text: &str, typed: bool, mode: Mode, probe: Option<&Probe>) -> (String, i32, bool) {
    if dest_text.trim().is_empty() {
        return ("Choose where they go: type a folder, browse, or pick a recent one.".into(), 0, false);
    }
    let (asked, trimmed) = match wanted(dest_text, typed) {
        Ok(w) => w,
        Err(e) => return (e, 4, false),
    };
    let probe = answer(probe, &asked, &trimmed);
    let dest = probe.map(|p| p.dest.clone()).unwrap_or_else(|| asked.clone());
    for s in sources {
        if paths::within(&dest, s) {
            return (format!("The destination is inside {}, one of the sources. Pick another folder.", paths::shown(s)), 4, false);
        }
    }
    let same_drive = sources.iter().all(|s| paths::drive_key(s) == paths::drive_key(&dest));
    let here = !sources.is_empty() && sources.iter().all(|s| paths::short(s).parent().is_some_and(|p| paths::same_path(p, &dest)));
    if here && mode == Mode::Move {
        return ("They are in that folder already: there is nothing to move.".into(), 3, false);
    }
    let here_note = if here { " Same folder: each copy gets a (2) name." } else { "" };
    // Typed with blanks at the end: which folder is meant waits for the disk, and so does Start.
    let Some(probe) = probe else {
        return (format!("Checking {}…", paths::shown(&dest)), 0, trimmed.is_none());
    };
    if let Some(what) = &probe.unknown {
        return (format!("Could not look at {what}. Start tries anyway; whatever cannot be written is told."), 3, true);
    }
    if !probe.root_ok {
        return (format!("{} is not there. Connect the drive or share, or pick another folder.", paths::shown(&paths::root_of(&dest))), 4, false);
    }
    if probe.exists && !probe.is_dir {
        return (format!("{} is a file, not a folder.", paths::shown(&dest)), 4, false);
    }
    let made = if probe.exists { String::new() } else { format!(" {} will be made.", paths::shown(&dest)) };
    if mode == Mode::Move && same_drive {
        return (format!("Same drive: the files are moved by renaming them, at once.{made}"), 1, true);
    }
    let Some(items) = walked else {
        return (format!("Counting what to copy…{made}"), 0, true);
    };
    let bytes: u64 = items.iter().filter(|i| !i.dir && i.state == ItemState::Pending).map(|i| i.size).sum();
    let root = paths::shown(&paths::root_of(&dest));
    let across = if mode == Mode::Move { " Each file is copied and compared (size and time; its bytes too with Verify), then removed at the source." } else { "" };
    match probe.free {
        Some(free) if free < bytes => (
            format!("Needs {}, only {} free on {root}. The job pauses with a note if the disk fills up.{across}{made}", format::bytes(bytes), format::bytes(free)),
            3,
            true,
        ),
        Some(free) => (format!("{} to copy, {} free on {root}.{across}{made}{here_note}", format::bytes(bytes), format::bytes(free)), 1, true),
        None => (format!("{} to copy.{across}{made}{here_note}", format::bytes(bytes)), 1, true),
    }
}

/// "12 files, 3 folders from D:\Photos" and the totals line, for the dialog.
fn sources_lines(sources: &[PathBuf], walked: Option<&[job::Item]>) -> (String, String) {
    let mut j = job::Job::new(0, sources.to_vec(), PathBuf::new(), Mode::Copy, false, Policy::Ask);
    if let Some(items) = walked {
        j.items = items.to_vec();
        j.scanned = true;
    }
    let t = j.tally();
    let line = job::title(&j, &t);
    let sub = match walked {
        None => "Counting…".to_string(),
        Some(_) => {
            let mut s = format!("{} in {}", format::bytes(t.bytes), format::files(t.files));
            if t.dirs > 0 {
                s.push_str(&format!(", {}", format::folders(t.dirs)));
            }
            let bad: u64 = j.items.iter().filter(|i| i.state.failed()).count() as u64;
            let links: u64 = j.items.iter().filter(|i| matches!(&i.state, ItemState::Skipped(_))).count() as u64;
            if bad > 0 {
                s.push_str(&format!(" · {} cannot be read", format::count(bad)));
            }
            if links > 0 {
                s.push_str(&format!(" · {} skipped (links)", format::count(links)));
            }
            s
        }
    };
    (line, sub)
}

impl App {
    /// Open the dialog for these sources.
    pub(crate) fn new_job(&self, sources: Vec<PathBuf>, mode: Mode) {
        let p = self.prefs.view();
        let g = self.ui.global::<NewJob>();
        g.set_mode(if mode == Mode::Move { 1 } else { 0 });
        g.set_verify(p.verify);
        g.set_policy(p.policy.clamp(0, 4));
        let first = p.recent.first().cloned().unwrap_or_default();
        g.set_dest(first.as_str().into());
        // Shown cleaned; a pick takes the stored text (by its index), exactly.
        super::view::sync(&self.view.recent, p.recent.iter().map(|r| SharedString::from(paths::clean(r))).collect());
        *self.form.borrow_mut() =
            Some(Form { id: self.next_seq(), sources: Vec::new(), walked: None, walk_seq: 0, walk_ctl: Arc::new(Control::default()), probe: None, probe_seq: 0, probe_for: None, pick_seq: 0, set: Some(first), refused: None });
        self.form_add_sources(sources);
        self.ui.set_new_open(true);
        self.form_changed();
    }

    /// The open dialog's mode, as it shows it.
    pub(crate) fn form_mode(&self) -> Mode {
        if self.ui.global::<NewJob>().get_mode() == 1 { Mode::Move } else { Mode::Copy }
    }

    /// More sources (a drop or a paste while the dialog is open): walked again, all of them.
    pub(crate) fn form_add_sources(&self, more: Vec<PathBuf>) {
        let (seq, sources, ctl) = {
            let mut f = self.form.borrow_mut();
            let Some(f) = f.as_mut() else { return };
            // Sorted, not a scan per file: on the UI thread, 5 000 pasted files froze it 20 s.
            let all = f.sources.drain(..).chain(more.iter().map(|m| paths::short(m))).collect();
            f.sources = paths::unique(all);
            f.walk_ctl.cancel();
            f.walk_ctl = Arc::new(Control::default());
            f.walked = None;
            f.walk_seq = self.next_seq();
            (f.walk_seq, f.sources.clone(), f.walk_ctl.clone())
        };
        let (tx, wake) = self.sender();
        let disk = self.env.disk.clone();
        self.out.set(self.out.get() + 1);
        std::thread::spawn(move || {
            let items = crate::scan::scan(&*disk, &sources, &ctl, &|_, _| {});
            let _ = tx.send(Msg::Counted { seq, items });
            wake();
        });
        self.form_changed();
    }

    pub(crate) fn form_counted(&self, seq: u64, items: Option<Vec<job::Item>>) {
        {
            let mut f = self.form.borrow_mut();
            let Some(f) = f.as_mut() else { return };
            if f.walk_seq != seq {
                return;
            }
            f.walked = items;
        }
        self.form_changed();
    }

    /// Something in the dialog changed: check again (the destination on a worker when it is a
    /// new one).
    pub(crate) fn form_changed(&self) {
        let g = self.ui.global::<NewJob>();
        let text = g.get_dest().to_string();
        let mode = if g.get_mode() == 1 { Mode::Move } else { Mode::Copy };
        let Some(typed) = self.form.borrow().as_ref().map(|f| f.typed(&text)) else { return };
        // A destination not looked at yet: looked at now (the newest one asked wins).
        if let Ok((asked, trimmed)) = wanted(&text, typed) {
            let new = {
                let f = self.form.borrow();
                let Some(f) = f.as_ref() else { return };
                !f.probe_for.as_ref().is_some_and(|(p, t)| paths::same_path(p, &asked) && *t == trimmed)
            };
            if new {
                self.form_probe(asked, trimmed);
            }
        }
        let f = self.form.borrow();
        let Some(f) = f.as_ref() else { return };
        let (line, sub) = sources_lines(&f.sources, f.walked.as_deref());
        g.set_sources_line(line.into());
        g.set_sources_sub(sub.into());
        let (check, kind, ok) = match &f.refused {
            Some((t, why)) if *t == text => (why.clone(), 4, false),
            _ => check(&f.sources, f.walked.as_deref(), &text, typed, mode, f.probe.as_ref()),
        };
        g.set_check(check.into());
        g.set_check_kind(kind);
        g.set_can_start(ok && !f.sources.is_empty());
    }

    fn form_probe(&self, asked: PathBuf, trimmed: Option<PathBuf>) {
        let seq = self.next_seq();
        if let Some(f) = self.form.borrow_mut().as_mut() {
            f.probe_seq = seq;
            f.probe = None;
            f.probe_for = Some((asked.clone(), trimmed.clone()));
        }
        let (tx, wake) = self.sender();
        self.out.set(self.out.get() + 1);
        std::thread::spawn(move || {
            let p = probe(&asked, trimmed.as_deref());
            let _ = tx.send(Msg::Probed { seq, probe: p });
            wake();
        });
    }

    pub(crate) fn form_probed(&self, seq: u64, p: Probe) {
        {
            let mut f = self.form.borrow_mut();
            let Some(f) = f.as_mut() else { return };
            if f.probe_seq != seq {
                return;
            }
            f.probe = Some(p);
        }
        self.form_changed();
    }

    pub(crate) fn form_browse(&self) {
        let text = self.ui.global::<NewJob>().get_dest().to_string();
        let start = paths::normalize(&text).ok();
        let seq = self.next_seq();
        if let Some(f) = self.form.borrow_mut().as_mut() {
            f.pick_seq = seq;
        }
        let (tx, wake) = self.sender();
        self.out.set(self.out.get() + 1);
        self.env.shell.pick_folder(
            self.owner(),
            start,
            Box::new(move |path| {
                let _ = tx.send(Msg::Picked { seq, path });
                wake();
            }),
        );
    }

    pub(crate) fn form_picked(&self, seq: u64, path: Option<PathBuf>) {
        let ok = self.form.borrow().as_ref().is_some_and(|f| f.pick_seq == seq);
        if let (true, Some(p)) = (ok, path) {
            // Into the field as it is: the field's text is what the job goes to, so it is never
            // cleaned for display (a name with a zero-width character is another folder), and a
            // trailing dot or space stays (R2-COPY-1). A name text cannot hold (a lone surrogate)
            // would be a lookalike: refused, said in the dialog (R2-COPY-2).
            let short = paths::short(&p);
            let (text, refused) = match short.to_str() {
                Some(s) => (s.to_string(), None),
                None => {
                    let t = short.to_string_lossy().to_string();
                    (t.clone(), Some((t, paths::unreadable(&p))))
                }
            };
            if let Some(f) = self.form.borrow_mut().as_mut() {
                f.set = Some(text.clone());
                f.refused = refused;
            }
            self.ui.global::<NewJob>().set_dest(text.as_str().into());
            self.form_changed();
        }
    }

    pub(crate) fn form_pick_recent(&self, i: i32) {
        let p = self.prefs.view();
        if let Some(r) = p.recent.get(i.max(0) as usize) {
            if let Some(f) = self.form.borrow_mut().as_mut() {
                f.set = Some(r.clone());
                f.refused = None;
            }
            self.ui.global::<NewJob>().set_dest(r.as_str().into());
            self.form_changed();
        }
    }

    pub(crate) fn form_close(&self) {
        *self.form.borrow_mut() = None;
        self.ui.set_new_open(false);
        self.dialog_closed();
    }

    /// Start: the job joins the queue (with the dialog's walk, when it finished).
    pub(crate) fn form_start(&self) {
        let g = self.ui.global::<NewJob>();
        if !g.get_can_start() {
            return;
        }
        let text = g.get_dest().to_string();
        // The folder meant: for typed text ending in blanks, the disk's answer (Start waits for
        // it, so there is one).
        let dest = {
            let f = self.form.borrow();
            let Some(f) = f.as_ref() else { return };
            if f.refused.as_ref().is_some_and(|(t, _)| *t == text) {
                return;
            }
            let Ok((asked, trimmed)) = wanted(&text, f.typed(&text)) else { return };
            match (answer(f.probe.as_ref(), &asked, &trimmed), trimmed) {
                (Some(p), _) => p.dest.clone(),
                (None, None) => asked,
                (None, Some(_)) => return,
            }
        };
        let mode = if g.get_mode() == 1 { Mode::Move } else { Mode::Copy };
        let verify = g.get_verify();
        let policy = Policy::from_index(g.get_policy());
        let Some(f) = self.form.borrow_mut().take() else { return };
        let policy_index = policy.index();
        self.change_prefs(move |p| {
            p.verify = verify;
            p.policy = policy_index;
        });
        let walked = f.walked.clone();
        let sources = f.sources.clone();
        drop(f);
        self.ui.set_new_open(false);
        let id = self.add_job(sources, dest.clone(), mode, verify, policy, walked);
        let title = self.shared(id).map(|s| {
            let j = s.lock().unwrap();
            job::title(&j, &j.tally())
        });
        self.say(format!("Queued: {} to {}", title.unwrap_or_default(), paths::shown(&dest)));
        self.dialog_closed();
    }
}
