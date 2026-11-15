//! The queue's rules, pure: which jobs may start (one at a time per destination drive, two at
//! most, in queue order), what pause, resume and cancel do to a job in each state, how a run's
//! end lands, and reordering. The controller holds the jobs and the workers; it asks here.

use crate::job::State;
use crate::run::End;

/// At most this many jobs copy at once (to different drives: parallel copies to one disk only
/// fight over it).
pub const PARALLEL: usize = 2;

/// One job as the scheduler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub id: u64,
    /// The destination drive (`paths::drive_key`).
    pub drive: String,
    pub state: State,
}

/// The queued jobs that may start now, in queue order.
pub fn startable(slots: &[Slot]) -> Vec<u64> {
    let mut busy: Vec<&str> = slots.iter().filter(|s| s.state == State::Running).map(|s| s.drive.as_str()).collect();
    let mut out = Vec::new();
    for s in slots {
        if busy.len() >= PARALLEL {
            break;
        }
        if s.state == State::Queued && !busy.contains(&s.drive.as_str()) {
            busy.push(&s.drive);
            out.push(s.id);
        }
    }
    out
}

/// What the controller does for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Do {
    Nothing,
    /// The job takes this state now (it has no worker).
    Set(State),
    /// Its worker is told to pause (it stops inside the file; the end comes back as Paused).
    PauseWorker,
    /// Its worker is told to cancel (the part is removed; the end comes back as Cancelled).
    CancelWorker,
}

pub fn pause(s: State) -> Do {
    match s {
        State::Running => Do::PauseWorker,
        State::Queued | State::Waiting => Do::Set(State::Paused),
        // A stopped job is already still; finished ones are finished.
        _ => Do::Nothing,
    }
}

/// `answers_left`: a waiting job still has unanswered conflicts. A paused or stopped job goes
/// back in line either way (its other files copy; the run waits by itself when only answers
/// are left).
pub fn resume(s: State, answers_left: bool) -> Do {
    match s {
        State::Paused | State::Stopped => Do::Set(State::Queued),
        State::Waiting if !answers_left => Do::Set(State::Queued),
        _ => Do::Nothing,
    }
}

pub fn cancel(s: State) -> Do {
    match s {
        State::Running => Do::CancelWorker,
        s if s.finished() => Do::Nothing,
        _ => Do::Set(State::Cancelled),
    }
}

/// Retry failed: a finished or stopped job with failures goes back in line.
pub fn retry(s: State) -> Do {
    match s {
        State::Done | State::Stopped | State::Paused | State::Waiting => Do::Set(State::Queued),
        _ => Do::Nothing,
    }
}

/// The state a job lands in when its worker ends. A cancel asked for wins over whatever the
/// worker finished with (except a finish: what is done is done).
pub fn ended(end: &End, cancel_asked: bool) -> (State, String) {
    match end {
        End::Finished => (State::Done, String::new()),
        End::Cancelled => (State::Cancelled, String::new()),
        _ if cancel_asked => (State::Cancelled, String::new()),
        End::Paused => (State::Paused, String::new()),
        End::Stopped(why) => (State::Stopped, why.clone()),
        End::Waiting => (State::Waiting, String::new()),
    }
}

/// Move `id` one place up (delta -1) or down (+1) in `order`.
pub fn shift(order: &mut [u64], id: u64, delta: i32) -> bool {
    let Some(i) = order.iter().position(|x| *x == id) else { return false };
    let j = i as i64 + delta as i64;
    if j < 0 || j >= order.len() as i64 {
        return false;
    }
    order.swap(i, j as usize);
    true
}
