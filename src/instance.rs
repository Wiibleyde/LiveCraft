//! One LiveCraft per person. A later start, such as a project double-clicked
//! while the program runs, hands its project to the window already open and
//! ends, instead of running a second desk over the same session, ports and
//! outputs. The two talk through a channel only this account on this computer
//! can open (a named pipe on Windows, a socket in a private folder elsewhere):
//! no firewall is asked, and nobody else can open projects here.
//!
//! Each request is settled once, by one side: the window takes it, or the
//! start that sent it gives up on it, never both, so a project never opens in
//! two windows. A window that is busy, still starting or showing a file
//! dialog, says so and keeps the request. One that says nothing is frozen:
//! the later start runs on its own, without saving over the session.

use crate::platform::{LocalStream, connect_local, read_some};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The name real starts share; tests take their own.
pub const NAME: &str = "LiveCraft";

/// How long one start waits on the other.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// A window free to take a request that has not taken it within this
    /// long is frozen.
    pub window: Duration,
    /// How long a later start stays while the window says it is busy. Then
    /// it ends, leaving the request with the window.
    pub patience: Duration,
}

pub const TIMING: Timing = Timing { window: Duration::from_secs(8), patience: Duration::from_secs(60) };

/// How often a window that found the session taken asks for it again.
const RETRY_EVERY: Duration = Duration::from_secs(3);
/// Room for any path, and no more.
const MAX_REQUEST: usize = 256 * 1024;
/// Callers answered at once; more are turned away.
const MAX_CALLERS: usize = 8;
/// How long a caller has to say what it wants, and to hang up after the answer.
const SPEAK_WITHIN: Duration = Duration::from_secs(2);
/// How often a busy window says so.
const HEARTBEAT: Duration = Duration::from_millis(500);

/// How many things keep the window from taking requests right now.
static BUSY: AtomicUsize = AtomicUsize::new(0);

/// Runs `f` while the window cannot take requests, a file dialog say: later
/// starts are told to wait meanwhile, not taken for a frozen window.
pub fn while_busy<R>(f: impl FnOnce() -> R) -> R {
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            BUSY.fetch_sub(1, Ordering::AcqRel);
        }
    }
    BUSY.fetch_add(1, Ordering::AcqRel);
    let _done = Done;
    f()
}

/// What a later start asks of the window already open.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Request {
    /// Come to the front.
    Show,
    /// Come to the front and open this project.
    Open(String),
}

impl Request {
    /// What a start with this project, or with none, asks for.
    pub fn for_project(project: Option<&str>) -> Self {
        match project {
            // The window open works from its own folder, not this start's.
            Some(p) => Self::Open(
                std::path::absolute(p).map_or_else(|_| p.to_string(), |a| a.to_string_lossy().into_owned()),
            ),
            None => Self::Show,
        }
    }
}

/// This start's place among the LiveCraft windows of this account.
pub enum Role {
    /// The first: it answers later starts and keeps the session.
    First(First),
    /// Another window holds the session and did not answer: this one runs too,
    /// but must not save over that session while the other one is there.
    Second(Second),
    /// Later starts cannot reach this one: it runs as it always did. `foreign`
    /// when something else holds the name: another account, or a program of
    /// this one with fewer rights, which is no LiveCraft of ours.
    Alone { foreign: bool },
}

/// Becomes the first window, or hands `request` to the one already open.
/// None when it was handed over, and this start has nothing left to do.
pub fn settle(name: &str, request: &Request, timing: Timing) -> Option<Role> {
    // The first window may be closing: its name goes free within moments.
    for _ in 0..10 {
        match First::claim(name, timing) {
            Ok(first) => return Some(Role::First(first)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
            Err(_) => return Some(Role::Alone { foreign: false }),
        }
        match hand_over(name, request, timing) {
            Handed::Taken | Handed::Kept => return None,
            Handed::NobodyThere => std::thread::sleep(Duration::from_millis(50)),
            Handed::Foreign => return Some(Role::Alone { foreign: true }),
            Handed::Refused | Handed::NoAnswer => break,
        }
    }
    Some(Role::Second(Second::new(name, timing)))
}

#[derive(Debug, PartialEq)]
pub enum Handed {
    /// The window open took it.
    Taken,
    /// The window open is busy and keeps it for when it is free.
    Kept,
    /// No LiveCraft runs, or the one there is closing.
    NobodyThere,
    /// What holds the name is not this account's LiveCraft, or cannot be checked.
    Foreign,
    /// The window could not read the request.
    Refused,
    /// A window is there, but did not take it in time.
    NoAnswer,
}

/// Gives `request` to the window already open.
pub fn hand_over(name: &str, request: &Request, timing: Timing) -> Handed {
    let mut conn = match connect_local(name) {
        Ok(conn) => conn,
        Err(e) => {
            return match e.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Handed::NobodyThere,
                io::ErrorKind::PermissionDenied => Handed::Foreign,
                _ => Handed::NoAnswer,
            };
        }
    };
    let Ok(mut line) = serde_json::to_string(request) else { return Handed::Refused };
    line.push('\n');
    // Counted once connected: reaching the window is quick, or fails at once.
    let since = Instant::now();
    if conn.write_all(line.as_bytes()).is_err() {
        return Handed::NoAnswer;
    }
    let mut answers = Lines::default();
    // Between two words from the window: long enough for one free to take it.
    let silence = timing.window + Duration::from_secs(2);
    loop {
        match answers.next(&mut conn, 16, Instant::now() + silence).as_deref() {
            Ok("ok") => return Handed::Taken,
            Ok("wait") if since.elapsed() >= timing.patience => return Handed::Kept,
            Ok("wait") => {}
            Ok("gone") => return Handed::NobodyThere,
            Ok("bad") => return Handed::Refused,
            _ => return Handed::NoAnswer,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Heard {
    TooLong,
    Closed,
    Late,
    Failed,
}

/// Reads lines that arrive in pieces, each within its time.
#[derive(Default)]
struct Lines {
    held: Vec<u8>,
}

impl Lines {
    /// The next line, without its end, if it comes before `until` within
    /// `limit` bytes.
    fn next(&mut self, conn: &mut LocalStream, limit: usize, until: Instant) -> Result<String, Heard> {
        loop {
            if let Some(end) = self.held.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.held.drain(..=end).collect();
                return Ok(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            if self.held.len() > limit {
                return Err(Heard::TooLong);
            }
            let mut chunk = [0; 8192];
            match read_some(conn, &mut chunk, until) {
                Ok(0) => return Err(Heard::Closed),
                Ok(n) => self.held.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(Heard::Late),
                Err(_) => return Err(Heard::Failed),
            }
        }
    }
}

/// Says the last word, then lets the caller hang up first: hanging up at
/// once could lose the word before it is read.
fn say_last(conn: &mut LocalStream, lines: &mut Lines, word: &[u8]) {
    if conn.write_all(word).is_ok() {
        let _ = lines.next(conn, 0, Instant::now() + SPEAK_WITHIN);
    }
}

const PENDING: u8 = 0;
const TAKEN: u8 = 1;
const GIVEN_UP: u8 = 2;

/// One request handed over. Exactly one side settles it: the window takes
/// it, or the start that sent it gives up on it.
struct Ticket {
    request: Request,
    state: AtomicU8,
}

impl Ticket {
    fn new(request: Request) -> Self {
        Self { request, state: AtomicU8::new(PENDING) }
    }

    /// True for the side that settled it.
    fn settle(&self, to: u8) -> bool {
        self.state.compare_exchange(PENDING, to, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    fn taken(&self) -> bool {
        self.state.load(Ordering::Acquire) == TAKEN
    }
}

/// What the window and the threads answering callers share.
#[derive(Default)]
struct Shared {
    waker: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// The window runs its loop: before that, requests wait.
    ready: AtomicBool,
    callers: AtomicUsize,
    /// The window let the name go: callers still there are told so.
    gone: AtomicBool,
}

impl Shared {
    fn busy(&self) -> bool {
        !self.ready.load(Ordering::Acquire) || BUSY.load(Ordering::Acquire) > 0
    }

    fn wake(&self) {
        if let Some(wake) = self.waker.lock().unwrap().as_ref() {
            wake();
        }
    }
}

/// The first window's side: requests from later starts, waiting to be taken.
pub struct First {
    tickets: Receiver<Arc<Ticket>>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    name: String,
    accepting: Option<std::thread::JoinHandle<()>>,
}

impl First {
    /// Fails with `AddrInUse` while something holds the name.
    pub fn claim(name: &str, timing: Timing) -> io::Result<Self> {
        let mut listener = crate::platform::LocalListener::bind(name)?;
        let (tx, tickets) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (s, halt) = (shared.clone(), stop.clone());
        let accepting = std::thread::Builder::new().name("later starts".into()).spawn(move || {
            loop {
                let caller = listener.accept();
                if halt.load(Ordering::Acquire) {
                    break;
                }
                match caller {
                    Ok(conn) if s.callers.fetch_add(1, Ordering::AcqRel) < MAX_CALLERS => {
                        let (tx, s) = (tx.clone(), s.clone());
                        // Each caller apart: one that never speaks holds up no other.
                        std::thread::spawn(move || {
                            serve(conn, &tx, &s, timing);
                            s.callers.fetch_sub(1, Ordering::AcqRel);
                        });
                    }
                    // Too many at once: this one is turned away.
                    Ok(_) => {
                        s.callers.fetch_sub(1, Ordering::AcqRel);
                    }
                    // A caller that left at once, or no instance free: never spin.
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
        Ok(Self { tickets, shared, stop, name: name.to_string(), accepting: Some(accepting) })
    }

    /// Called as each request arrives, so that a window asleep takes it at once.
    pub fn wake_with(&self, wake: impl Fn() + Send + 'static) {
        *self.shared.waker.lock().unwrap() = Some(Box::new(wake));
    }

    /// The window runs its loop: requests are taken from now on, not told to wait.
    pub fn set_ready(&self) {
        self.shared.ready.store(true, Ordering::Release);
    }

    /// The next request handed over that its start has not given up on.
    pub fn next(&self) -> Option<Request> {
        while let Ok(ticket) = self.tickets.try_recv() {
            if ticket.settle(TAKEN) {
                return Some(ticket.request.clone());
            }
        }
        None
    }
}

impl Drop for First {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The accepting thread waits for a caller: be one, and stay until it
        // has let the name go.
        if let Ok(_wake) = connect_local(&self.name)
            && let Some(accepting) = self.accepting.take()
        {
            let _ = accepting.join();
        }
        // Only now: a caller told so finds the name free.
        self.shared.gone.store(true, Ordering::Release);
    }
}

/// Reads one request and hands it to the window. Then it answers "ok" once
/// the window took it, "wait" while the window is busy, and nothing at all
/// when a window free to take it does not: that one is frozen.
fn serve(mut conn: LocalStream, tickets: &Sender<Arc<Ticket>>, shared: &Shared, timing: Timing) {
    let mut lines = Lines::default();
    let request = match lines.next(&mut conn, MAX_REQUEST, Instant::now() + SPEAK_WITHIN) {
        Ok(line) => serde_json::from_str::<Request>(&line).ok(),
        Err(Heard::TooLong) => None,
        Err(_) => return,
    };
    let Some(request) = request else {
        // Said at once, so that the caller does not wait for nothing.
        return say_last(&mut conn, &mut lines, b"bad\n");
    };
    let ticket = Arc::new(Ticket::new(request));
    if tickets.send(ticket.clone()).is_ok() {
        shared.wake();
    }
    let (mut free_since, mut said_wait) = (None::<Instant>, None::<Instant>);
    loop {
        if ticket.taken() {
            return say_last(&mut conn, &mut lines, b"ok\n");
        }
        if shared.gone.load(Ordering::Acquire) && ticket.settle(GIVEN_UP) {
            return say_last(&mut conn, &mut lines, b"gone\n");
        }
        if shared.busy() {
            free_since = None;
            if said_wait.is_none_or(|t| t.elapsed() >= HEARTBEAT) {
                // A caller that left after hearing this trusts the window to
                // take the request later: it stays.
                if conn.write_all(b"wait\n").is_err() {
                    return;
                }
                said_wait = Some(Instant::now());
            }
        } else if free_since.get_or_insert_with(Instant::now).elapsed() >= timing.window
            && ticket.settle(GIVEN_UP)
        {
            // Free, yet not taking it: frozen. The caller runs on its own,
            // and this request is never opened here.
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A window that found the session taken. It asks again now and then, and
/// becomes the first once the other one is gone.
pub struct Second {
    name: String,
    timing: Timing,
    next_try: Instant,
}

impl Second {
    fn new(name: &str, timing: Timing) -> Self {
        Self { name: name.to_string(), timing, next_try: Instant::now() + RETRY_EVERY }
    }

    pub fn try_first(&mut self) -> Option<First> {
        if Instant::now() < self.next_try {
            return None;
        }
        self.next_try = Instant::now() + RETRY_EVERY;
        First::claim(&self.name, self.timing).ok()
    }

    /// Tests do not wait for the next try.
    #[cfg(test)]
    pub fn ask_now(&mut self) {
        self.next_try = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pipe and socket names are shared by the whole account: each test, and
    /// each run of the tests, takes its own, never the real one.
    fn unique(tag: &str) -> String {
        format!("lc-test-{}-{tag}", std::process::id())
    }

    const WAIT: Duration = Duration::from_secs(5);
    const QUICK: Timing = Timing { window: WAIT, patience: WAIT };

    /// Takes the next request as a window would, on each pass of its loop.
    fn next_request(first: &First) -> Request {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(r) = first.next() {
                return r;
            }
            assert!(Instant::now() < deadline, "nothing handed over");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn later_start(name: &str, request: Request, timing: Timing) -> std::thread::JoinHandle<Option<Role>> {
        let name = name.to_string();
        std::thread::spawn(move || settle(&name, &request, timing))
    }

    /// A first window whose loop runs.
    fn running(name: &str, timing: Timing) -> First {
        let first = First::claim(name, timing).unwrap();
        first.set_ready();
        first
    }

    #[test]
    fn a_later_start_hands_its_project_to_the_first_and_ends() {
        let name = unique("open");
        let Some(Role::First(first)) = settle(&name, &Request::Show, QUICK) else {
            panic!("the first start keeps the name");
        };
        first.set_ready();
        let woken = Arc::new(AtomicBool::new(false));
        let w = woken.clone();
        first.wake_with(move || w.store(true, Ordering::Release));
        let path = "C:/émissions/le direct\n2.livecraft".to_string();
        let later = later_start(&name, Request::Open(path.clone()), QUICK);
        assert_eq!(next_request(&first), Request::Open(path), "the whole path, whatever it holds");
        assert!(later.join().unwrap().is_none(), "the later start ends once the window took it");
        assert!(woken.load(Ordering::Acquire), "a window asleep is woken");

        let later = later_start(&name, Request::Show, QUICK);
        assert_eq!(next_request(&first), Request::Show, "a start without a project brings it forward");
        assert!(later.join().unwrap().is_none());
    }

    #[test]
    fn with_nobody_there_a_start_becomes_the_first_at_once() {
        let name = unique("none");
        let started = Instant::now();
        assert_eq!(hand_over(&name, &Request::Show, QUICK), Handed::NobodyThere);
        assert!(started.elapsed() < Duration::from_secs(2), "no waiting when nobody runs");
        assert!(matches!(settle(&name, &Request::Show, QUICK), Some(Role::First(_))));
    }

    #[test]
    fn the_name_goes_free_when_the_first_window_closes() {
        let name = unique("free");
        let first = First::claim(&name, QUICK).unwrap();
        let again = First::claim(&name, QUICK).err().expect("held while the first runs");
        assert_eq!(again.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        assert!(First::claim(&name, QUICK).is_ok(), "free again once it closed");
    }

    /// A window free to take requests that does not, frozen, is not waited
    /// on: the later start runs on its own, without the session, until that
    /// window is gone.
    #[test]
    fn a_window_that_does_not_answer_is_not_waited_on() {
        let name = unique("frozen");
        let short = Timing { window: Duration::from_millis(300), patience: WAIT };
        let first = running(&name, short);
        let started = Instant::now();
        let Some(Role::Second(mut second)) = settle(&name, &Request::Open("x.livecraft".into()), short)
        else {
            panic!("runs on its own");
        };
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(first.next(), None, "a request given up on is never opened later");
        second.ask_now();
        assert!(second.try_first().is_none(), "the session stays with the first while it runs");
        drop(first);
        assert!(second.try_first().is_none(), "asked again only now and then");
        second.ask_now();
        assert!(second.try_first().is_some(), "and comes to this window once the first is gone");
    }

    /// A window still starting, or showing a file dialog, is busy, not
    /// frozen: the later start waits, then leaves the request with it.
    #[test]
    fn a_busy_window_keeps_the_request_for_later() {
        let name = unique("busy");
        let timing = Timing { window: Duration::from_millis(300), patience: Duration::from_millis(1500) };
        // Its loop has not run yet.
        let first = First::claim(&name, timing).unwrap();
        let started = Instant::now();
        assert_eq!(hand_over(&name, &Request::Open("y.livecraft".into()), timing), Handed::Kept);
        assert!(started.elapsed() >= timing.patience, "waited as long as the window said to");
        first.set_ready();
        assert_eq!(next_request(&first), Request::Open("y.livecraft".into()), "taken once free");
    }

    /// A process stuck whole, its listening thread too, answers nothing at all.
    #[test]
    fn a_process_that_froze_is_not_waited_on() {
        let name = unique("stuck");
        let _listening_but_stuck = crate::platform::LocalListener::bind(&name).unwrap();
        let timing = Timing { window: Duration::from_millis(300), patience: WAIT };
        let started = Instant::now();
        assert_eq!(hand_over(&name, &Request::Show, timing), Handed::NoAnswer);
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    }

    /// Only one side ever settles a request, whichever comes first.
    #[test]
    fn a_request_is_taken_or_given_up_never_both() {
        let ticket = Ticket::new(Request::Show);
        assert!(ticket.settle(TAKEN));
        assert!(!ticket.settle(GIVEN_UP), "too late to give it up");
        let ticket = Ticket::new(Request::Show);
        assert!(ticket.settle(GIVEN_UP));
        assert!(!ticket.settle(TAKEN), "too late to take it");
        assert!(!ticket.taken());

        // The window takes it just as the wait runs out, again and again.
        let name = unique("race");
        let timing = Timing { window: Duration::from_millis(100), patience: WAIT };
        let first = running(&name, timing);
        for _ in 0..5 {
            let later = later_start(&name, Request::Show, timing);
            std::thread::sleep(Duration::from_millis(95));
            let opened_here = first.next().is_some();
            let handed = later.join().unwrap().is_none();
            assert_eq!(opened_here, handed, "opened in exactly one window");
        }
    }

    /// A request too long or unreadable is refused at once, not waited on.
    #[test]
    fn a_request_it_cannot_read_is_refused_at_once() {
        let name = unique("bad");
        let _first = running(&name, QUICK);
        for sent in [vec![b'a'; MAX_REQUEST + 1], b"not a request\n".to_vec()] {
            let mut conn = connect_local(&name).unwrap();
            let started = Instant::now();
            conn.write_all(&sent).unwrap();
            let answer = Lines::default().next(&mut conn, 16, Instant::now() + WAIT);
            assert_eq!(answer, Ok("bad".to_string()));
            assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        }
    }

    /// The window let the name go while a start waited on it: that start
    /// takes its place, rather than waiting on a window that will not answer.
    #[test]
    fn a_start_waiting_on_a_window_that_closes_takes_its_place() {
        let name = unique("closing");
        let first = First::claim(&name, QUICK).unwrap();
        let later = later_start(&name, Request::Show, QUICK);
        std::thread::sleep(Duration::from_millis(300));
        drop(first);
        assert!(matches!(later.join().unwrap(), Some(Role::First(_))));
    }

    #[test]
    fn a_project_named_from_another_folder_keeps_its_place() {
        let Request::Open(path) = Request::for_project(Some("show.livecraft")) else { panic!() };
        let path = std::path::Path::new(&path);
        assert!(path.is_absolute());
        assert_eq!(path, std::env::current_dir().unwrap().join("show.livecraft"));
        assert_eq!(Request::for_project(None), Request::Show);
    }

    /// Something under our name that is not ours, here a program of this
    /// account with fewer rights, is never handed a project nor taken for a
    /// frozen window: this start runs as it always did.
    #[cfg(windows)]
    #[test]
    fn a_pipe_that_is_not_ours_is_left_alone() {
        let name = unique("lesser");
        let _lesser = crate::platform::bind_as_lesser(&name).unwrap();
        assert_eq!(hand_over(&name, &Request::Open("z.livecraft".into()), QUICK), Handed::Foreign);
        // Its only instance is taken from now on: still not ours, never waited on.
        let started = Instant::now();
        assert!(matches!(settle(&name, &Request::Show, QUICK), Some(Role::Alone { foreign: true })));
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    }

    /// A crash leaves the socket file behind: it must not pass for a window.
    #[cfg(unix)]
    #[test]
    fn a_socket_left_by_a_crash_is_taken_over() {
        let name = unique("crash");
        let path = crate::platform::socket_path(&name).unwrap();
        let _ = std::fs::remove_file(&path);
        // What a crash leaves: the lock gone with the process, the file still there.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert_eq!(hand_over(&name, &Request::Show, QUICK), Handed::NobodyThere);
        assert!(matches!(settle(&name, &Request::Show, QUICK), Some(Role::First(_))));
    }
}
