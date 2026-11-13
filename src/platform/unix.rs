//! What Linux and macOS share: process groups, a socket for later starts, and
//! secrets sealed with a key kept in the system keyring.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::LazyLock;

/// Each child leads a process group of its own, which everything it starts
/// joins: the whole tree can then be ended at once.
pub fn prepare_child(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Whether the child has ended, without reaping it: while it stays a zombie
/// its number cannot be given to another program, so a group kill aimed at it
/// is still safe. `None` when it was reaped already, and the number is free.
fn exit_state(child: &Child) -> Option<bool> {
    let pid = libc::id_t::try_from(child.id()).ok()?;
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid only reads its arguments and writes `info`; WNOWAIT leaves the child to be reaped later.
    let r =
        unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    if r != 0 {
        return None;
    }
    // SAFETY: waitid filled `info`; the process number is zero while the child still runs.
    #[cfg(target_os = "linux")]
    let who = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let who = info.si_pid;
    Some(who != 0)
}

/// Whether the child has ended. It is not reaped, so `end_process_tree` can
/// still end what it left behind before `wait` frees its number.
pub fn has_exited(child: &mut Child) -> bool {
    !matches!(exit_state(child), Some(false))
}

/// A child can have children of its own: ffmpeg is often started through a
/// launcher, and a browser is dozens of processes. Stopping only the first
/// would leave the real work running. A child that was reaped already is left
/// alone: its number may belong to someone else by now.
pub fn end_process_tree(child: &mut Child) {
    if exit_state(child).is_none() {
        return;
    }
    // Only children started with `prepare_child` lead a group; for any other
    // there is no group of that number and nothing happens.
    if let Ok(group) = i32::try_from(child.id()) {
        // SAFETY: kill only sends a signal; a negative number names the group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Nothing to do here: ffmpeg stops by itself when the pipe it reads from or
/// writes to closes, which happens however this program ends. The browser has
/// no such pipe and is held by a `ProcessTree` instead.
pub fn tie_children_to_this_process() {}

/// A browser runs as dozens of processes, and nothing tells it that this
/// program has gone. A small shell waits on a pipe only this program holds:
/// when the pipe closes, on drop or because this program died, it ends the
/// browser's whole group.
pub struct ProcessTree {
    guard: Child,
    line: Option<ChildStdin>,
}

impl ProcessTree {
    pub fn hold(child: &Child) -> Option<Self> {
        // Interrupts aimed at the terminal must not take the guard first.
        let script = "trap '' INT HUP TERM; while read -r _; do :; done; kill -KILL -\"$1\" 2>/dev/null";
        let mut guard = Command::new("/bin/sh")
            .args(["-c", script, "livecraft-guard", &child.id().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .ok()?;
        let line = guard.stdin.take();
        Some(Self { guard, line })
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        drop(self.line.take());
        let _ = self.guard.wait();
    }
}

/// A folder only this account can enter, for the socket later starts call:
/// in XDG_RUNTIME_DIR, which is this account's own, else in the settings folder.
fn private_dir() -> std::io::Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute() && d.is_dir());
    if let Some(dir) = runtime.and_then(|run| own_private_dir(&run.join("livecraft")).ok()) {
        return Ok(dir);
    }
    let settings = crate::platform::config_dir().join("LiveCraft");
    std::fs::create_dir_all(&settings)?;
    own_private_dir(&settings.join("instance"))
}

/// Makes the folder, or takes the one there only if it is a real folder (not
/// a link someone left) of this account; then closes it to everyone else.
fn own_private_dir(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir() || meta.uid() != me {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not this account's own folder",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir.to_path_buf())
}

/// The socket later starts call under this name; its lock sits beside it.
pub fn socket_path(name: &str) -> std::io::Result<PathBuf> {
    Ok(private_dir()?.join(format!("{name}.sock")))
}

/// Where the LiveCraft already running for this account listens for later
/// starts: a socket in a private folder, beside a lock its holder keeps, so a
/// socket left behind by a crash is told apart from a live one.
pub struct LocalListener {
    socket: std::os::unix::net::UnixListener,
    path: PathBuf,
    _lock: std::fs::File,
}

pub type LocalStream = std::os::unix::net::UnixStream;

impl LocalListener {
    /// Fails with `AddrInUse` while another LiveCraft of this account holds the name.
    pub fn bind(name: &str) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let path = socket_path(name)?;
        // Only its lock matters, never what it holds.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        // SAFETY: flock only reads the descriptor, which `lock` keeps open.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                std::io::Error::new(std::io::ErrorKind::AddrInUse, e)
            } else {
                e
            });
        }
        // Only a run that ended without cleaning up leaves one: the lock is ours.
        let _ = std::fs::remove_file(&path);
        let socket = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { socket, path, _lock: lock })
    }

    pub fn accept(&mut self) -> std::io::Result<LocalStream> {
        self.socket.accept().map(|(stream, _)| stream)
    }
}

impl Drop for LocalListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Opens the socket of the LiveCraft already running for this account:
/// `NotFound`, or `ConnectionRefused` after a crash, when there is none. A
/// caller waits its turn in the socket's queue, so `until` is not needed here.
pub fn connect_local(name: &str, _until: std::time::Instant) -> std::io::Result<LocalStream> {
    LocalStream::connect(socket_path(name)?)
}

/// Bringing a window forward from outside its own loop is for the window
/// system to refuse here: the window does it itself when it takes a request.
pub fn raise_window(_window: Option<isize>) {}

/// Reads what has come, waiting until `until` at most: `TimedOut` then, and
/// 0 once the other side hung up.
pub fn read_some(
    stream: &mut LocalStream,
    buf: &mut [u8],
    until: std::time::Instant,
) -> std::io::Result<usize> {
    use std::io::Read;
    let left = until.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    stream.set_read_timeout(Some(left))?;
    match stream.read(buf) {
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            Err(std::io::ErrorKind::TimedOut.into())
        }
        read => read,
    }
}

/// Opens a web page in the person's own browser.
pub fn open_in_browser(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let started = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Waited for apart, so that it does not linger once done.
    if let Ok(mut child) = started {
        std::thread::spawn(move || child.wait());
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// The first of these programs found on the PATH.
pub fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    names
        .iter()
        .find_map(|name| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| is_program(p)))
}

pub fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Where a keyring keeps one key, with its own way of saying it is not
/// reachable (`Err`) or holds nothing yet (`Ok(None)`).
pub struct Keystore {
    pub load: fn() -> Result<Option<Vec<u8>>, ()>,
    pub save: fn(&[u8]) -> bool,
}

type SealKey = [u8; 32];
const NONCE: usize = 12;

/// The keys that may open a secret, the one sealing new ones first. A key
/// lives in the system keyring; without one (a server, a bare window
/// manager) it lives in a file only this account can read, so a copy of the
/// session file alone still reveals nothing. A key from the other place is
/// kept too, so secrets sealed before the keyring came or went still open.
fn gather_keys(store: &Keystore, file: &Path) -> Vec<SealKey> {
    let from = |bytes: Vec<u8>| SealKey::try_from(bytes.as_slice()).ok();
    let mut stored = (store.load)();
    if let Ok(None) = stored {
        let fresh = new_key();
        stored = Ok(fresh.filter(|k| (store.save)(k)).map(Vec::from));
    }
    let stored = stored.ok().flatten().and_then(from);
    let mut filed = read_key_file(file);
    if stored.is_none() && filed.is_none() {
        filed = new_key().and_then(|k| install_key_file(file, &k));
    }
    stored.into_iter().chain(filed).collect()
}

fn new_key() -> Option<SealKey> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).ok()?;
    Some(key)
}

/// The key in the file, if the file is what this program wrote: a regular
/// file (never a link followed), owned by this account, holding exactly one
/// key. One this account owns but left open to others is closed again; any
/// other is ignored, and the key in it is never trusted.
fn read_key_file(file: &Path) -> Option<SealKey> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(file).ok()?;
    let meta = f.metadata().ok()?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.len() != 32 {
        return None;
    }
    if meta.mode() & 0o077 != 0 {
        f.set_permissions(std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    let mut key = [0u8; 32];
    f.read_exact(&mut key).ok()?;
    Some(key)
}

/// Puts a key in place and returns the one that is there now. It is written
/// whole to a file of its own and then linked in, so no reader ever sees half
/// a key; if another instance got there first its key is kept, and a file that
/// is not a valid key (cut short, empty, a link) is replaced.
fn install_key_file(file: &Path, key: &SealKey) -> Option<SealKey> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = file.parent()?;
    let _ = std::fs::create_dir_all(dir);
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).ok()?;
    let side = dir.join(format!(".secret.key.{}.{}", std::process::id(), u64::from_le_bytes(random)));
    // Created readable by this account only, never widened afterwards.
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&side).ok()?;
    let written = f.write_all(key).and_then(|_| f.sync_all());
    drop(f);
    let placed = if written.is_err() {
        None
    } else {
        match std::fs::hard_link(&side, file) {
            Ok(()) => Some(*key),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                read_key_file(file).or_else(|| std::fs::rename(&side, file).ok().map(|_| *key))
            }
            Err(_) => None,
        }
    };
    let _ = std::fs::remove_file(&side);
    placed
}

/// Read or made once, on first use: the keyring can be slow to answer.
static KEYS: LazyLock<Vec<SealKey>> = LazyLock::new(|| {
    gather_keys(&super::os::KEYSTORE, &super::os::config_dir().join("LiveCraft").join("secret.key"))
});

pub fn seal(plain: &[u8]) -> Option<Vec<u8>> {
    seal_by(KEYS.first()?, plain)
}

pub fn open_sealed(sealed: &[u8]) -> Option<Vec<u8>> {
    KEYS.iter().find_map(|k| open_by(k, sealed))
}

fn seal_by(key: &SealKey, plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).ok()?;
    let sealed =
        ChaCha20Poly1305::new(Key::from_slice(key)).encrypt(Nonce::from_slice(&nonce), plain).ok()?;
    Some([nonce.as_slice(), &sealed].concat())
}

fn open_by(key: &SealKey, sealed: &[u8]) -> Option<Vec<u8>> {
    let (nonce, body) = sealed.split_at_checked(NONCE)?;
    ChaCha20Poly1305::new(Key::from_slice(key)).decrypt(Nonce::from_slice(nonce), body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("livecraft-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("secret.key")
    }

    fn unreachable_store() -> Keystore {
        Keystore { load: || Err(()), save: |_| false }
    }

    #[test]
    fn without_a_keyring_the_key_file_is_private_and_lasts() {
        use std::os::unix::fs::PermissionsExt;
        let file = temp_file("nokeyring");
        let first = gather_keys(&unreachable_store(), &file);
        assert_eq!(first.len(), 1);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only this account reads the key");
        assert_eq!(gather_keys(&unreachable_store(), &file), first, "the next run finds the same key");
        let sealed = seal_by(&first[0], b"rtmp-key").unwrap();
        assert!(!sealed.windows(8).any(|w| w == b"rtmp-key"));
        assert_eq!(open_by(&first[0], &sealed).unwrap(), b"rtmp-key");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_damaged_key_file_is_replaced_not_left_to_disable_sealing() {
        use std::os::unix::fs::PermissionsExt;
        for (name, bytes) in [("empty", &b""[..]), ("short", &b"half a key"[..]), ("long", &[1u8; 40][..])] {
            let file = temp_file(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, bytes).unwrap();
            let keys = gather_keys(&unreachable_store(), &file);
            assert_eq!(keys.len(), 1, "{name}: a key is made again");
            assert_eq!(std::fs::read(&file).unwrap(), keys[0], "{name}: and kept");
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(gather_keys(&unreachable_store(), &file), keys, "{name}: the same one next time");
            let left: Vec<_> = std::fs::read_dir(file.parent().unwrap()).unwrap().collect();
            assert_eq!(left.len(), 1, "{name}: no side file is left behind");
            let _ = std::fs::remove_dir_all(file.parent().unwrap());
        }
    }

    #[test]
    fn a_key_file_that_is_a_link_is_not_followed() {
        let file = temp_file("link");
        let dir = file.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("elsewhere");
        std::fs::write(&target, [9u8; 32]).unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        let keys = gather_keys(&unreachable_store(), &file);
        assert_eq!(keys.len(), 1);
        assert_ne!(keys[0], [9u8; 32], "the planted key is not used");
        assert_eq!(std::fs::read(&target).unwrap(), [9u8; 32], "and its target is untouched");
        assert!(!std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_file_open_to_others_is_closed_again() {
        use std::os::unix::fs::PermissionsExt;
        let file = temp_file("wide");
        let key = gather_keys(&unreachable_store(), &file)[0];
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(gather_keys(&unreachable_store(), &file), [key]);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn two_instances_starting_together_end_up_with_one_key() {
        let file = temp_file("race");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let file = file.clone();
                std::thread::spawn(move || gather_keys(&unreachable_store(), &file))
            })
            .collect();
        let all: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(all.iter().all(|k| k.len() == 1 && *k == all[0]), "{all:?}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_child_already_reaped_is_not_signalled_again() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        prepare_child(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let t0 = Instant::now();
        while !has_exited(&mut child) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(has_exited(&mut child));
        assert!(exit_state(&child).is_some(), "still a zombie: not reaped by looking");
        child.wait().unwrap();
        assert!(exit_state(&child).is_none(), "reaped: its number is free");
        end_process_tree(&mut child); // must do nothing, not signal a stranger
    }

    #[test]
    fn a_keyring_key_seals_and_an_older_file_key_still_opens() {
        let file = temp_file("both");
        let old = gather_keys(&unreachable_store(), &file)[0];
        let store = Keystore { load: || Ok(Some(vec![7; 32])), save: |_| true };
        let keys = gather_keys(&store, &file);
        assert_eq!(keys, [[7; 32], old]);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_wrong_key_or_damaged_text_opens_nothing() {
        let sealed = seal_by(&[1; 32], b"secret").unwrap();
        assert!(open_by(&[2; 32], &sealed).is_none());
        let mut damaged = sealed.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(open_by(&[1; 32], &damaged).is_none());
        assert!(open_by(&[1; 32], &sealed[..5]).is_none());
    }

    fn gone(pid: i32) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A child that starts a grandchild, then says the grandchild's number.
    fn family() -> (Child, i32) {
        use std::io::{BufRead, BufReader};
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"]).stdout(Stdio::piped());
        prepare_child(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        (child, line.trim().parse().unwrap())
    }

    #[test]
    fn ending_a_tree_ends_the_grandchildren_too() {
        let (mut child, grandchild) = family();
        end_process_tree(&mut child);
        let _ = child.wait();
        assert!(gone(grandchild), "the grandchild kept running");
    }

    #[test]
    fn letting_go_of_a_held_tree_ends_it() {
        let (mut child, grandchild) = family();
        let tree = ProcessTree::hold(&child).unwrap();
        // SAFETY: signal 0 only asks whether the process exists.
        assert_eq!(unsafe { libc::kill(grandchild, 0) }, 0, "held trees keep running");
        drop(tree);
        let _ = child.wait();
        assert!(gone(grandchild), "the grandchild kept running");
    }

    #[test]
    fn the_folder_for_later_starts_is_never_a_link_nor_open_to_others() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let base = std::env::temp_dir().join(format!("livecraft-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let made = own_private_dir(&base.join("fresh")).unwrap();
        assert_eq!(mode(&made), 0o700);
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        own_private_dir(&open).unwrap();
        assert_eq!(mode(&open), 0o700, "closed to others");
        let link = base.join("link");
        std::os::unix::fs::symlink(&made, &link).unwrap();
        assert!(own_private_dir(&link).is_err(), "a link is never followed");
        let _ = std::fs::remove_dir_all(&base);
    }
}
