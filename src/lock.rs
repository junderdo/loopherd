//! Exclusive flocks that release the moment their guard drops.

use std::fs::File;
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// How long to wait for a lock held only for a read and rewrite.
pub const BRIEF: Duration = Duration::from_secs(30);

/// The longest pause between tries in [`Lock::wait`].
const MAX_POLL: Duration = Duration::from_millis(50);

/// An exclusive flock on a file, released when dropped.
#[derive(Debug)]
pub struct Lock(File);

impl Lock {
    /// Waits up to `bound` for `file`'s lock. Gives up with a `TimedOut`
    /// error naming the holder's PID where the system lists it, since any
    /// process that can open the file, read-only included, can hold it.
    pub fn wait(file: File, bound: Duration) -> io::Result<Lock> {
        let given_up = Instant::now() + bound;
        let mut pause = Duration::from_millis(1);
        loop {
            if try_flock(&file)? {
                return Ok(Lock(file));
            }
            let now = Instant::now();
            if now >= given_up {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "held past {}{}",
                        humantime::format_duration(bound),
                        held_by(&file)
                    ),
                ));
            }
            std::thread::sleep(pause.min(given_up - now));
            pause = (pause * 2).min(MAX_POLL);
        }
    }

    /// Locks `file`, or returns `None` at once if another holder has it.
    pub fn try_take(file: File) -> io::Result<Option<Lock>> {
        Ok(try_flock(&file)?.then_some(Lock(file)))
    }

    /// The locked file, still locked: whoever holds it, or a process it is
    /// passed to, keeps the lock until every copy closes.
    pub fn into_file(self) -> File {
        let lock = std::mem::ManuallyDrop::new(self);
        // SAFETY: `lock` is never dropped, so the file is read out exactly once.
        unsafe { std::ptr::read(&lock.0) }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Unlock before close: a child forked by another thread shares this open
        // file description until it execs, and closing alone leaves it locked.
        let _ = flock(&self.0, libc::LOCK_UN);
    }
}

fn try_flock(file: &File) -> io::Result<bool> {
    match flock(file, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "linux")]
fn held_by(file: &File) -> String {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = file.metadata() else {
        return "; holder unknown".to_string();
    };
    let dev = meta.dev();
    let id = format!(
        "{:02x}:{:02x}:{}",
        libc::major(dev),
        libc::minor(dev),
        meta.ino()
    );
    let listed = std::fs::read_to_string("/proc/locks").map(|locks| holders(&locks, &id));
    named(listed, meta.ino())
}

#[cfg(not(target_os = "linux"))]
fn held_by(_: &File) -> String {
    "; holder unknown".to_string()
}

/// The pids `/proc/locks` text lists holding a flock on `id`, `<major>:<minor>:<inode>`.
#[cfg(target_os = "linux")]
fn holders(locks: &str, id: &str) -> Vec<u32> {
    // A blocked waiter's line carries "->"; it doesn't hold the lock.
    locks
        .lines()
        .filter(|line| !line.contains("->"))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields[..] {
                [_, "FLOCK", _, _, pid, at, ..] if at == id => pid.parse().ok(),
                _ => None,
            }
        })
        .collect()
}

/// A holder can drop the lock, or hold it from another pid namespace, between
/// the failed try and the read of `/proc/locks`.
#[cfg(target_os = "linux")]
fn named(listed: io::Result<Vec<u32>>, inode: u64) -> String {
    match listed {
        Ok(pids) if pids.is_empty() => {
            format!("; holder not found in /proc/locks for inode {inode}")
        }
        Ok(pids) => {
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            format!(" by pid {}", pids.join(", "))
        }
        Err(e) => format!("; holder unknown, can't read /proc/locks: {e}"),
    }
}

fn flock(file: &File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: flock on a descriptor `file` keeps open for the call.
    match unsafe { libc::flock(file.as_raw_fd(), op) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

impl Deref for Lock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl DerefMut for Lock {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn open(path: &Path) -> File {
        File::create(path).unwrap()
    }

    #[test]
    fn a_held_lock_refuses_a_try_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = Lock::try_take(open(&path)).unwrap().unwrap();
        assert!(Lock::try_take(open(&path)).unwrap().is_none());
        drop(first);
        assert!(Lock::try_take(open(&path)).unwrap().is_some());
    }

    #[test]
    fn wait_blocks_until_the_holder_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = Lock::wait(open(&path), BRIEF).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || Lock::wait(open(&path), BRIEF).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished());
        drop(first);
        waiter.join().unwrap();
    }

    #[test]
    fn dropping_frees_it_while_a_spawning_thread_holds_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = Lock::wait(open(&path), BRIEF).unwrap();
        // A child forked by another thread holds this copy until it execs.
        let _inherited = first.try_clone().unwrap();
        drop(first);
        assert!(Lock::try_take(open(&path)).unwrap().is_some());
    }

    #[test]
    fn wait_gives_up_after_its_bound_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let _held = Lock::try_take(open(&path)).unwrap().unwrap();

        let started = std::time::Instant::now();
        let e = Lock::wait(open(&path), Duration::from_millis(100)).unwrap_err();

        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        let holder = format!("by pid {}", std::process::id());
        if cfg!(target_os = "linux") {
            assert!(e.to_string().contains(&holder), "{e}");
        }
        assert!(e.to_string().starts_with("held past 100ms"), "{e}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_holder_missing_from_proc_locks_is_said_with_the_inode() {
        let locks = "1: FLOCK  ADVISORY  WRITE 4242 08:20:77 0 EOF\n";

        let named = named(Ok(holders(locks, "08:20:99")), 99);

        assert_eq!(named, "; holder not found in /proc/locks for inode 99");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_holder_listed_in_proc_locks_is_named_and_a_waiter_isnt() {
        let locks = "1: FLOCK  ADVISORY  WRITE 4242 08:20:77 0 EOF\n\
                     1: -> FLOCK  ADVISORY  WRITE 5151 08:20:77 0 EOF\n";

        let named = named(Ok(holders(locks, "08:20:77")), 77);

        assert_eq!(named, " by pid 4242");
    }

    #[test]
    fn wait_on_a_free_lock_with_no_time_to_spare_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        assert!(Lock::wait(open(&path), Duration::ZERO).is_ok());
    }

    /// Lines outside a trailing `mod tests` block whose flock may block.
    /// `File::lock` and `lock_shared` are clippy's `disallowed_methods`.
    fn unbounded_flocks(text: &str) -> Vec<(usize, &str)> {
        let mut found = vec![];
        let mut depth = 0i32;
        for (n, line) in text.lines().enumerate() {
            if depth == 0 && line.trim_start().starts_with("mod tests {") {
                depth = 1;
                continue;
            }
            if depth > 0 {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                continue;
            }
            let flock = line.contains("LOCK_EX") || line.contains("LOCK_SH");
            if (flock && !line.contains("LOCK_NB")) || line.contains("lock_exclusive") {
                found.push((n + 1, line));
            }
        }
        found
    }

    #[test]
    fn the_check_reads_past_an_early_test_item_and_skips_only_the_tests() {
        let text = "#[cfg(test)]\nmod fakes;\n\nfn take() {\n    unsafe { libc::flock(0, libc::LOCK_EX) };\n}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        libc::LOCK_EX;\n    }\n}\n\nfn after() { libc::LOCK_SH; }\n";

        let found: Vec<usize> = unbounded_flocks(text).iter().map(|(n, _)| *n).collect();

        assert_eq!(found, [5, 15]);
    }

    /// Every flock in loopherd is non-blocking, so each wait is [`Lock::wait`]'s bounded poll.
    #[test]
    fn no_unbounded_lock_wait_remains_in_the_source() {
        let mut found = vec![];
        let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in unbounded_flocks(&text) {
                        found.push(format!("{}:{n}: {line}", path.display()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "unbounded flock:\n{}", found.join("\n"));
    }
}
