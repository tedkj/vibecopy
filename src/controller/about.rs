//! About's link and its install check (the VibeWare template's): the
//! check runs on a worker, one at a time, and the app looks whether it has its answer every
//! CHECK_POLL_MS meanwhile. About shows the last answer when it opens again.

use super::App;
use crate::check::{Dot, Fetched, Local, Release, Rows};
use crate::{AppWindow, CheckDot, CheckRow};
use slint::{ModelRc, TimerMode};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, channel};
use std::time::Duration;

/// About's outside world: the website link, the install check's two halves and what the check
/// holds the list to. `main` hands in `About::system`; the tests hand in fakes.
pub struct About {
    /// A web page in the browser (About's link).
    pub open_url: Box<dyn Fn(&str)>,
    /// The install check's offline half, on its worker: this copy's files, their signatures and
    /// hashes (`trust::inspect_from`; true: installed, so the uninstaller too).
    pub inspect: Fetcher<bool, Local>,
    /// The install check's online half, on its worker: the release list at a path on the site,
    /// waited for at most `release.wait`. Only `main` hands in the real one (the exe's
    /// `net::fetch_release_list`, in no test build); the tests hand in fakes.
    pub fetch: Fetcher<str, Fetched>,
    /// What the check holds the list to: the app, its version, the list's key, the code signer.
    pub release: Release,
}

/// A blocking call the check's worker makes.
pub type Fetcher<A, R> = Arc<dyn Fn(&A) -> R + Send + Sync>;

impl About {
    /// The real services. `fetch` is the release-list fetcher: `main` hands in the exe's
    /// `net::fetch_release_list`, and is the only place that names it.
    pub fn system(fetch: Fetcher<str, Fetched>) -> About {
        // The exe's identity as this process started from it, taken now, before an install
        // could replace it, and handed to every check (FED3, RVD7).
        let started = crate::trust::exe_identity();
        About {
            // The shell can take a moment: never on the window's thread.
            open_url: Box::new(|u| {
                let u = u.to_string();
                std::thread::spawn(move || {
                    let _ = crate::host::open_url(&u);
                });
            }),
            inspect: Arc::new(move |installed: &bool| crate::trust::inspect_from(started, &crate::trust::own_exe(), *installed)),
            fetch,
            release: Release::this(),
        }
    }
}

/// How often the app looks whether a running check has its answer.
const CHECK_POLL_MS: u64 = 50;

/// `fetch` on a thread of its own, waited for at most `deadline` (FETCH_DEADLINE, review RAB-3):
/// past it the answer is `TimedOut`, whatever the site or a proxy is doing, and the thread is
/// left to end by itself (the real fetcher's read loop stops at the same deadline, and WinHTTP's
/// own timeouts bound each step); its late answer goes nowhere.
fn fetch_within(fetch: Fetcher<str, Fetched>, path: String, deadline: Duration) -> Fetched {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch(&path));
    });
    match rx.recv_timeout(deadline) {
        Ok(f) => f,
        Err(RecvTimeoutError::Timeout) => Fetched::TimedOut,
        Err(RecvTimeoutError::Disconnected) => Fetched::Failed("the fetch stopped before it answered".into()),
    }
}

/// The rows as the window shows them.
pub(crate) fn rows_of(rows: &Rows) -> Vec<CheckRow> {
    let dot = |d: Dot| match d {
        Dot::Idle => CheckDot::Idle,
        Dot::Ok => CheckDot::Ok,
        Dot::Warn => CheckDot::Warn,
        Dot::Danger => CheckDot::Danger,
    };
    rows.all().iter().map(|(name, r)| CheckRow { name: (*name).into(), text: r.text.as_str().into(), dot: dot(r.dot) }).collect()
}

impl App {
    /// About's version line (VibeCopy is always installed: it has no portable mode) and the
    /// check's rows.
    pub(crate) fn show_about(&self, ui: &AppWindow) {
        ui.set_check_rows(ModelRc::from(self.check_rows.clone()));
        let bits = if cfg!(target_pointer_width = "64") { "64-bit" } else { "32-bit" };
        ui.set_about_line(format!("version {} · {bits} · installed for you", crate::VERSION).into());
    }

    /// "Check this install": one at a time, on a worker (it reads the files and may go online);
    /// the app looks for the answer every CHECK_POLL_MS meanwhile and goes on. Closing About
    /// leaves it running: the answer lands in the rows, which About shows when it opens again.
    pub(crate) fn check_install(&self) {
        if self.check_rx.borrow().is_some() {
            return;
        }
        self.check_seq.set(self.check_seq.get() + 1);
        let seq = self.check_seq.get();
        let (tx, rx) = channel();
        *self.check_rx.borrow_mut() = Some(rx);
        self.ui.set_checking(true);
        let about = &self.env.about;
        let (inspect, fetch, release) = (about.inspect.clone(), about.fetch.clone(), about.release.clone());
        std::thread::spawn(move || {
            let local = inspect(&true);
            // The exe updated or replaced under this one while it ran: nothing to compare (FED3).
            let rows = if local.replaced {
                crate::check::replaced_rows(&release, &local)
            } else {
                let fetched = fetch_within(fetch, crate::check::list_path(release.app), release.wait);
                crate::check::rows(&release, &local, fetched)
            };
            let _ = tx.send((seq, rows));
        });
        let w = self.me.clone();
        self.check_timer.start(TimerMode::Repeated, Duration::from_millis(CHECK_POLL_MS), move || {
            if let Some(a) = w.upgrade() {
                a.poll_check();
            }
        });
    }

    /// The check's answer, once it is there: the rows, and the button back.
    fn poll_check(&self) {
        let got = {
            let rx = self.check_rx.borrow();
            let Some(rx) = rx.as_ref() else { return };
            match rx.try_recv() {
                Ok((seq, rows)) => Some((seq == self.check_seq.get()).then_some(rows)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Some(Rows::stopped())),
            }
        };
        let Some(rows) = got else { return };
        self.check_timer.stop();
        *self.check_rx.borrow_mut() = None;
        if let Some(rows) = rows {
            self.check_rows.set_vec(rows_of(&rows));
        }
        self.ui.set_checking(false);
        self.ui.set_checked(true);
    }
}
