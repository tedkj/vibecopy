//! One VibeCopy per user session, so one queue decides who uses which drive. The first start
//! owns a named event; a second start (Send To, `--to`, a double click) leaves what it was asked
//! in the first one's inbox (`<local>\inbox\*.json`, written whole), signals the event and
//! exits once the first one has taken it; not taken within `HAND_WAIT` (that one is quitting),
//! it withdraws it and runs on its own (RI5-4). Taking and withdrawing are the same claim
//! (`claim_file`, RI6-1): whoever opens the message's file first, with no sharing, owns the
//! request, so it runs exactly once. The first one reads its inbox when signalled (and once at
//! start, for a second start that raced it). Names carry a base the tests choose, so a test
//! never meets a real VibeCopy.
//!
//! Only an event this user made is trusted (2026-10-27): another process in the session could
//! make the name first, to keep VibeCopy from starting or to be handed what a second start was
//! asked. So the name carries the user's SID (`Local\` is the session's own namespace), and an
//! event found under it is used only when this user owns it (or this process's default owner
//! does, which may be Administrators when elevated) and its integrity label is not below this
//! process's own (`trusted`). Anyone else's is left alone: this start runs on its own, as when
//! no event can be made at all, rather than not at all.
//!
//! The event is made with a descriptor of its own (round 2, RI-1): only this user may open it,
//! and nothing below this process's integrity level may wait on, read or set it, so a sandbox of
//! this user can neither keep it alive after the app has gone (every later start would hand its
//! files to an inbox nobody reads) nor wake it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetKernelObjectSecurity, GetSecurityDescriptorOwner, GetSecurityDescriptorSacl, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SetKernelObjectSecurity, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel, TokenOwner, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_ALWAYS, WRITE_DAC, WRITE_OWNER};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, SetEvent, WaitForSingleObject};

/// The name the shipped app uses.
pub const NAME: &str = "com.tedkj.vibecopy";

pub struct Instance {
    event: HANDLE,
    /// A second start whose message was not taken (RI6-2): it holds the session's event, and
    /// keeps listening on it only once it is the queue's owner (`settle`).
    late: bool,
}

// A kernel object: usable from any thread.
unsafe impl Send for Instance {}

pub enum Start {
    /// This process is the one: keep `Instance` alive as long as it runs.
    First(Instance),
    /// Another one runs; it was handed the arguments and signalled. This process ends at once
    /// with this code (`main`): 0 once that one took them; `UNTOLD_EXIT` once it said it cannot
    /// tell (`UNKNOWN`), so a window that claimed the message but looks at this start only after
    /// it has ended knows which of the two it was (RF2-2).
    Second(i32),
}

/// The code a start ends with after its "may not have been done" box (`UNKNOWN`), never 0: a
/// window that took its message and finds it ended with this does not act on it (RI7-1), while
/// one that ended with 0 saw the window's claim and counted it as taken, so the window acts
/// (RF2-2). 2 is the command line's own (an option VibeCopy does not take).
pub const UNTOLD_EXIT: i32 = 3;

// ---- The trust in an instance event: the same in VibeBackup and VibeCopy. ----

/// This process's user: its TOKEN_USER, its TOKEN_OWNER (the owner its new objects get: the
/// user, or when elevated the token's default owner, which may be Administrators), its integrity
/// level and the user's SID as text.
struct User {
    token_user: Vec<u64>,
    token_owner: Vec<u64>,
    /// The RID of this process's integrity label (medium; high when elevated).
    level: u32,
    text: String,
}

/// One class of the token's information, in a buffer of its own (None: unreadable).
fn token_info(tok: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
    let mut need = 0u32;
    // SAFETY: a size query, then a buffer of `buf.len() * 8` bytes.
    unsafe {
        GetTokenInformation(tok, class, std::ptr::null_mut(), 0, &mut need);
        let mut buf = vec![0u64; (need as usize).div_ceil(8).max(8)];
        (GetTokenInformation(tok, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need) != 0).then_some(buf)
    }
}

/// The last sub-authority of `sid` (a label's level; None: it has none).
///
/// # Safety
/// `sid` is a valid SID.
unsafe fn last_rid(sid: PSID) -> Option<u32> {
    // SAFETY: the caller's SID.
    unsafe {
        let n = *GetSidSubAuthorityCount(sid);
        (n != 0).then(|| *GetSidSubAuthority(sid, u32::from(n) - 1))
    }
}

impl User {
    /// None: the token cannot be read.
    fn current() -> Option<User> {
        let mut tok: HANDLE = std::ptr::null_mut();
        // SAFETY: the current process's token, query only; closed once below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
            return None;
        }
        let (token_user, token_owner, label) = (token_info(tok, TokenUser), token_info(tok, TokenOwner), token_info(tok, TokenIntegrityLevel));
        // SAFETY: opened above.
        unsafe { CloseHandle(tok) };
        let (token_user, token_owner, label) = (token_user?, token_owner?, label?);
        // SAFETY: each buffer holds what its class says, the SIDs pointing into it; the string is
        // NUL-terminated and freed once.
        unsafe {
            let level = last_rid((*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid)?;
            let sid = (*(token_user.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut p: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut p) == 0 || p.is_null() {
                return None;
            }
            let n = (0..).take_while(|&i| *p.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            LocalFree(p.cast());
            Some(User { token_user, token_owner, level, text })
        }
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER whose SID points into it.
        unsafe { (*(self.token_user.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    /// The owners of this user's own events: the user, and this process's default owner.
    fn owners(&self) -> [PSID; 2] {
        // SAFETY: the buffer holds a TOKEN_OWNER whose SID points into it.
        [self.sid(), unsafe { (*(self.token_owner.as_ptr() as *const TOKEN_OWNER)).Owner }]
    }

    /// The level of the events this process makes and listens on: its own, never below medium.
    /// An elevated start so never trusts, nor can be signalled through, an event a normal
    /// process of this user made (RI-4).
    fn bar(&self) -> u32 {
        self.level.max(MEDIUM)
    }

    /// A descriptor for this user's own objects, labelled `level`: only this user may open them
    /// (the DACL), and no process below `level` may read, wait on or change them (no-read-up,
    /// no-execute-up, no-write-up), so a sandbox of this user can neither hold one open nor set
    /// it (RI-1).
    fn own(&self, level: u32) -> Option<Sd> {
        Sd::new(&format!("D:P(A;;GA;;;{})S:(ML;;NWNRNX;;;S-1-16-{level})", self.text))
    }
}

/// A security descriptor made from SDDL, freed on drop.
struct Sd(PSECURITY_DESCRIPTOR);

impl Sd {
    fn new(sddl: &str) -> Option<Sd> {
        let w: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut p: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string; freed by Drop.
        let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut p, std::ptr::null_mut()) } != 0;
        (ok && !p.is_null()).then_some(Sd(p))
    }

    /// Attributes that hand this descriptor to a create call (valid while `self` lives).
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: made by the conversion above.
        unsafe { LocalFree(self.0) };
    }
}

/// SYSTEM_MANDATORY_LABEL_ACE_TYPE.
const LABEL_ACE: u8 = 0x11;
/// SECURITY_MANDATORY_MEDIUM_RID: a normal (not sandboxed) process's integrity level.
const MEDIUM: u32 = 0x2000;

/// A descriptor (its owner and label) of an event this user can trust: owned by one of `mine`
/// (`User::owners`), and labelled no lower than `bar` (an event without a label is medium). A
/// process of another user cannot give an event this user (or, unelevated, Administrators) as
/// its owner, and no process can label one above its own level.
fn trusted_sd(sd: PSECURITY_DESCRIPTOR, mine: &[PSID], bar: u32) -> bool {
    let mut owner: PSID = std::ptr::null_mut();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let (mut dflt, mut present) = (0, 0);
    // SAFETY: a descriptor the caller holds; the pointers point into it; each ACE read through
    // GetAce.
    unsafe {
        if GetSecurityDescriptorOwner(sd, &mut owner, &mut dflt) == 0 || owner.is_null() || !mine.iter().any(|&m| EqualSid(owner, m) != 0) {
            return false;
        }
        if GetSecurityDescriptorSacl(sd, &mut present, &mut sacl, &mut dflt) == 0 {
            return false;
        }
        let mut level: Option<u32> = None;
        if present != 0 && !sacl.is_null() {
            for i in 0..u32::from((*sacl).AceCount) {
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                if GetAce(sacl, i, &mut p) == 0 || p.is_null() {
                    return false;
                }
                if (*(p as *const ACE_HEADER)).AceType != LABEL_ACE {
                    continue;
                }
                // The header, the mask, then the label's SID (S-1-16-<level>).
                let Some(rid) = last_rid((p as *mut u8).add(8).cast()) else { return false };
                level = Some(level.map_or(rid, |l| l.min(rid)));
            }
        }
        level.unwrap_or(MEDIUM) >= bar
    }
}

/// The event behind `h` (opened with READ_CONTROL) is one to trust (`trusted_sd`).
fn trusted(h: HANDLE, mine: &[PSID], bar: u32) -> bool {
    let info = OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let mut need = 0u32;
    // SAFETY: a size query, then a buffer of `len` bytes that lives through the judgement.
    unsafe {
        GetKernelObjectSecurity(h, info, std::ptr::null_mut(), 0, &mut need);
        if need == 0 {
            return false;
        }
        let len = need;
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        GetKernelObjectSecurity(h, info, buf.as_mut_ptr().cast(), len, &mut need) != 0 && trusted_sd(buf.as_mut_ptr().cast(), mine, bar)
    }
}

/// Create the event `name` (auto-reset, not signalled) as `me`'s own (`User::own` at
/// `User::bar`), or open it when it is there and trusted at that bar. Creating it is also how a
/// second start opens it: no gap between "is there one?" and "make one" for a third start to
/// slip through. The handle (null: it cannot be made, or it is someone else's) and whether it
/// was there.
fn make(name: &[u16], me: &User) -> (HANDLE, bool) {
    let bar = me.bar();
    let Some(sd) = me.own(bar) else { return (std::ptr::null_mut(), false) };
    let sa = sd.attributes();
    // SAFETY: a NUL-terminated name, attributes that live through the call; a handle closed
    // once (here, or by the caller).
    unsafe {
        let h = CreateEventW(&sa, 0, 0, name.as_ptr());
        if h.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
            return (h, false);
        }
        if trusted(h, &me.owners(), bar) {
            return (h, true);
        }
        CloseHandle(h);
        (std::ptr::null_mut(), true)
    }
}

// ---- End of the shared part. ----

fn event_name(base: &str, me: &User) -> Vec<u16> {
    crate::host::wide(&format!("Local\\{base}.{}.wake", me.text))
}

impl Instance {
    /// Running on its own: deaf to second starts.
    fn alone() -> Instance {
        Instance { event: std::ptr::null_mut(), late: false }
    }

    /// RI6-2: a start whose message was not taken (`claim`) still holds the session's event.
    /// Once the app knows whether it owns the queue (`owner`), it keeps it and listens as the
    /// first one would (the window that did not take it has gone, or is going: it gave up the
    /// queue lock before its event), or lets it go and runs deaf, as a guest beside that window.
    /// Holding the event across the wait leaves no moment in which the old window's event is gone
    /// and this one has none: a later start always finds the one that owns the queue. Any other
    /// instance is left as it is. Called by `on_signal` (RI7-2), so nothing listens undecided.
    fn settle(&mut self, owner: bool) {
        if self.late && !owner && !self.event.is_null() {
            // SAFETY: the handle `claim` kept, closed once.
            unsafe { CloseHandle(self.event) };
            self.event = std::ptr::null_mut();
        }
        self.late = false;
    }
}

/// Claim the session, or hand `args` (already absolute) to the instance that has it.
pub fn claim(base: &str, inbox: &Path, args: &[OsString]) -> Start {
    claim_with(base, inbox, args, &tell)
}

/// `claim`, with `say` for the words a start shows when it cannot tell whether its request was
/// taken (the tests' own; the app's is a message box, `tell`).
fn claim_with(base: &str, inbox: &Path, args: &[OsString], say: &dyn Fn(&str)) -> Start {
    let Some(me) = User::current() else { return Start::First(Instance::alone()) }; // cannot tell: run, rather than not at all
    let (h, there) = make(&event_name(base, &me), &me);
    if h.is_null() {
        return Start::First(Instance::alone()); // none can be made, or someone else's is there
    }
    if !there {
        return Start::First(Instance { event: h, late: false });
    }
    // Posted even without files (a double click), so a window that does not take it shows.
    // Not posted (the inbox cannot be written, RI6-3; too big, IN-VibeCopy-2): nothing to hand,
    // so this start runs it.
    let Ok(file) = post(inbox, args) else { return Start::First(Instance { event: h, late: true }) };
    // SAFETY: a handle made above.
    unsafe { SetEvent(h) };
    let outcome = handed(&file, HAND_WAIT, HAND_GRACE);
    if outcome == Handed::Withdrawn {
        return Start::First(Instance { event: h, late: true });
    }
    // SAFETY: a handle made above, closed once.
    unsafe { CloseHandle(h) };
    if outcome == Handed::Unknown {
        say_and_keep_claiming(&file, say);
        return Start::Second(UNTOLD_EXIT);
    }
    Start::Second(0)
}

/// RI7-1: say `UNKNOWN` (the box blocks until it is closed), and meanwhile keep claiming the
/// message (`claim_file`), so the window cannot take it once the program holding it lets go: a
/// claim this start gets withdraws it, not run, as the box says it may not have been done; "not
/// found" means the window took it the instant it could, and it shows there. Once the box is
/// closed this start exits with `UNTOLD_EXIT`; a message still held then is never acted on,
/// since its start has ended (`accept`, `pending`), nor is one the window claimed but looks at
/// again only after this exit (RF2-2). Every 1 ms at first, then every 50 ms (RIN2-5): the box
/// can be up for hours, and the holder (most likely a scanner) need not see a thousand opens a
/// second. A later claim only means a window that wins it shows the job, where the box points.
fn say_and_keep_claiming(file: &Path, say: &dyn Fn(&str)) {
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let t0 = std::time::Instant::now();
            while !done.load(std::sync::atomic::Ordering::SeqCst) && claim_file(file) == Claim::Busy {
                std::thread::sleep(std::time::Duration::from_millis(if t0.elapsed() < CLAIM_FAST { 1 } else { 50 }));
            }
        });
        say(UNKNOWN);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
    });
}

/// What a start says when its message could be neither seen taken nor withdrawn (`Handed::Unknown`).
const UNKNOWN: &str = "VibeCopy could not tell whether the open VibeCopy window took this request: another program kept its message busy. It may not have been done. Check that window before you ask again.";

/// A message box with `text`: a start has no window of its own to say it in.
fn tell(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MessageBoxW};
    let (text, title) = (crate::host::wide(text), crate::host::wide("VibeCopy"));
    // SAFETY: NUL-terminated strings that live through the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING | MB_SETFOREGROUND) };
}

/// How long a second start waits for the window to take its message.
const HAND_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long past `HAND_WAIT` a start keeps trying to claim a message that is busy (`Claim::Busy`):
/// the window's own claim lasts an instant, a scanner's open a moment.
const HAND_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a start whose box is up claims its message every 1 ms (`say_and_keep_claiming`).
const CLAIM_FAST: std::time::Duration = std::time::Duration::from_millis(500);

/// A message older than this is not acted on: a `--to` is never queued long after it was asked.
/// Its start (alive: it waits `HAND_WAIT`) withdraws it.
const STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// What a claim on a message's file came to (`claim_file`).
#[derive(Debug, PartialEq, Eq)]
enum Claim {
    /// This side has it, and it is gone: the request is this side's to act on.
    Mine,
    /// It is not there: the other side claimed it.
    Gone,
    /// Not decided: the other side is claiming it this instant (a sharing violation), something
    /// else has it open (a scanner), or the open failed otherwise.
    Busy,
}

/// FILE_FLAG_DELETE_ON_CLOSE and DELETE, as `OpenOptions` takes them.
const DELETE_ON_CLOSE: u32 = windows_sys::Win32::Storage::FileSystem::FILE_FLAG_DELETE_ON_CLOSE;
const DELETE: u32 = windows_sys::Win32::Storage::FileSystem::DELETE;

/// RI6-1: claim a message's file. It is opened with no sharing at all and delete-on-close, so the
/// kernel lets exactly one opener in (any other open of it with data or delete access is refused
/// while the handle lives), and the file goes when the handle closes, here. Both the window
/// (`accept`) and the start (`handed`) claim this way. `remove_file` is no claim: it opens with
/// delete sharing and then marks the file, so two at once can both succeed (measured: in about
/// 6 of 10 simultaneous pairs); nor is a rename (both succeed).
fn claim_file(file: &Path) -> Claim {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().access_mode(DELETE).share_mode(0).custom_flags(DELETE_ON_CLOSE).open(file) {
        Ok(_) => Claim::Mine, // the handle drops here: the file goes with it
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Claim::Gone,
        Err(_) => Claim::Busy,
    }
}

/// What became of a posted message (`handed`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Handed {
    /// The window claimed it, so it acts on it.
    Taken,
    /// This start claimed it back, so it acts on it.
    Withdrawn,
    /// Neither, within the grace: something kept it busy. Nobody is known to have acted on it.
    Unknown,
}

/// Wait up to `wait` for `file` to be taken (`accept`); then claim it (`claim_file`): this
/// start's (Withdrawn) or already gone (Taken), whichever the kernel decides, so exactly one of
/// the window and this start acts on it. Only "not found" counts as taken: a claim refused for
/// any other reason is tried again, for up to `grace` more, then Unknown.
fn handed(file: &Path, wait: std::time::Duration, grace: std::time::Duration) -> Handed {
    let t0 = std::time::Instant::now();
    // Only look meanwhile (attributes, which never stand in the window's claim's way).
    while t0.elapsed() < wait && !std::fs::symlink_metadata(file).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    loop {
        match claim_file(file) {
            Claim::Mine => return Handed::Withdrawn,
            Claim::Gone => return Handed::Taken,
            // Most often the window's own claim, which lasts an instant: soon again.
            Claim::Busy if t0.elapsed() < wait + grace => std::thread::sleep(std::time::Duration::from_millis(1)),
            Claim::Busy => return Handed::Unknown,
        }
    }
}

/// The most a message may be, and the most arguments it may carry (IN-VibeCopy-2). A command
/// line holds at most 32 767 characters, so a real one is far below both; a message above either
/// is never read whole or offered, and a start never posts one (it runs the request itself).
const MSG_MAX: u64 = 1 << 20;
const ARGS_MAX: usize = 16 * 1024;

/// A message's file name: its stamp (ns), then the id of the process that posted it. Its
/// creation time is in the body (`created`), so a later process with a reused id never passes
/// for it (RI7-1), while a window of an older build (the installer lets one run on) still reads
/// it. A message that does not say it (an older start's) is never acted on here (RIN2-2): its
/// start withdraws it after `HAND_WAIT` and runs it on its own.
fn message_name(stamp: u128, pid: u32) -> String {
    format!("{stamp:032}-{pid}.json")
}

/// The creation time (FILETIME) of the process behind `h` (None: it cannot be read).
fn created_of(h: HANDLE) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
    // SAFETY: a process handle the caller holds; four FILETIMEs to fill.
    (unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// This process's creation time (None: it cannot be read).
fn own_created() -> Option<u64> {
    // SAFETY: the current process's pseudo handle.
    created_of(unsafe { GetCurrentProcess() })
}

/// Leave `args` in the inbox, whole or not at all; the message's file. Too many or too big
/// (`ARGS_MAX`, `MSG_MAX`): refused, so the start runs the request itself.
pub fn post(inbox: &Path, args: &[OsString]) -> std::io::Result<PathBuf> {
    post_as(inbox, args, own_created())
}

/// `post`, saying `created` as this start's creation time. Unknown (None) is not said at all
/// (RIN2-7), never a false time (a mismatch would have the window take a live start's message
/// for an ended one's and sweep it, while the start counts it as taken): the window then cannot
/// tell, leaves it, and this start runs the request itself.
fn post_as(inbox: &Path, args: &[OsString], created: Option<u64>) -> std::io::Result<PathBuf> {
    #[derive(serde::Serialize)]
    struct Msg {
        #[serde(with = "crate::paths::serde_path::vec")]
        args: Vec<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created: Option<u64>,
    }
    let body = serde_json::to_vec(&Msg { args: args.iter().map(PathBuf::from).collect(), created })?;
    if args.len() > ARGS_MAX || body.len() as u64 > MSG_MAX {
        return Err(std::io::Error::other("too big for the inbox"));
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let file = inbox.join(message_name(stamp, std::process::id()));
    crate::prefs::write_atomic(&file, &body)?;
    Ok(file)
}

/// A second start's message in the inbox: its file (`accept` it before acting), its arguments,
/// and its start's creation time (None: not said, by an older build's start or one that could
/// not read it; such a message is never acted on).
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub file: PathBuf,
    pub args: Vec<OsString>,
    pub created: Option<u64>,
}

/// What is known about the start that posted a message (`sender`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sender {
    /// Proven running: its process opened, has not ended, and was made at the time the message
    /// says. Only such a message is offered and acted on.
    Runs,
    /// Proven gone: no process has its id, that process has ended, or it was made at another
    /// time (a reused id). Its message is removed unread: nobody waits for it.
    Ended,
    /// Not told (RIN2-2): the process cannot be opened (access denied: a service has the id now,
    /// or the start is one this user cannot open), its time cannot be read, or the message does
    /// not say when its start was made. Its message is neither offered nor removed: a start that
    /// still waits withdraws it after `HAND_WAIT` and runs it itself, and a gone start's never
    /// runs.
    Unknown,
}

/// The start that posted the message `name` (`message_name`: `<stamp>-<pid>.json`), whose body
/// says `created` (RI7-1, RIN2-2). Running only on positive proof.
fn sender(name: &str, created: Option<u64>) -> Sender {
    look(name, created).0
}

/// A running start's process, held open from the window's look before its claim to its look
/// after it (`accept`, RF2-2): the same process object throughout, so its end and its exit code
/// are that start's, never a later process's with its id. None in `look`: this process.
struct StartProc(HANDLE);

impl Drop for StartProc {
    fn drop(&mut self) {
        // SAFETY: opened by `look`, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// `sender`, with the start's process kept open when it runs (`StartProc`; None for any other
/// answer, and for this process).
fn look(name: &str, created: Option<u64>) -> (Sender, Option<StartProc>) {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE};
    let Some(pid) = name.get(32..).and_then(|s| s.strip_prefix('-')).and_then(|s| s.strip_suffix(".json")).and_then(|s| s.parse::<u32>().ok()) else {
        return (Sender::Ended, None); // not one a start posted
    };
    // Made when the message says (None: either is unknown).
    let by_time = |is: Option<u64>| match (created, is) {
        (Some(said), Some(is)) if said == is => Sender::Runs,
        (Some(_), Some(_)) => Sender::Ended,
        _ => Sender::Unknown,
    };
    if pid == std::process::id() {
        return (by_time(own_created()), None);
    }
    // SAFETY: a query-and-wait handle, closed once (here, or by `StartProc`).
    unsafe {
        let h = OpenProcess(SYNCHRONIZATION_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return (if GetLastError() == ERROR_INVALID_PARAMETER { Sender::Ended } else { Sender::Unknown }, None);
        }
        let held = StartProc(h);
        let s = match WaitForSingleObject(h, 0) {
            WAIT_TIMEOUT => by_time(created_of(h)),
            WAIT_OBJECT_0 => Sender::Ended,
            _ => Sender::Unknown,
        };
        (s, (s == Sender::Runs).then_some(held))
    }
}

/// RF2-2: after the window's claim, the start it saw running before it (`held`; None: this
/// process) still runs, or has ended because it saw that claim: `handed` said Taken, and `main`
/// ended it with 0 (`Start::Second(0)`). Such a start counted its request as taken, so the window
/// must act on it, however long its own thread was held up between the claim and this look. A
/// start that ended any other way is not acted on: after its "may not have been done" box
/// (`UNTOLD_EXIT`, RI7-1), or ended from outside. Unreadable: not acted on.
fn waits_or_saw_the_claim(held: Option<&StartProc>) -> bool {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    let Some(StartProc(h)) = held else { return true };
    // SAFETY: a handle `held` keeps open; one u32 to fill.
    unsafe {
        match WaitForSingleObject(*h, 0) {
            WAIT_TIMEOUT => true,
            WAIT_OBJECT_0 => {
                let mut code = 1u32;
                GetExitCodeProcess(*h, &mut code) != 0 && code == 0
            }
            _ => false,
        }
    }
}

/// The inbox's messages, oldest first, left in place: each is acted on only once `accept`ed.
/// Only a message whose start is proven gone is removed unread, by a claim (RI6-4): nobody waits
/// for it, and it never ran. Any other is left for its start, which is still waiting or cannot
/// be told from a gone one, and which claims it back itself: one whose start cannot be told
/// running (`Sender::Unknown`, RIN2-2), one that cannot be read just now (a scanner has it), one
/// that does not parse, and one older than `STALE` (or that only looks it, after a clock change)
/// are not offered. So a message is never gone from under its waiting start unless a window took
/// it to act on it.
pub fn pending(inbox: &Path) -> Vec<Message> {
    #[derive(serde::Deserialize)]
    struct Msg {
        #[serde(with = "crate::paths::serde_path::vec")]
        args: Vec<PathBuf>,
        #[serde(default)]
        created: Option<u64>,
    }
    let Ok(rd) = std::fs::read_dir(inbox) else { return Vec::new() };
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    names.sort();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut out = Vec::new();
    for p in names {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Read first (at most `MSG_MAX`, IN-VibeCopy-2): it says when its start was made. One
        // that cannot be read now says nothing, so it is removed only when its start's id has
        // no live process at all.
        let msg = read_capped(&p).and_then(|b| serde_json::from_slice::<Msg>(&b).ok());
        let created = msg.as_ref().and_then(|m| m.created);
        match sender(name, created) {
            Sender::Runs => {}
            Sender::Ended => {
                claim_file(&p);
                continue;
            }
            Sender::Unknown => continue,
        }
        let stamp = name.get(..32).and_then(|s| s.parse::<u128>().ok());
        let fresh = stamp.is_some_and(|s| now.saturating_sub(s) < STALE.as_nanos());
        if let Some(m) = msg
            && m.args.len() <= ARGS_MAX
            && fresh
        {
            out.push(Message { file: p, args: m.args.into_iter().map(PathBuf::into_os_string).collect(), created });
        }
    }
    out
}

/// A message's bytes, at most `MSG_MAX` (None: more, or it cannot be read now).
fn read_capped(p: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    std::fs::File::open(p).ok()?.take(MSG_MAX + 1).read_to_end(&mut b).ok()?;
    (b.len() as u64 <= MSG_MAX).then_some(b)
}

/// Take a message to act on it (`claim_file`): true once only, never once its start has
/// withdrawn it (`handed`), and only while its start is proven running (`Sender::Runs`). That is
/// decided before the claim (RIN2-2): a message whose start cannot be told is not claimed, so a
/// live start of that kind is never left without its request (it withdraws it and runs it), and
/// a gone one's never runs; a proven gone start's is claimed and dropped. The same process is
/// looked at again after the claim (`waits_or_saw_the_claim`): one read while its start waited
/// but taken after that start said it could not tell and exited is dropped too (RI7-1), while one
/// whose start exited because it saw this claim is acted on (RF2-2). So a request runs only for
/// a start that waited for it, and never goes unrun once its start counted it as taken.
pub fn accept(m: &Message) -> bool {
    accept_with(m, &|| {})
}

/// `accept`, with `then` run between the claim and the look after it (the tests' hold-up of the
/// window's thread there).
fn accept_with(m: &Message, then: &dyn Fn()) -> bool {
    let Some(name) = m.file.file_name().and_then(|n| n.to_str()) else { return false };
    match look(name, m.created) {
        (Sender::Runs, held) => {
            claim_file(&m.file) == Claim::Mine && {
                then();
                waits_or_saw_the_claim(held.as_ref())
            }
        }
        (Sender::Ended, _) => {
            claim_file(&m.file);
            false
        }
        (Sender::Unknown, _) => false,
    }
}

/// Every fresh message in the inbox, taken (`pending`, then `accept`), oldest first.
pub fn take(inbox: &Path) -> Vec<Vec<OsString>> {
    pending(inbox).into_iter().filter(accept).map(|m| m.args).collect()
}

impl Instance {
    /// Call `f` (on a background thread) each time a second start signals. `owner`: this app
    /// owns the queue (`!App::is_guest`). It is decided first (`settle`, RI7-2): a late start
    /// that is a guest lets the event go and listens to nothing. The thread gets its own handle
    /// to the event: dropping the `Instance` never pulls it away from under a wait.
    pub fn on_signal(&mut self, owner: bool, f: impl Fn() + Send + 'static) {
        self.settle(owner);
        if self.event.is_null() {
            return;
        }
        let mut own: HANDLE = std::ptr::null_mut();
        let ok = unsafe { DuplicateHandle(GetCurrentProcess(), self.event, GetCurrentProcess(), &mut own, 0, 0, DUPLICATE_SAME_ACCESS) };
        if ok == 0 {
            return;
        }
        let own = own as usize;
        std::thread::spawn(move || {
            let h = own as HANDLE;
            while unsafe { WaitForSingleObject(h, INFINITE) } == 0 {
                f();
            }
        });
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.event.is_null() {
            unsafe { CloseHandle(self.event) };
        }
    }
}

// ---- A lock file: the same in VibeBackup and VibeCopy. ----

/// A lock file's handle, held for as long as the lock is. The file is opened with no sharing:
/// the handle is the lock, so the OS lets go when the holder drops it or its process ends, and a
/// file left behind locks nothing. Its descriptor is this user's own at medium (`lock_file`), so
/// a sandbox of this user can neither open it nor hold the lock, while an elevated and a normal
/// instance share it.
pub struct Lock(HANDLE);

// A file handle: usable from any thread.
unsafe impl Send for Lock {}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: opened by `lock_file`, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The lock was refused because it is held (ERROR_SHARING_VIOLATION), not for another reason.
pub fn busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
}

/// Open the lock file `path` (its folder made when missing) with no sharing, as `me`'s own. The
/// label is medium whatever `me`'s own level (RI-4, RI2-4): an elevated instance's lock is a
/// normal one's to take once it is free, and is only ever busy to it, never denied. The
/// descriptor is put on the file at every take (RI3-2), so a file that was there with another
/// one (the folder's inherited descriptor lets a sandbox of this user read it, and so hold it)
/// is this user's own again before it counts as the lock. Without `me` (the token cannot be
/// read) nothing is taken or made: fail closed, never a lock file a sandbox can hold.
fn lock_file(path: &Path, me: Option<&User>) -> std::io::Result<Lock> {
    let Some(sd) = me.and_then(|me| me.own(MEDIUM)) else {
        return Err(std::io::Error::other("this user's own descriptor for it cannot be made"));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sa = sd.attributes();
    let name = crate::host::wide_path(path);
    // SAFETY: a NUL-terminated path; attributes that live through the call; the handle is
    // closed once, by `Lock`.
    let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER, 0, &sa, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let lock = Lock(h);
    // SAFETY: the handle above (WRITE_DAC for the DACL, WRITE_OWNER for the label); a
    // descriptor that lives through the call.
    if unsafe { SetKernelObjectSecurity(h, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION, sd.0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(lock)
}

// ---- End of the shared part. ----

/// Take the queue lock in the app's data folder `local` (`queue.lock`; made when missing), for as
/// long as the app runs (RI2-5): the VibeCopy that has it owns `queue.json`, and is the only one
/// that reads or writes it. Err: another VibeCopy of this user has it (`busy`), or it cannot be
/// made.
pub fn lock_queue(local: &Path) -> std::io::Result<Lock> {
    lock_file(&local.join("queue.lock"), User::current().as_ref())
}

/// The shipped exe's file name: a lock holder whose exe has it is a VibeCopy (another copy or
/// version of it).
const EXE: &str = "vibecopy.exe";

/// Why a start did not take the queue lock, so it runs as a guest (RI2-5, RI4-1).
#[derive(Debug)]
pub enum Guest {
    /// Another VibeCopy has it (the Restart Manager lists the file's holders, `judge`).
    Another,
    /// No VibeCopy is known to have it: the programs that do, after the wait, each one's name
    /// and id ("PING.EXE (process 4242)"; empty: they cannot be listed).
    Held(Vec<String>),
    /// Another error: its folder cannot be made, access is denied, and so on.
    Failed(std::io::Error),
}

/// Take the queue lock (`lock_queue`, RI4-1). Held by another VibeCopy: a guest at once. Held by
/// a program that is not known to be VibeCopy: waited for, up to `wait` (a scanner or a sync tool
/// often has a file open briefly), then a guest. Any other error: a guest at once.
pub fn take_queue(local: &Path, wait: std::time::Duration) -> Result<Lock, Guest> {
    let t0 = std::time::Instant::now();
    loop {
        let e = match lock_queue(local) {
            Ok(lock) => return Ok(lock),
            Err(e) => e,
        };
        if !busy(&e) {
            return Err(Guest::Failed(e));
        }
        match judge(holders(&local.join("queue.lock")), EXE) {
            Held::App => return Err(Guest::Another),
            Held::Others(names) if t0.elapsed() >= wait => return Err(Guest::Held(names)),
            Held::Others(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

// ---- Who holds a lock file: the same in VibeBackup and VibeCopy. ----

/// A process that has a lock file open: its id and its exe (None: it cannot be read).
type Holder = (u32, Option<std::path::PathBuf>);

/// What the holders of a busy lock file come to (`judge`).
#[derive(Debug, PartialEq, Eq)]
enum Held {
    /// One of them is this app.
    App,
    /// None is: each one's name and id, for the words ("PING.EXE (process 4242)"; empty: they
    /// cannot be listed).
    Others(Vec<String>),
}

/// The holders of a busy lock file (`holders`), judged: this app only when one of them has an exe
/// of this app's (`is_app`, `exe` its file name). Holders that cannot be listed, and one whose exe
/// cannot be read, are never taken for it (RI3-1, RI4-3): what cannot be named is said, never
/// passed over in silence.
fn judge(found: Option<Vec<Holder>>, exe: &str) -> Held {
    let Some(found) = found else { return Held::Others(Vec::new()) };
    if found.iter().any(|(_, e)| e.as_deref().is_some_and(|e| is_app(e, exe))) {
        return Held::App;
    }
    Held::Others(
        found
            .into_iter()
            .map(|(pid, e)| match e.as_deref().and_then(Path::file_name) {
                Some(n) => format!("{} (process {pid})", n.to_string_lossy()),
                None => format!("process {pid}"),
            })
            .collect(),
    )
}

/// An exe of this app: this one, or one whose file name is `exe` (another copy or version).
fn is_app(path: &Path, exe: &str) -> bool {
    let lower = |p: &Path| p.as_os_str().to_string_lossy().to_lowercase();
    path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(exe)) || std::env::current_exe().is_ok_and(|me| lower(&me) == lower(path))
}

/// The processes that have `path` open (the Restart Manager's list): each one's id and its exe
/// (None: it cannot be read). None: they cannot be listed.
fn holders(path: &Path) -> Option<Vec<Holder>> {
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::RestartManager::{CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession};
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: a key buffer of the documented size; the session is ended once, below.
    if unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) } != 0 {
        return None;
    }
    let name = crate::host::wide_path(path);
    let files = [name.as_ptr()];
    let list = (|| {
        // SAFETY: one NUL-terminated path that lives through the call.
        if unsafe { RmRegisterResources(session, 1, files.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) } != 0 {
            return None;
        }
        // SAFETY: an all-zero RM_PROCESS_INFO is a valid one (integers and arrays).
        let mut infos: Vec<RM_PROCESS_INFO> = vec![unsafe { std::mem::zeroed() }; 8];
        for _ in 0..4 {
            let (mut need, mut n, mut reasons) = (0u32, infos.len() as u32, 0u32);
            // SAFETY: a buffer of `n` entries.
            match unsafe { RmGetList(session, &mut need, &mut n, infos.as_mut_ptr(), &mut reasons) } {
                0 => {
                    infos.truncate(n as usize);
                    return Some(infos);
                }
                // SAFETY: as above.
                ERROR_MORE_DATA => infos.resize(need as usize + 4, unsafe { std::mem::zeroed() }),
                _ => return None,
            }
        }
        None
    })();
    // SAFETY: the session started above.
    unsafe { RmEndSession(session) };
    Some(list?.iter().map(|p| (p.Process.dwProcessId, exe_of(p.Process.dwProcessId))).collect())
}

/// The exe of process `pid` (None: it cannot be read).
fn exe_of(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    // SAFETY: a query-only handle, closed once; a buffer of `n` units.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0;
        CloseHandle(h);
        ok.then(|| std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize])))
    }
}

// ---- End of the shared part. ----
