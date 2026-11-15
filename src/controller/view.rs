//! The window: wired to the app once, filled from the app's state on every change and on a
//! beat while copies run.

use super::{App, Confirm, Entry, FileModel, ToastDo};
use crate::format;
use crate::job::{self, Mode, Phase, State};
use crate::taskbar::Bar;
use crate::{Ask, JobRow, NewJob, Tile, ToastItem, paths};
use slint::{ComponentHandle, Model, ModelRc, SharedString, TimerMode, VecModel};
use std::rc::Rc;
use std::time::Duration;

pub(crate) struct Models {
    pub jobs: Rc<VecModel<JobRow>>,
    pub tiles: Rc<VecModel<Tile>>,
    pub toasts: Rc<VecModel<ToastItem>>,
    pub recent: Rc<VecModel<SharedString>>,
    pub files: Rc<FileModel>,
    /// When the open file list was last caught up (it is not redrawn more than twice a second).
    pub files_at: std::cell::Cell<f64>,
}

impl Default for Models {
    fn default() -> Self {
        Models {
            jobs: Rc::new(VecModel::default()),
            tiles: Rc::new(VecModel::default()),
            toasts: Rc::new(VecModel::default()),
            recent: Rc::new(VecModel::default()),
            files: Rc::new(FileModel::default()),
            files_at: std::cell::Cell::new(f64::NEG_INFINITY),
        }
    }
}

/// Makes `model` hold `want`, touching only the rows that differ: a row whose key is in both
/// keeps its place in the model (its data is set only if it changed), so Slint keeps the item
/// built for it. Replacing the whole model (or setting rows by position) rebuilds or renames
/// items, and a screen reader then reads a toast that stayed out again (X15RFTP-2).
pub(crate) fn sync_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, want: &[T], same: impl Fn(&T, &T) -> bool) {
    for i in (0..model.row_count()).rev() {
        if let Some(row) = model.row_data(i)
            && !want.iter().any(|w| same(w, &row))
        {
            model.remove(i);
        }
    }
    for (i, w) in want.iter().enumerate() {
        match model.row_data(i) {
            Some(row) if same(w, &row) => {
                if row != *w {
                    model.set_row_data(i, w.clone());
                }
            }
            _ => model.insert(i, w.clone()),
        }
    }
    while model.row_count() > want.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Replace a model's rows, touching only those that changed (a list keeps its scroll and
/// redraws only what moved).
pub(crate) fn sync<T: Clone + PartialEq + 'static>(m: &VecModel<T>, rows: Vec<T>) {
    if m.row_count() != rows.len() {
        m.set_vec(rows);
        return;
    }
    for (i, r) in rows.into_iter().enumerate() {
        if m.row_data(i).as_ref() != Some(&r) {
            m.set_row_data(i, r);
        }
    }
}

macro_rules! on {
    ($app:expr, $ui:expr, $setter:ident, |$a:ident $(, $arg:ident)*| $body:expr) => {{
        let me = $app.me.clone();
        $ui.$setter(move |$($arg),*| {
            if let Some($a) = me.upgrade() {
                $body
            }
        });
    }};
}

impl App {
    pub(crate) fn wire(&self) {
        let ui = &self.ui;
        ui.set_jobs(ModelRc::from(self.view.jobs.clone()));
        ui.set_tiles(ModelRc::from(self.view.tiles.clone()));
        ui.set_toasts(ModelRc::from(self.view.toasts.clone()));
        ui.set_files(ModelRc::from(self.view.files.clone()));
        self.show_about(ui);
        ui.global::<NewJob>().set_recent(ModelRc::from(self.view.recent.clone()));
        on!(self, ui, on_paste, |a| a.paste());
        on!(self, ui, on_pause_all, |a| a.pause_all());
        on!(self, ui, on_resume_all, |a| a.resume_all());
        on!(self, ui, on_clear_finished, |a| a.clear_finished());
        on!(self, ui, on_open_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.open_detail(id)
            }
        });
        on!(self, ui, on_pause_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.pause(id)
            }
        });
        on!(self, ui, on_resume_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.resume(id)
            }
        });
        on!(self, ui, on_cancel_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.cancel(id)
            }
        });
        on!(self, ui, on_remove_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.remove(id)
            }
        });
        on!(self, ui, on_reveal_job, |a, i| {
            if let Some(id) = a.id_at(i) {
                a.reveal(id)
            }
        });
        on!(self, ui, on_job_menu, |a, i, entry| {
            if let Some(id) = a.id_at(i) {
                match entry {
                    0 => a.open_detail(id),
                    1 => a.shift(id, -1),
                    2 => a.shift(id, 1),
                    3 => a.retry(id),
                    5 => a.remove(id),
                    _ => {}
                }
            }
        });
        on!(self, ui, on_resume_restored, |a| a.resume_restored());
        on!(self, ui, on_discard_restored, |a| a.discard_restored());
        on!(self, ui, on_back, |a| a.close_detail());
        on!(self, ui, on_detail_retry, |a| {
            if let Some(id) = a.detail.get() {
                a.retry(id)
            }
        });
        on!(self, ui, on_detail_reveal, |a| {
            if let Some(id) = a.detail.get() {
                a.reveal(id)
            }
        });
        on!(self, ui, on_detail_pause, |a| {
            if let Some(id) = a.detail.get() {
                a.pause(id)
            }
        });
        on!(self, ui, on_detail_resume, |a| {
            if let Some(id) = a.detail.get() {
                a.resume(id)
            }
        });
        on!(self, ui, on_detail_cancel, |a| {
            if let Some(id) = a.detail.get() {
                a.cancel(id)
            }
        });
        on!(self, ui, on_new_start, |a| a.form_start());
        on!(self, ui, on_new_cancel, |a| a.form_close());
        on!(self, ui, on_confirm_accepted, |a| a.confirm_answer(true));
        on!(self, ui, on_confirm_cancelled, |a| a.confirm_answer(false));
        on!(self, ui, on_toast_dismissed, |a, id| a.dismiss_toast(id));
        on!(self, ui, on_check_install, |a| a.check_install());
        on!(self, ui, on_open_site, |a| (a.env.about.open_url)(crate::check::SITE));
        on!(self, ui, on_toggle_theme, |a| a.toggle_theme());
        on!(self, ui, on_toast_action, |a, id| a.toast_action(id));
        on!(self, ui, on_dialogs_closed, |a| a.dialog_closed());
        let f = ui.global::<NewJob>();
        on!(self, f, on_changed, |a| a.form_changed());
        on!(self, f, on_browse, |a| a.form_browse());
        on!(self, f, on_pick_recent, |a, i| a.form_pick_recent(i));
        let q = ui.global::<Ask>();
        on!(self, q, on_answer, |a, i| a.ask_answer(i));
        on!(self, q, on_cancel_job, |a| a.ask_cancel_job());
        {
            let me = self.me.clone();
            ui.window().on_close_requested(move || {
                if let Some(app) = me.upgrade() {
                    app.close_requested();
                }
                slint::CloseRequestResponse::KeepWindowShown
            });
        }
    }

    pub(crate) fn dialog_open(&self) -> bool {
        let ui = &self.ui;
        ui.get_new_open() || ui.get_ask_open() || ui.get_confirm_open() || ui.get_about_open()
    }

    /// A dialog other than New job, the top one (drawn: ask, About, Quit), as a toast names it
    /// (RRF2-2); none: no dialog, or only New job.
    pub(crate) fn dialog_over(&self) -> Option<String> {
        let ui = &self.ui;
        if ui.get_confirm_open() {
            Some("the Quit question".into())
        } else if ui.get_about_open() {
            Some("About".into())
        } else if ui.get_ask_open() {
            Some("the conflict question".into())
        } else {
            None
        }
    }

    /// The owner for Windows' own dialogs and the taskbar: the window's HWND.
    pub(crate) fn owner(&self) -> Option<isize> {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        self.ui
            .window()
            .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
                _ => None,
            })
            .flatten()
    }

    pub(crate) fn refresh(&self) {
        self.refresh_view();
    }

    /// Push everything into the window.
    pub(crate) fn refresh_view(&self) {
        let now = self.now();
        let ui = &self.ui;
        // Speeds: a sample per running job per refresh.
        {
            let mut e = self.entries.borrow_mut();
            for en in e.iter_mut() {
                if en.ctl.is_some() {
                    let copied = en.shared.lock().unwrap().live.copied;
                    en.meter.sample(now, copied);
                }
            }
        }
        let entries = self.entries.borrow();
        let busy: Vec<String> = entries.iter().filter(|e| e.ctl.is_some()).map(|e| e.drive.clone()).collect();
        let rows: Vec<JobRow> = entries.iter().map(|e| self.row(e, &busy)).collect();
        let (mut queued, mut running, mut done, mut failed) = (0u64, 0u64, 0u64, 0u64);
        let mut speed = 0.0;
        let mut any_speed = false;
        let (mut bar_done, mut bar_total) = (0u64, 0u64);
        let (mut stopped, mut paused_like, mut counting) = (false, false, false);
        let mut restored = 0;
        for e in entries.iter() {
            let j = e.shared.lock().unwrap();
            let t = j.tally();
            failed += t.failed;
            match j.state {
                State::Running => running += 1,
                State::Done => done += 1,
                State::Cancelled => {}
                _ => queued += 1,
            }
            if e.restored {
                restored += 1;
            }
            if let Some(s) = e.meter.speed() {
                speed += s;
                any_speed = true;
            }
            if !j.state.finished() {
                bar_total += t.bytes;
                bar_done += t.settled_bytes + if j.state == State::Running { j.live.cur_done } else { t.part_bytes };
                match j.state {
                    State::Stopped => stopped = true,
                    State::Running if !j.scanned => counting = true,
                    State::Running => {}
                    _ => paused_like = true,
                }
            }
        }
        drop(entries);
        sync(&self.view.jobs, rows);
        let jobs = |n: u64| format::plural(n, "job", "jobs");
        sync(
            &self.view.tiles,
            vec![
                Tile { caption: "QUEUED".into(), value: jobs(queued).into(), alert: false },
                Tile { caption: "RUNNING".into(), value: jobs(running).into(), alert: false },
                Tile { caption: "SPEED".into(), value: if any_speed { format::speed(speed).into() } else { "—".into() }, alert: false },
                Tile { caption: "DONE".into(), value: jobs(done).into(), alert: false },
                Tile { caption: "FAILED".into(), value: format::plural(failed, "file", "files").into(), alert: failed > 0 },
            ],
        );
        let states: Vec<State> = self.entries.borrow().iter().map(|e| e.state()).collect();
        ui.set_can_clear(states.iter().any(|s| s.finished()));
        ui.set_any_running(running > 0);
        ui.set_can_pause_all(states.iter().any(|s| matches!(s, State::Running | State::Queued | State::Waiting)));
        ui.set_all_paused(!states.iter().any(|s| matches!(s, State::Running | State::Queued)) && states.iter().any(|s| matches!(s, State::Paused | State::Stopped)));
        ui.set_restored(match restored {
            0 => SharedString::new(),
            1 => "1 job was left unfinished when VibeCopy closed. It waits, paused, where it stopped.".into(),
            n => format!("{n} jobs were left unfinished when VibeCopy closed. They wait, paused, where they stopped.").into(),
        });
        ui.set_status_left(SharedString::from(self.note.borrow().as_str()));
        ui.set_status_right(match states.len() {
            0 => SharedString::new(),
            n => jobs(n as u64).into(),
        });
        let toasts: Vec<ToastItem> = self
            .shown_toasts()
            .iter()
            .map(|(id, t, k, act)| ToastItem { id: *id, text: t.into(), kind: *k, action: if matches!(act, ToastDo::Show(_)) { "Show".into() } else { SharedString::new() } })
            .collect();
        sync_rows(&self.view.toasts, &toasts, |a, b| a.id == b.id);

        // The taskbar button: red when a job stopped, yellow when all waits, blue while copying.
        let bar = if bar_total == 0 && counting {
            Bar::Busy
        } else if stopped {
            Bar::Error
        } else if running > 0 {
            if bar_total == 0 { Bar::Busy } else { Bar::Normal }
        } else if paused_like {
            Bar::Paused
        } else {
            Bar::None
        };
        let permille = bar_done.saturating_mul(1000).checked_div(bar_total).unwrap_or(0);
        if self.taskbar_last.get() != (bar, permille) {
            self.taskbar_last.set((bar, permille));
            self.env.shell.taskbar(self.owner(), bar, bar_done, bar_total);
        }

        // The beat runs only while something runs.
        if self.running_count() > 0 && !self.beat.running() {
            let me = self.me.clone();
            self.beat.start(TimerMode::Repeated, Duration::from_millis(250), move || {
                if let Some(app) = me.upgrade() {
                    app.poll();
                }
            });
        } else if self.running_count() == 0 && self.beat.running() {
            self.beat.stop();
        }
        self.refresh_detail(now);
        self.refresh_ask();
    }

    fn refresh_detail(&self, now: f64) {
        let Some(id) = self.detail.get() else { return };
        let Some(i) = self.index_of(id) else {
            self.close_detail();
            return;
        };
        let busy: Vec<String> = self.entries.borrow().iter().filter(|e| e.ctl.is_some()).map(|e| e.drive.clone()).collect();
        let row = self.row(&self.entries.borrow()[i], &busy);
        let (summary, failed) = {
            let e = self.entries.borrow();
            let j = e[i].shared.lock().unwrap();
            (summary(&j), j.tally().failed)
        };
        self.ui.set_detail_can_retry(row.can_retry);
        self.ui.set_detail(row);
        self.ui.set_detail_summary(summary.into());
        self.ui.set_detail_failed(failed as i32);
        if now - self.view.files_at.get() >= 0.5 {
            self.view.files_at.set(now);
            self.view.files.update(false);
        }
    }

    pub fn open_detail(&self, id: u64) {
        let Some(sh) = self.shared(id) else { return };
        self.detail.set(Some(id));
        self.view.files.show(Some(sh));
        self.view.files_at.set(self.now());
        self.ui.set_page(1);
        self.refresh();
    }

    pub fn close_detail(&self) {
        self.detail.set(None);
        self.view.files.show(None);
        self.ui.set_page(0);
        self.refresh();
    }

    /// A job's card.
    pub(crate) fn row(&self, e: &Entry, busy: &[String]) -> JobRow {
        let j = e.shared.lock().unwrap();
        let t = j.tally();
        let moving = j.mode == Mode::Move;
        let left_answers = super::answers_left(&j);
        let (state, word) = match j.state {
            State::Queued if busy.contains(&e.drive) => (0, format!("Queued: {} is busy", paths::shown(&paths::root_of(&j.dest)).trim_end_matches('\\'))),
            State::Queued => (0, "Queued".to_string()),
            State::Running => (
                2,
                match j.live.phase {
                    Phase::Counting => "Counting".into(),
                    Phase::Verifying => "Verifying".into(),
                    Phase::Removing => "Removing originals".into(),
                    _ if t.conflicts > 0 => format!("{}, {} to answer", if moving { "Moving" } else { "Copying" }, t.conflicts),
                    _ => (if moving { "Moving" } else { "Copying" }).into(),
                },
            ),
            State::Paused => (5, "Paused".into()),
            State::Stopped => (3, "Stopped".into()),
            State::Waiting => (3, if t.conflicts == 1 { "1 conflict to answer".into() } else { format!("{} conflicts to answer", t.conflicts) }),
            State::Done if t.failed > 0 => (4, format!("Done, {} failed", t.failed)),
            State::Done => (1, "Done".into()),
            State::Cancelled => (6, "Cancelled".into()),
        };
        let settled = t.settled_bytes + if j.state == State::Running { j.live.cur_done } else { t.part_bytes };
        let numbers = if !j.scanned {
            if j.state == State::Running { format!("{} so far, {}", format::files(j.live.found_files), format::bytes(j.live.found_bytes)) } else { String::new() }
        } else if j.state.finished() {
            format!("{} · {}", format::bytes(t.bytes), format::files(t.files))
        } else {
            format!("{} of {} · {} of {}", format::bytes(settled.min(t.bytes)), format::bytes(t.bytes), format::count(t.done + t.skipped + t.failed_files), format::files(t.files))
        };
        let speed = match (j.state, e.meter.speed()) {
            (State::Running, Some(s)) if j.scanned => {
                let left = t.bytes.saturating_sub(settled);
                if s >= 1.0 && left > 0 { format!("{} · {} left", format::speed(s), format::eta(left as f64 / s)) } else { format::speed(s) }
            }
            (State::Done, _) => checked_words(&t, moving),
            _ => String::new(),
        };
        let current = match j.state {
            State::Running if j.live.phase == Phase::Counting => String::new(),
            State::Running if j.live.phase == Phase::Verifying => format!("Verifying {}", j.live.current),
            State::Running => j.live.current.clone(),
            State::Stopped => j.message.clone(),
            State::Waiting => "Answer the question to finish this job.".into(),
            State::Done if t.failed > 0 => first_failure(&j),
            _ => String::new(),
        };
        let progress = if j.state.finished() {
            -2.0
        } else if !j.scanned {
            if j.state == State::Running { -1.0 } else { -2.0 }
        } else if t.bytes > 0 {
            (settled as f64 / t.bytes as f64).clamp(0.0, 1.0) as f32
        } else if t.files + t.dirs > 0 {
            ((t.done + t.skipped + t.failed) as f32 / (t.files + t.dirs).max(1) as f32).clamp(0.0, 1.0)
        } else {
            -2.0
        };
        JobRow {
            id: j.id as i32,
            title: format!("{} {}", if moving { "Move" } else { "Copy" }, job::title(&j, &t)).into(),
            dest: paths::shown(&j.dest).into(),
            mode: (if moving { "Move" } else { "Copy" }).into(),
            state,
            word: paths::clean(&word).into(),
            numbers: numbers.into(),
            speed: speed.into(),
            current: paths::clean(&current).into(),
            progress,
            running: j.state == State::Running,
            can_pause: matches!(j.state, State::Running | State::Queued | State::Waiting),
            can_resume: matches!(j.state, State::Paused | State::Stopped) || (j.state == State::Waiting && !left_answers),
            can_cancel: !j.state.finished(),
            can_retry: t.failed > 0 && !matches!(j.state, State::Running | State::Cancelled),
            finished: j.state.finished(),
        }
    }

    pub(crate) fn ask_confirm(&self, c: Confirm, title: &str, body: String, ok: &str, no: &str, danger: bool) {
        let ui = &self.ui;
        ui.set_confirm_title(title.into());
        ui.set_confirm_body(body.into());
        ui.set_confirm_ok(ok.into());
        ui.set_confirm_no(no.into());
        ui.set_confirm_danger(danger);
        ui.set_confirm_open(true);
        *self.confirm.borrow_mut() = Some(c);
        // Under the Quit question the "Not added" place shows the newest (`head`, RRF6-2).
        self.refresh_view();
    }

    fn confirm_answer(&self, yes: bool) {
        let c = self.confirm.borrow_mut().take();
        self.ui.set_confirm_open(false);
        if let (Some(Confirm::Quit), true) = (c, yes) {
            self.quit();
            return;
        }
        self.dialog_closed();
    }
}

/// How a finished job's files were checked.
fn checked_words(t: &job::Tally, moving: bool) -> String {
    if t.done == 0 {
        return String::new();
    }
    if t.renamed == t.done && moving {
        return "Moved by renaming (same drive)".into();
    }
    if t.hashed > 0 && t.quick == 0 && t.unchecked == 0 {
        return "Verified: SHA-256 of both sides".into();
    }
    if t.hashed > 0 {
        return format!("{} verified by SHA-256", format::files(t.hashed));
    }
    // Moves found done after a restart were not checked then (RVCD4-7).
    if t.unchecked > 0 && t.quick == 0 {
        return "Moved earlier, not checked again after the restart".into();
    }
    if t.unchecked > 0 {
        return format!("{} checked by size and modified time", format::files(t.quick));
    }
    "Checked: size and modified time".into()
}

fn first_failure(j: &job::Job) -> String {
    j.items
        .iter()
        .find_map(|it| match &it.state {
            job::ItemState::Failed { reason, .. } => Some(format!("{}: {}", paths::clean(&it.rel.to_string_lossy()), reason)),
            _ => None,
        })
        .unwrap_or_default()
}

/// A job's line above its file list: what happened, and how it was checked.
pub(crate) fn summary(j: &job::Job) -> String {
    let t = j.tally();
    if !j.scanned {
        return if j.state == State::Running { "Counting what to copy…".into() } else { "Not started yet.".into() };
    }
    let mut parts: Vec<String> = Vec::new();
    let verb = if j.mode == Mode::Move { "moved" } else { "copied" };
    if t.done > 0 {
        parts.push(format!("{} {verb}", format::count(t.done)));
    }
    if t.failed > 0 {
        parts.push(format!("{} failed", format::count(t.failed)));
    }
    if t.skipped > 0 {
        parts.push(format!("{} skipped", format::count(t.skipped)));
    }
    if t.conflicts > 0 {
        parts.push(format!("{} waiting for your answer", format::count(t.conflicts)));
    }
    let left = t.left().saturating_sub(t.conflicts);
    if left > 0 {
        // A cancelled job has nothing "to go": it says what it left out (C25).
        parts.push(if j.state == State::Cancelled { format!("{} not {verb} (cancelled)", format::count(left)) } else { format!("{} to go", format::count(left)) });
    }
    if parts.is_empty() {
        parts.push("Nothing to copy".into());
    }
    let mut s = parts.join(" · ");
    // How the done files were checked: one way for all, or each way with its count (a move
    // that renamed some and copied others with SHA-256 did not verify the renamed ones).
    let ways = [
        (t.hashed, "verified by SHA-256 of both sides"),
        (t.renamed, "moved by renaming on the same drive"),
        (t.quick, "checked by size and modified time"),
        (t.unchecked, "moved earlier (not checked again after the restart)"),
    ];
    let how = match ways.iter().filter(|w| w.0 > 0).collect::<Vec<_>>().as_slice() {
        [] if j.verify => "each file verified by SHA-256 of both sides".to_string(),
        [] => "each file checked by size and modified time".to_string(),
        [(_, one)] if t.renamed > 0 || t.hashed > 0 || t.unchecked > 0 => one.to_string(),
        [(_, one)] => format!("each file {one}"),
        many => many.iter().map(|(n, w)| format!("{} {w}", format::count(*n))).collect::<Vec<_>>().join(", "),
    };
    s.push_str(" · ");
    s.push_str(&how);
    s
}
