//! The conflict dialog: one file that is already there, asked while the rest of its job keeps
//! copying. Shown whenever a file waits and no other dialog is open; one at a time, in queue
//! order. Everything it shows comes from the job's items (the worker noted the file that is
//! there), so asking never looks at the disk on the UI thread.

use super::App;
use crate::conflict::{Choice, SAME_TIME};
use crate::format;
use crate::job::{ItemState, State};
use crate::{Ask, paths};
use slint::ComponentHandle;

impl App {
    /// The next unanswered conflict, if nothing else is on screen.
    pub(crate) fn maybe_ask(&self) {
        if self.dialog_open() || self.asking.get().is_some() {
            return;
        }
        let found = {
            let e = self.entries.borrow();
            e.iter().filter(|en| !en.cancel_asked).find_map(|en| {
                let j = en.shared.lock().unwrap();
                if j.state.finished() || j.state == State::Paused || j.policy != crate::conflict::Policy::Ask {
                    return None;
                }
                let i = j.items.iter().position(|it| it.state == ItemState::Conflict && it.answer.is_none())?;
                Some((en.id, i, en.shared.clone()))
            })
        };
        let Some((id, i, sh)) = found else { return };
        // Everything read under the job's lock first; the window is filled after it is let go.
        let (name, folder, there, size, mtime, more, job, layout) = {
            let j = sh.lock().unwrap();
            // The item list may have grown in between (a retry's walk, FS1): asked again.
            let Some(it) = j.items.get(i).filter(|it| it.state == ItemState::Conflict) else { return };
            let parents = crate::scan::parents(&j.items);
            let target = crate::run::target_of(&j, &parents, i);
            let more = j.items.iter().filter(|x| x.state == ItemState::Conflict && x.answer.is_none()).count().saturating_sub(1);
            (
                paths::shown_name(target.file_name().unwrap_or_default()),
                format::elide_middle(&paths::shown(target.parent().unwrap_or(&j.dest)), 60),
                it.there.unwrap_or_default(),
                it.size,
                it.mtime,
                more,
                crate::job::title(&j, &j.tally()),
                j.layout,
            )
        };
        let g = self.ui.global::<Ask>();
        g.set_name(name.into());
        g.set_folder(folder.into());
        g.set_there_size(format::bytes(there.size).into());
        g.set_there_date(format::stamp(there.mtime).into());
        g.set_new_size(format::bytes(size).into());
        g.set_new_date(format::stamp(mtime).into());
        g.set_newer(if there.mtime >= mtime.saturating_add(SAME_TIME) {
            1
        } else if mtime >= there.mtime.saturating_add(SAME_TIME) {
            2
        } else {
            0
        });
        g.set_more(more as i32);
        g.set_all(false);
        g.set_job(job.into());
        self.asking.set(Some((id, i, layout)));
        self.ui.set_ask_open(true);
    }

    pub(crate) fn close_ask(&self) {
        self.asking.set(None);
        self.ui.set_ask_open(false);
        self.dialog_closed();
    }

    /// 0 skip, 1 keep both, 2 overwrite.
    pub(crate) fn ask_answer(&self, a: i32) {
        let Some((id, i, layout)) = self.asking.get() else { return };
        let choice = match a {
            1 => Choice::KeepBoth,
            2 => Choice::Overwrite,
            _ => Choice::Skip,
        };
        let all = self.ui.global::<Ask>().get_all();
        let requeue = self.shared(id).is_some_and(|sh| {
            let mut j = sh.lock().unwrap();
            // Items were put in between since it was asked (a retry's walk, FS1): index `i` may
            // be another file now. Nothing is answered; the question comes up again.
            if j.layout != layout {
                return false;
            }
            if let Some(it) = j.items.get_mut(i)
                && it.state == ItemState::Conflict
            {
                it.answer = Some(choice);
            }
            if all {
                j.policy = choice.policy();
            }
            j.rev += 1;
            // A job that waits only for answers goes back in line once none is left.
            j.state == State::Waiting && !super::answers_left(&j)
        });
        if requeue && let Some(sh) = self.shared(id) {
            sh.lock().unwrap().state = State::Queued;
        }
        self.save_now();
        self.schedule();
        self.refresh();
        // The next question, if any (it comes up as this one goes).
        self.close_ask();
    }

    pub(crate) fn ask_cancel_job(&self) {
        let Some((id, _, _)) = self.asking.get() else { return };
        // The cancel closes this question (and the next one skips this job).
        self.cancel(id);
    }
}

impl App {
    /// The open question's "n more waiting", kept current while the job finds more.
    pub(crate) fn refresh_ask(&self) {
        let Some((id, _, _)) = self.asking.get() else { return };
        let Some(sh) = self.shared(id) else { return };
        let more = sh.lock().unwrap().items.iter().filter(|x| x.state == ItemState::Conflict && x.answer.is_none()).count().saturating_sub(1);
        let g = self.ui.global::<Ask>();
        if g.get_more() != more as i32 {
            g.set_more(more as i32);
        }
    }
}
