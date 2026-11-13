//! One LiveCraft per person. A later start, such as a project double-clicked
//! while the program runs, hands its project to the window already open and
//! ends, instead of running a second desk over the same session, ports and
//! outputs. The two talk through a channel only this account on this computer
//! can open (a named pipe on Windows, a socket in a private folder elsewhere):
//! no firewall is asked, and nobody else can open projects here.
//!
//! The window answers only once it has taken the request, so a later start
//! never waits long on a window that froze: it gives up and runs on its own,
//! without saving over the other one's session.

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The name real starts share; tests take their own.
pub const NAME: &str = "LiveCraft";

/// How long a window may take to pick up a request: long enough for one still
/// starting, short enough that a frozen one is not waited on for long.
pub const WINDOW_WAIT: Duration = Duration::from_secs(8);

/// How often a window that found the session taken asks for it again.
const RETRY_EVERY: Duration = Duration::from_secs(3);

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
    /// Later starts cannot reach this one here: it runs as it always did.
    Alone,
}

/// Becomes the first window, or hands `request` to the one already open.
/// None when it was handed over, and this start has nothing left to do.
pub fn settle(name: &str, request: &Request, wait: Duration) -> Option<Role> {
    // Twice: the first window may close in between.
    for _ in 0..2 {
        match First::claim(name, wait) {
            Ok(first) => return Some(Role::First(first)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
            Err(_) => return Some(Role::Alone),
        }
        // A little longer than the window is given, so that its answer arrives.
        match hand_over(name, request, wait + Duration::from_secs(2)) {
            Handed::Taken => return None,
            Handed::NobodyThere => {}
            Handed::NoAnswer => break,
        }
    }
    Some(Role::Second(Second::new(name, wait)))
}

#[derive(Debug, PartialEq)]
pub enum Handed {
    /// The window open took it.
    Taken,
    /// No LiveCraft runs.
    NobodyThere,
    /// One runs, but did not take it in time.
    NoAnswer,
}

/// Gives `request` to the window already open, waiting at most `wait`.
pub fn hand_over(name: &str, request: &Request, wait: Duration) -> Handed {
    let Ok(mut line) = serde_json::to_string(request) else { return Handed::NoAnswer };
    line.push('\n');
    let name = name.to_string();
    let (tx, rx) = mpsc::channel();
    // A process that froze can keep a read waiting for ever: the asking is
    // done apart, and left behind when it takes too long.
    std::thread::spawn(move || {
        let _ = tx.send(ask(&name, &line));
    });
    rx.recv_timeout(wait).unwrap_or(Handed::NoAnswer)
}

fn ask(name: &str, line: &str) -> Handed {
    let mut conn = match crate::platform::connect_local(name) {
        Ok(conn) => conn,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
            return Handed::NobodyThere;
        }
        Err(_) => return Handed::NoAnswer,
    };
    let mut answer = String::new();
    let asked =
        conn.write_all(line.as_bytes()).and_then(|()| BufReader::new(&mut conn).read_line(&mut answer));
    if asked.is_ok() && answer == "ok\n" { Handed::Taken } else { Handed::NoAnswer }
}

type Waker = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

struct Delivery {
    request: Request,
    taken: Sender<()>,
}

/// The first window's side: requests from later starts, waiting to be taken.
pub struct First {
    requests: Receiver<Delivery>,
    waker: Waker,
    stop: Arc<AtomicBool>,
    name: String,
    accepting: Option<std::thread::JoinHandle<()>>,
}

impl First {
    /// Fails with `AddrInUse` while another LiveCraft of this account runs.
    pub fn claim(name: &str, wait: Duration) -> io::Result<Self> {
        let mut listener = crate::platform::LocalListener::bind(name)?;
        let (tx, requests) = mpsc::channel();
        let waker = Waker::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (w, s) = (waker.clone(), stop.clone());
        let accepting = std::thread::Builder::new().name("later starts".into()).spawn(move || {
            loop {
                let caller = listener.accept();
                if s.load(Ordering::Acquire) {
                    break;
                }
                match caller {
                    Ok(conn) => {
                        let (tx, w) = (tx.clone(), w.clone());
                        // Each caller apart: one that never speaks holds up no other.
                        std::thread::spawn(move || serve(conn, &tx, &w, wait));
                    }
                    // A caller that left at once, or no handles to spare: never spin.
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
        Ok(Self { requests, waker, stop, name: name.to_string(), accepting: Some(accepting) })
    }

    /// Called as each request arrives, so that a window asleep takes it at once.
    pub fn wake_with(&self, wake: impl Fn() + Send + 'static) {
        *self.waker.lock().unwrap() = Some(Box::new(wake));
    }

    /// The next request handed over. One its start stopped waiting for is
    /// dropped: that start runs on its own by now.
    pub fn next(&self) -> Option<Request> {
        while let Ok(d) = self.requests.try_recv() {
            if d.taken.send(()).is_ok() {
                return Some(d.request);
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
        if let Ok(_wake) = crate::platform::connect_local(&self.name)
            && let Some(accepting) = self.accepting.take()
        {
            let _ = accepting.join();
        }
    }
}

/// Reads one request, hands it to the window, and answers once it was taken.
fn serve(mut conn: crate::platform::LocalStream, requests: &Sender<Delivery>, waker: &Waker, wait: Duration) {
    let mut line = String::new();
    // A path is short: anything longer is no request of ours.
    if BufReader::new((&mut conn).take(64 * 1024)).read_line(&mut line).is_err() {
        return;
    }
    let Ok(request) = serde_json::from_str(&line) else { return };
    let (taken, was_taken) = mpsc::channel();
    if requests.send(Delivery { request, taken }).is_err() {
        return;
    }
    if let Some(wake) = waker.lock().unwrap().as_ref() {
        wake();
    }
    if was_taken.recv_timeout(wait).is_ok() && conn.write_all(b"ok\n").is_ok() {
        // Hanging up at once could lose the answer: the asker hangs up first.
        let _ = conn.read(&mut [0; 1]);
    }
}

/// A window that found the session taken. It asks again now and then, and
/// becomes the first once the other one is gone.
pub struct Second {
    name: String,
    wait: Duration,
    next_try: Instant,
}

impl Second {
    fn new(name: &str, wait: Duration) -> Self {
        Self { name: name.to_string(), wait, next_try: Instant::now() + RETRY_EVERY }
    }

    pub fn try_first(&mut self) -> Option<First> {
        if Instant::now() < self.next_try {
            return None;
        }
        self.next_try = Instant::now() + RETRY_EVERY;
        First::claim(&self.name, self.wait).ok()
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

    fn later_start(name: &str, request: Request) -> std::thread::JoinHandle<bool> {
        let name = name.to_string();
        std::thread::spawn(move || settle(&name, &request, WAIT).is_none())
    }

    #[test]
    fn a_later_start_hands_its_project_to_the_first_and_ends() {
        let name = unique("open");
        let Some(Role::First(first)) = settle(&name, &Request::Show, WAIT) else {
            panic!("the first start keeps the name");
        };
        let woken = Arc::new(AtomicBool::new(false));
        let w = woken.clone();
        first.wake_with(move || w.store(true, Ordering::Release));
        let path = "C:/émissions/le direct\n2.livecraft".to_string();
        let later = later_start(&name, Request::Open(path.clone()));
        assert_eq!(next_request(&first), Request::Open(path), "the whole path, whatever it holds");
        assert!(later.join().unwrap(), "the later start ends once the window took it");
        assert!(woken.load(Ordering::Acquire), "a window asleep is woken");

        let later = later_start(&name, Request::Show);
        assert_eq!(next_request(&first), Request::Show, "a start without a project brings it forward");
        assert!(later.join().unwrap());
    }

    #[test]
    fn with_nobody_there_a_start_becomes_the_first_at_once() {
        let name = unique("none");
        let started = Instant::now();
        assert_eq!(hand_over(&name, &Request::Show, WAIT), Handed::NobodyThere);
        assert!(started.elapsed() < Duration::from_secs(2), "no waiting when nobody runs");
        assert!(matches!(settle(&name, &Request::Show, WAIT), Some(Role::First(_))));
    }

    #[test]
    fn the_name_goes_free_when_the_first_window_closes() {
        let name = unique("free");
        let first = First::claim(&name, WAIT).unwrap();
        let again = First::claim(&name, WAIT).err().expect("held while the first runs");
        assert_eq!(again.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        assert!(First::claim(&name, WAIT).is_ok(), "free again once it closed");
    }

    /// A window that does not take requests, frozen or busy, is not waited
    /// on: the later start runs on its own, without the session, until that
    /// window is gone.
    #[test]
    fn a_window_that_does_not_answer_is_not_waited_on() {
        let name = unique("busy");
        let short = Duration::from_millis(300);
        let first = First::claim(&name, short).unwrap();
        let started = Instant::now();
        let Some(Role::Second(mut second)) = settle(&name, &Request::Open("x.livecraft".into()), short)
        else {
            panic!("runs on its own");
        };
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(first.next(), None, "a request given up on is not opened later");
        second.ask_now();
        assert!(second.try_first().is_none(), "the session stays with the first while it runs");
        drop(first);
        assert!(second.try_first().is_none(), "asked again only now and then");
        second.ask_now();
        assert!(second.try_first().is_some(), "and comes to this window once the first is gone");
    }

    /// A process stuck whole, its listening thread too, answers nothing at all.
    #[test]
    fn a_process_that_froze_is_not_waited_on() {
        let name = unique("stuck");
        let _listening_but_stuck = crate::platform::LocalListener::bind(&name).unwrap();
        let started = Instant::now();
        assert_eq!(hand_over(&name, &Request::Show, Duration::from_millis(500)), Handed::NoAnswer);
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    #[test]
    fn a_project_named_from_another_folder_keeps_its_place() {
        let Request::Open(path) = Request::for_project(Some("show.livecraft")) else { panic!() };
        let path = std::path::Path::new(&path);
        assert!(path.is_absolute());
        assert_eq!(path, std::env::current_dir().unwrap().join("show.livecraft"));
        assert_eq!(Request::for_project(None), Request::Show);
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
        assert_eq!(hand_over(&name, &Request::Show, WAIT), Handed::NobodyThere);
        assert!(matches!(settle(&name, &Request::Show, WAIT), Some(Role::First(_))));
    }
}
