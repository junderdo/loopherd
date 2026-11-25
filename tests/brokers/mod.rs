use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A lead's broker, with the launch record `loopherd lead` keeps; killed on drop.
pub struct Broker(Child);

impl Broker {
    /// Runs `loopherd`, which carries the test's environment, as lead
    /// `lead`'s broker for herd `herd`, and waits until it serves.
    pub fn start(mut loopherd: Command, config: &Path, lead: &str, herd: &str) -> Broker {
        let sandbox = config.join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        fs::write(
            sandbox.join(format!("{lead}.lead.json")),
            format!(r#"{{"herd": "{herd}", "args": []}}"#),
        )
        .unwrap();
        // A broker reads its herd file at start, for whether it guards merges by credentials.
        let herd_file = config.join(format!("herds/{herd}.toml"));
        if !herd_file.exists() {
            fs::create_dir_all(herd_file.parent().unwrap()).unwrap();
            fs::write(&herd_file, "repo = \"/\"\n").unwrap();
        }
        let child = loopherd
            .args(["broker", lead])
            .env_remove("HERDR_PANE_ID")
            .env_remove("LOOPHERD_BROKER_OF")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lock = config.join(format!("broker/{lead}.lock"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !File::open(&lock).is_ok_and(|file| held(&file)) {
            assert!(Instant::now() < deadline, "the broker never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        Broker(child)
    }
}

impl Broker {
    #[allow(dead_code)] // Not every test file that starts a broker reads its pid.
    pub fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether it exits within `within`.
    #[allow(dead_code)] // Not every test file that starts a broker waits on it.
    pub fn wait_exited(&mut self, within: Duration) -> bool {
        let given_up = Instant::now() + within;
        while self.0.try_wait().unwrap().is_none() {
            if Instant::now() >= given_up {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether another process holds `file`'s lock.
pub fn held(file: &File) -> bool {
    // SAFETY: flock on a descriptor `file` keeps open for the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
}
