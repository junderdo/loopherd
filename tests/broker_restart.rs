//! `loopherd broker-restart`, the shepherd's way to put a lead's broker on
//! the current binary, refused from the lead's own pane; and brokers that
//! move onto a reinstalled loopherd by themselves.

mod brokers;
mod fakes;

use brokers::{Broker, held};
use fakes::executable;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/herdr-test.sock";

/// lead-k leads herd k from pane w1:p1, with worker lh-1 in w1:p2, and a stub
/// herdr whose `agent read` prints `served`, once no `hold` file sits in the root. Dropping it retires lead-k, which
/// stops any broker a restart left running, and waits until it has.
struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state/loopherd");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("ledger.json"), ledger(false)).unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let agent = |name: &str, pane: &str| {
            format!(
                r#"{{"name": "{name}", "pane_id": "{pane}", "tab_id": "w1:t1", "agent_status": "idle"}}"#
            )
        };
        executable(
            &bin.join("herdr"),
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n\
                 'agent list') echo '{{\"result\": {{\"agents\": [{}, {}]}}}}' ;;\n\
                 'agent read') while [ -e '{}' ]; do sleep 0.05; done; echo served ;;\nesac\n",
                agent("lead-k", "w1:p1"),
                agent("lh-1", "w1:p2"),
                root.path().join("hold").display()
            ),
        );
        World { root }
    }

    fn broker(&self) -> Broker {
        Broker::start(self.loopherd(None, &[]), &self.config(), "lead-k", "k")
    }

    fn run(&self, pane: Option<&str>, args: &[&str]) -> Output {
        self.loopherd(pane, args).output().unwrap()
    }

    fn loopherd(&self, pane: Option<&str>, args: &[&str]) -> Command {
        self.loopherd_at(Path::new(env!("CARGO_BIN_EXE_loopherd")), pane, args)
    }

    fn loopherd_at(&self, program: &Path, pane: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()),
            )
            .env("HERDR_SOCKET_PATH", SOCKET)
            .env_remove("CLAUDE_PID");
        match pane {
            Some(pane) => cmd.env("HERDR_PANE_ID", pane),
            None => cmd.env_remove("HERDR_PANE_ID"),
        };
        cmd.args(args);
        cmd
    }

    fn hold(&self) -> std::path::PathBuf {
        self.root.path().join("hold")
    }

    fn config(&self) -> std::path::PathBuf {
        self.root.path().join("config")
    }

    fn serving(&self) -> bool {
        File::open(self.config().join("broker/lead-k.lock")).is_ok_and(|file| held(&file))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let ledger = self.root.path().join("state/loopherd/ledger.json");
        let _ = fs::write(ledger, self::ledger(true));
        let given_up = Instant::now() + Duration::from_secs(5);
        while self.serving() && Instant::now() < given_up {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn ledger(retired: bool) -> String {
    format!(
        r#"{{"lead-k": {{"name": "lead-k", "pane_id": "w1:p1", "session_id": null,
        "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
        "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "socket": "{SOCKET}", "herd": "k", "role": "lead", "retired": {retired}}},
        "lh-1": {{"name": "lh-1", "pane_id": "w1:p2", "session_id": null,
        "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
        "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "socket": "{SOCKET}", "herd": "k", "role": null}}}}"#
    )
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into()
}

/// The pid after `pid ` in `text`, ahead of `before`.
fn pid_in(text: &str, before: &str) -> u32 {
    let head = &text[..text.find(before).unwrap()];
    head.rsplit("pid ").next().unwrap().trim().parse().unwrap()
}

fn exited(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_with_timeout(mut child: Child, within: Duration) -> Output {
    let given_up = Instant::now() + within;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= given_up {
            let _ = child.kill();
            panic!(
                "still running after {within:?}: {:?}",
                child.wait_with_output()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// `bwrap` with the filesystem read-only and a pid namespace of its own, as a
/// lead's sandbox; `None` where bwrap can't make one.
fn sandboxed() -> Option<Command> {
    let bwrap = || {
        let mut bwrap = Command::new("bwrap");
        bwrap.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
        ]);
        bwrap
    };
    bwrap()
        .arg("true")
        .status()
        .is_ok_and(|s| s.success())
        .then(bwrap)
}

fn inside(mut sandbox: Command, loopherd: &Command) -> Command {
    sandbox
        .arg("--")
        .arg(loopherd.get_program())
        .args(loopherd.get_args())
        .envs(loopherd.get_envs().filter_map(|(k, v)| Some((k, v?))))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sandbox
}

#[test]
fn broker_restart_replaces_a_running_broker_with_one_from_this_binary() {
    let world = World::new();
    let mut old = world.broker();
    let old_pid = old.pid();

    let restarted = stdout(&world.run(None, &["broker-restart", "lead-k"]));

    let new_pid = pid_in(&restarted, " serves");
    assert_eq!(
        restarted,
        format!(
            "asked lead-k's broker (pid {old_pid}) to stop once its request in flight is answered\n\
             restarted lead-k's broker: pid {old_pid} stopped, pid {new_pid} serves\n"
        )
    );
    assert!(old.wait_exited(Duration::from_secs(5)));
    assert_ne!(new_pid, old_pid);
    assert!(!exited(new_pid));
    assert!(world.serving());
}

#[test]
fn a_reinstall_while_broker_restart_waits_still_leaves_a_broker_serving() {
    let world = World::new();
    let (started, release) = (
        world.root.path().join("started"),
        world.root.path().join("release"),
    );
    let herdr = world.root.path().join("bin/herdr");
    let stub = fs::read_to_string(&herdr).unwrap();
    let gated = format!(
        "touch {}; until [ -e {} ]; do sleep 0.02; done; echo served",
        started.display(),
        release.display()
    );
    executable(&herdr, stub.replace("echo served", &gated));
    let installed = world.root.path().join("install/loopherd");
    let reinstall = || {
        let part = installed.with_extension("part");
        fs::copy(env!("CARGO_BIN_EXE_loopherd"), &part).unwrap();
        fs::rename(&part, &installed).unwrap();
    };
    fs::create_dir(installed.parent().unwrap()).unwrap();
    reinstall();
    let _old = world.broker();
    let in_flight = world
        .loopherd(Some("w1:p1"), &["read", "lh-1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let given_up = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(Instant::now() < given_up, "the read never reached herdr");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut restart = world
        .loopherd_at(&installed, None, &["broker-restart", "lead-k"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(restart.stdout.take().unwrap());
    let mut asked = String::new();
    out.read_line(&mut asked).unwrap();
    assert!(asked.starts_with("asked lead-k's broker"), "{asked}");
    reinstall();
    fs::write(&release, "").unwrap();

    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    let status = restart.wait_with_output().unwrap();
    assert!(status.status.success(), "{rest}{status:?}");
    assert!(rest.contains(" serves\n"), "{rest}");
    assert!(world.serving());
    assert_eq!(stdout(&in_flight.wait_with_output().unwrap()), "served\n");
}

#[test]
fn broker_restart_with_no_broker_running_starts_one() {
    let world = World::new();
    drop(world.broker());

    let started = stdout(&world.run(None, &["broker-restart", "lead-k"]));

    let pid = pid_in(&started, "\n");
    assert_eq!(started, format!("started lead-k's broker: pid {pid}\n"));
    assert!(world.serving());
}

#[test]
fn broker_restart_from_a_leads_pane_is_refused_and_leaves_its_broker_alone() {
    let world = World::new();
    let mut broker = world.broker();

    let refused = world.run(Some("w1:p1"), &["broker-restart", "lead-k"]);

    assert!(!refused.status.success());
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "loopherd broker-restart: refused: broker-restart is the shepherd's or the human's, \
         and a lead runs this loopherd\n"
    );
    assert!(!broker.wait_exited(Duration::from_millis(300)));
    assert!(world.serving());
}

#[test]
fn broker_restart_run_by_a_brokers_own_request_is_refused() {
    let world = World::new();
    let mut broker = world.broker();

    let refused = world
        .loopherd(None, &["broker-restart", "lead-k"])
        .env("LOOPHERD_BROKER_OF", "lead-k")
        .output()
        .unwrap();

    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("refused: broker-restart is the shepherd's"),
        "{refused:?}"
    );
    assert!(!broker.wait_exited(Duration::from_millis(300)));
}

#[test]
fn a_sandboxed_leads_request_waits_out_a_restarts_gap_though_it_cant_see_the_restarts_pid() {
    let Some(mut sandbox) = sandboxed() else {
        eprintln!("skipped: bwrap can't make a pid namespace here");
        return;
    };
    let world = World::new();
    let brokers = world.config().join("broker");
    fs::create_dir_all(&brokers).unwrap();
    fs::create_dir_all(
        world
            .root
            .path()
            .join("state/loopherd/broker/lead-k/requests"),
    )
    .unwrap();
    // A restart in its gap: holding its lock, the old broker gone, no new one yet.
    let restart = File::create(brokers.join("lead-k.restart")).unwrap();
    // SAFETY: flock on a descriptor `restart` keeps open.
    assert_eq!(
        unsafe { libc::flock(restart.as_raw_fd(), libc::LOCK_EX) },
        0
    );
    fs::write(brokers.join("lead-k.stop"), "999999").unwrap();
    let root = world.root.path().to_str().unwrap();
    sandbox.args(["--bind", root, root]);
    let lead = world.loopherd(Some("w1:p1"), &["read", "lh-1"]);
    let mut read = inside(sandbox, &lead).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));

    assert!(
        read.try_wait().unwrap().is_none(),
        "{:?}",
        read.wait_with_output()
    );
    let _broker = world.broker();
    fs::remove_file(brokers.join("lead-k.stop")).unwrap();
    drop(restart);
    assert_eq!(stdout(&read.wait_with_output().unwrap()), "served\n");
}

#[test]
fn broker_restart_waits_for_the_old_brokers_pid_while_another_broker_takes_its_lock() {
    let world = World::new();
    fs::write(world.hold(), "").unwrap();
    let mut old = world.broker();
    let old_pid = old.pid();
    let in_flight = world
        .loopherd(Some("w1:p1"), &["read", "lh-1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let restart = world
        .loopherd(None, &["broker-restart", "lead-k"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stop = world.config().join("broker/lead-k.stop");
    while !stop.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }

    let second = world.run(None, &["broker-restart", "lead-k"]);
    // A broker started in the gap, as a revive's would be: it retries the lock a moment.
    let mut revived = world
        .loopherd(None, &["broker", "lead-k"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fs::remove_file(world.hold()).unwrap();
    let restarted = wait_with_timeout(restart, Duration::from_secs(20));

    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr)
            .contains("another loopherd broker-restart lead-k is running"),
        "{second:?}"
    );
    assert_eq!(stdout(&in_flight.wait_with_output().unwrap()), "served\n");
    assert!(old.wait_exited(Duration::from_secs(5)));
    let restarted = stdout(&restarted);
    let serving = pid_in(&restarted, " serves");
    assert!(
        restarted.ends_with(&format!(
            "restarted lead-k's broker: pid {old_pid} stopped, pid {serving} serves\n"
        )),
        "{restarted}"
    );
    // Either may win the lock; the restart names whichever serves.
    assert!(!exited(serving));
    assert!(world.serving());
    assert!(!stop.exists());
    drop(world);
    let _ = revived.kill();
    let _ = revived.wait();
}

#[test]
fn broker_restart_from_inside_a_leads_sandbox_is_refused_with_or_without_its_pane() {
    let Some(_) = sandboxed() else {
        eprintln!("skipped: bwrap can't make a pid namespace here");
        return;
    };
    let world = World::new();
    let mut broker = world.broker();
    let requests = world
        .root
        .path()
        .join("state/loopherd/broker/lead-k/requests");
    let requests = requests.to_str().unwrap();

    for pane in [Some("w1:p1"), None] {
        let mut sandbox = sandboxed().unwrap();
        sandbox.args(["--bind", requests, requests]);
        let restart = world.loopherd(pane, &["broker-restart", "lead-k"]);
        let refused = inside(sandbox, &restart).output().unwrap();

        assert!(!refused.status.success(), "{pane:?}: {refused:?}");
        assert_eq!(
            String::from_utf8_lossy(&refused.stderr),
            "loopherd broker-restart: refused: broker-restart is the shepherd's or the human's, \
             and a lead runs this loopherd\n",
            "{pane:?}"
        );
    }
    assert!(!world.config().join("broker/lead-k.stop").exists());
    assert!(!broker.wait_exited(Duration::from_millis(300)));
    assert!(world.serving());
}

/// The device and inode of the file at `path`.
fn file_id(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

impl World {
    /// Installs this build as `loopherd` on the broker's PATH, renaming over
    /// any there, as `scripts/install` does, and returns its path.
    fn install(&self) -> std::path::PathBuf {
        self.install_from(Path::new(env!("CARGO_BIN_EXE_loopherd")))
    }

    fn install_from(&self, source: &Path) -> std::path::PathBuf {
        let installed = self.root.path().join("bin/loopherd");
        let part = installed.with_extension("part");
        // Copied by a child, so no test's fork inherits it open for writing (ETXTBSY).
        let copied = Command::new("cp").arg(source).arg(&part).status().unwrap();
        assert!(copied.success());
        fs::rename(&part, &installed).unwrap();
        installed
    }

    fn broker_from(&self, program: &Path) -> Broker {
        Broker::start(
            self.loopherd_at(program, None, &[]),
            &self.config(),
            "lead-k",
            "k",
        )
    }

    /// The file the serving broker recorded it runs.
    fn recorded_file(&self) -> Option<(u64, u64)> {
        let lock = fs::read_to_string(self.config().join("broker/lead-k.lock")).ok()?;
        let (dev, ino) = lock
            .lines()
            .find_map(|l| l.strip_prefix("file "))?
            .split_once(' ')?;
        Some((dev.parse().ok()?, ino.parse().ok()?))
    }

    /// Waits until the broker records it runs `file`, failing after 10 seconds.
    fn wait_until_runs(&self, file: (u64, u64)) {
        let given_up = Instant::now() + Duration::from_secs(10);
        while self.recorded_file() != Some(file) {
            assert!(
                Instant::now() < given_up,
                "the broker still runs {:?}, not {file:?}",
                self.recorded_file()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn serving_pid(&self) -> u32 {
        fs::read_to_string(self.config().join("broker/lead-k.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn read_as_lead(&self) -> String {
        stdout(&self.run(Some("w1:p1"), &["read", "lh-1"]))
    }
}

#[test]
fn a_broker_moves_onto_a_reinstalled_loopherd_after_its_next_request_keeping_its_pid() {
    let world = World::new();
    let installed = world.install();
    let broker = world.broker_from(&installed);
    world.wait_until_runs(file_id(&installed));

    world.install();
    let reinstalled = file_id(&installed);
    assert_eq!(world.read_as_lead(), "served\n");

    world.wait_until_runs(reinstalled);
    assert_eq!(world.serving_pid(), broker.pid());
    assert!(world.serving());
    assert_eq!(world.read_as_lead(), "served\n");
}

#[test]
fn a_broker_keeps_its_lock_held_through_the_exec_onto_a_reinstall() {
    let world = World::new();
    let installed = world.install();
    let _broker = world.broker_from(&installed);
    world.wait_until_runs(file_id(&installed));
    let dev_build = Path::new(env!("CARGO_BIN_EXE_loopherd"));
    let seen = world.root.path().join("seen");
    let wrapper = world.root.path().join("wrapper");
    executable(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = broker ]; then\n\
             if flock -n {lock} true; then echo free; else echo held; fi >> {seen}\n\
             fi\n\
             exec {dev_build} \"$@\"\n",
            lock = world.config().join("broker/lead-k.lock").display(),
            seen = seen.display(),
            dev_build = dev_build.display(),
        ),
    );

    world.install_from(&wrapper);
    assert_eq!(world.read_as_lead(), "served\n");

    world.wait_until_runs(file_id(dev_build));
    assert_eq!(fs::read_to_string(&seen).unwrap(), "held\n");
    assert_eq!(world.read_as_lead(), "served\n");
}

impl World {
    /// Makes herdr's `agent read` wait for a `release` file, returning
    /// `(started, release)`: the file it touches on entry, and the one it waits for.
    fn gate_reads(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        let (started, release) = (
            self.root.path().join("started"),
            self.root.path().join("release"),
        );
        let herdr = self.root.path().join("bin/herdr");
        let stub = fs::read_to_string(&herdr).unwrap();
        let gated = format!(
            "touch {}; until [ -e {} ]; do sleep 0.02; done; echo served",
            started.display(),
            release.display()
        );
        executable(&herdr, stub.replace("echo served", &gated));
        (started, release)
    }

    /// Starts the lead's `read lh-1` and waits until it reaches herdr.
    fn read_in_flight(&self, started: &Path) -> std::process::Child {
        let read = self
            .loopherd(Some("w1:p1"), &["read", "lh-1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let given_up = Instant::now() + Duration::from_secs(10);
        while !started.exists() {
            assert!(Instant::now() < given_up, "the read never reached herdr");
            std::thread::sleep(Duration::from_millis(20));
        }
        read
    }
}

#[test]
fn a_reinstall_mid_request_answers_that_request_then_moves_the_broker() {
    let world = World::new();
    let (started, release) = world.gate_reads();
    let installed = world.install();
    let broker = world.broker_from(&installed);
    let in_flight = world.read_in_flight(&started);

    world.install();
    let reinstalled = file_id(&installed);
    std::thread::sleep(Duration::from_millis(300));
    fs::write(&release, "").unwrap();

    assert_eq!(stdout(&in_flight.wait_with_output().unwrap()), "served\n");
    world.wait_until_runs(reinstalled);
    assert_eq!(world.serving_pid(), broker.pid());
    assert_eq!(world.read_as_lead(), "served\n");
}

#[test]
fn two_quick_reinstalls_end_with_the_newest_serving() {
    let world = World::new();
    let (started, release) = world.gate_reads();
    let installed = world.install();
    let _broker = world.broker_from(&installed);
    let in_flight = world.read_in_flight(&started);

    world.install();
    world.install();
    let newest = file_id(&installed);
    fs::write(&release, "").unwrap();

    assert_eq!(stdout(&in_flight.wait_with_output().unwrap()), "served\n");
    world.wait_until_runs(newest);
    assert_eq!(world.read_as_lead(), "served\n");
}

fn notes(world: &World) -> Vec<String> {
    let mailbox = world
        .root
        .path()
        .join("state/loopherd/reports/lead-k.jsonl");
    fs::read_to_string(mailbox)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn an_install_that_exits_at_once_is_noted_once_and_the_old_broker_serves_until_the_next_install() {
    let world = World::new();
    let installed = world.install();
    let broker = world.broker_from(&installed);
    let running = file_id(&installed);
    let probes = world.root.path().join("probes");
    let broken = world.root.path().join("broken");
    executable(
        &broken,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", probes.display()),
    );
    world.install_from(&broken);

    assert_eq!(world.read_as_lead(), "served\n");
    let given_up = Instant::now() + Duration::from_secs(10);
    while notes(&world).is_empty() {
        assert!(
            Instant::now() < given_up,
            "no note reached the lead's mailbox"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(world.read_as_lead(), "served\n");
    std::thread::sleep(Duration::from_millis(300));

    let notes = notes(&world);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("exited at once")
            && notes[0].contains("the shepherd runs loopherd broker-restart lead-k"),
        "{}",
        notes[0]
    );
    assert_eq!(fs::read_to_string(&probes).unwrap(), "--version\n");
    assert_eq!(world.recorded_file(), Some(running));
    assert_eq!(world.serving_pid(), broker.pid());

    world.install();
    let fixed = file_id(&installed);
    assert_eq!(world.read_as_lead(), "served\n");
    world.wait_until_runs(fixed);
}

#[test]
fn a_broker_started_from_another_loopherd_than_paths_stays_on_it() {
    let world = World::new();
    let dev_build = Path::new(env!("CARGO_BIN_EXE_loopherd"));
    world.install();
    let _broker = world.broker_from(dev_build);
    world.wait_until_runs(file_id(dev_build));

    assert_eq!(world.read_as_lead(), "served\n");
    std::thread::sleep(Duration::from_millis(1500));

    assert_eq!(world.recorded_file(), Some(file_id(dev_build)));
    assert_eq!(world.read_as_lead(), "served\n");
}
