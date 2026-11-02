//! Exclusive flocks that release the moment their guard drops.

use std::fs::File;
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;

/// An exclusive flock on a file, released when dropped.
#[derive(Debug)]
pub struct Lock(File);

impl Lock {
    /// Waits until `file` is locked.
    pub fn wait(file: File) -> io::Result<Lock> {
        flock(&file, libc::LOCK_EX)?;
        Ok(Lock(file))
    }

    /// Locks `file`, or returns `None` at once if another holder has it.
    pub fn try_take(file: File) -> io::Result<Option<Lock>> {
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Lock(file))),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Unlock before close: a child forked by another thread shares this open
        // file description until it execs, and closing alone leaves it locked.
        let _ = flock(&self.0, libc::LOCK_UN);
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
        let first = Lock::wait(open(&path)).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || Lock::wait(open(&path)).unwrap())
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
        let first = Lock::wait(open(&path)).unwrap();
        // A child forked by another thread holds this copy until it execs.
        let _inherited = first.try_clone().unwrap();
        drop(first);
        assert!(Lock::try_take(open(&path)).unwrap().is_some());
    }
}
