//! A lead's broker (D98): one process per lead, outside its sandbox, that
//! carries out the lead's herdr-touching operations within its herd's reach.
//!
//! The lead writes requests to its own folder in the state dir; the broker
//! answers in the config dir, which the lead can read but not write.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::context::SHEPHERD;
use crate::herd::Paths;
use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::lock::Lock;
use crate::process::Procs;
use crate::sandbox;
use crate::tool_hook;

/// An operation a lead asks its broker for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    Tell { name: String, text: String },
}

impl Op {
    fn command(&self) -> &'static str {
        match self {
            Op::Tell { .. } => "tell",
        }
    }

    fn target(&self) -> &str {
        match self {
            Op::Tell { name, .. } => name,
        }
    }

    fn carry_out(&self, herdr: &dyn Herdr) -> Result<String> {
        match self {
            Op::Tell { name, text } => {
                crate::tell::tell(herdr, name, text, crate::tell::WORKING_TIMEOUT)
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Done(String),
    Refused(String),
    Failed(String),
}

/// How a broker's run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    /// Its lead was retired, or left the ledger.
    Retired,
    /// Another broker already serves the lead.
    AlreadyRunning,
}

/// Requests bigger than this are refused unread.
const MAX_REQUEST: u64 = 1 << 20;
/// A reply nobody read by now belongs to a client that gave up.
const REPLY_KEPT: Duration = Duration::from_secs(600);

/// Runs `op` through the lead's broker when `client` is set, else against herdr directly.
pub fn run(herdr: &dyn Herdr, client: Option<&Client>, op: &Op) -> Result<String> {
    match client {
        Some(client) => client.call(op),
        None => op.carry_out(herdr),
    }
}

/// Serves lead `lead`'s requests, polling every `poll`, until it is retired.
/// Its herd comes from the record `loopherd lead` kept in the config dir,
/// never from a request or the ledger.
pub fn serve(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    lead: &str,
    poll: Duration,
) -> Result<Served> {
    let Some(_lock) = take_lock(paths, lead)? else {
        return Ok(Served::AlreadyRunning);
    };
    let herd = sandbox::Lead::kept(paths, lead)?
        .ok_or_else(|| anyhow!("{lead} has no launch record, so its broker has no herd"))?
        .herd;
    let broker = Broker { lead, herd: &herd };
    let requests = requests_dir(paths, lead);
    let replies = replies_dir(paths, lead);
    for dir in [&requests, &replies] {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    loop {
        let unretired = ledger
            .entries()
            .map(|entries| entries.get(lead).is_some_and(|r| !r.retired))
            // An unreadable ledger is no proof of retirement.
            .unwrap_or(true);
        if !unretired {
            return Ok(Served::Retired);
        }
        for request in pending(&requests) {
            broker.answer(herdr, ledger, &request, &replies);
        }
        prune(&replies);
        std::thread::sleep(poll);
    }
}

struct Broker<'a> {
    lead: &'a str,
    herd: &'a str,
}

impl Broker<'_> {
    /// Writes a reply for the request at `path`, then removes it. A request
    /// whose file name can't name a reply is only removed.
    fn answer(&self, herdr: &dyn Herdr, ledger: &dyn Ledger, path: &Path, replies: &Path) {
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .filter(|id| is_id(id))
            .map(str::to_string);
        if let Some(id) = id {
            let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.reply(herdr, ledger, path)
            }))
            .unwrap_or_else(|_| Reply::Failed("the broker panicked on this request".into()));
            let _ = write_atomically(
                replies,
                &id,
                &serde_json::to_vec(&reply).unwrap_or_default(),
            );
        }
        let _ = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
            _ => std::fs::remove_file(path),
        };
    }

    fn reply(&self, herdr: &dyn Herdr, ledger: &dyn Ledger, path: &Path) -> Reply {
        let op = match read_request(path) {
            Ok(op) => op,
            Err(e) => return Reply::Refused(format!("{e:#}")),
        };
        match self.refusal(ledger, &op) {
            Ok(Some(why)) => return Reply::Refused(why),
            Ok(None) => {}
            Err(e) => return Reply::Failed(format!("{e:#}")),
        }
        match op.carry_out(herdr) {
            Ok(text) => Reply::Done(text),
            Err(e) => Reply::Failed(format!("{e:#}")),
        }
    }

    /// Why `op` is outside the lead's reach: its own herd's unretired workers.
    fn refusal(&self, ledger: &dyn Ledger, op: &Op) -> Result<Option<String>> {
        let (lead, herd) = (self.lead, self.herd);
        let target = op.target();
        let reach = format!("{lead} reaches only herd {herd}'s workers");
        if target == SHEPHERD {
            return Ok(Some(format!("{target} is the shepherd; {reach}")));
        }
        if target == lead {
            return Ok(Some(format!(
                "{target} is the lead itself; {} reaches only herd {herd}'s workers",
                op.command()
            )));
        }
        let entries = ledger.entries()?;
        let Some(record) = entries.get(target).filter(|r| !r.retired) else {
            return Ok(Some(format!(
                "no unretired member is named {target}; {reach}"
            )));
        };
        Ok(match record.herd.as_deref() {
            None => Some(format!("{target} is in no herd; {reach}")),
            Some(other) if other != herd => Some(format!("{target} is in herd {other}; {reach}")),
            Some(_) if record.is_lead() => Some(format!("{target} is a lead; {reach}")),
            Some(_) => None,
        })
    }
}

/// The lead's side of the transport: writes requests, reads replies.
pub struct Client {
    lead: String,
    requests: PathBuf,
    replies: PathBuf,
    lock: PathBuf,
    answer_within: Duration,
}

impl Client {
    pub fn for_lead(paths: &Paths, lead: &str) -> Client {
        Client {
            lead: lead.into(),
            requests: requests_dir(paths, lead),
            replies: replies_dir(paths, lead),
            lock: lock_file(paths, lead),
            answer_within: Duration::from_secs(120),
        }
    }

    /// Hands `op` to the broker and waits for its answer. Fails at once when
    /// no broker runs, and as soon as one stops without answering.
    pub fn call(&self, op: &Op) -> Result<String> {
        let lead = &self.lead;
        let revive = format!("run loopherd revive --name {lead} to start it");
        if !held(&self.lock) {
            bail!(
                "no broker is running for {lead}, so it can't {}: {revive}",
                op.command()
            );
        }
        let id = new_id();
        write_atomically(
            &self.requests,
            &id,
            &serde_json::to_vec(&Request { op: op.clone() })?,
        )
        .with_context(|| format!("hand {} to {lead}'s broker", op.command()))?;
        let reply = self.replies.join(format!("{id}.json"));
        let given_up = Instant::now() + self.answer_within;
        loop {
            if let Ok(text) = std::fs::read(&reply) {
                return match serde_json::from_slice(&text)
                    .with_context(|| format!("read {lead}'s broker's reply"))?
                {
                    Reply::Done(text) => Ok(text),
                    Reply::Refused(why) => {
                        Err(anyhow!("{lead}'s broker refused {}: {why}", op.command()))
                    }
                    Reply::Failed(why) => Err(anyhow!("{why}")),
                };
            }
            if !held(&self.lock) {
                bail!(
                    "{lead}'s broker stopped before answering {}: {revive}",
                    op.command()
                );
            }
            if Instant::now() >= given_up {
                bail!(
                    "{lead}'s broker didn't answer {} within {}; check it with loopherd doctor",
                    op.command(),
                    humantime::format_duration(self.answer_within)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    op: Op,
}

/// The lead running this loopherd, process `me`: the one whose sandboxed
/// Claude is an ancestor, else the lead the ledger has in `pane`. Asks herdr nothing.
pub fn lead_here(
    paths: &Paths,
    ledger: &dyn Ledger,
    procs: &dyn Procs,
    me: u32,
    pane: Option<&tool_hook::Pane>,
) -> Result<Option<String>> {
    if let Some(lead) = sandbox::calling_lead(paths, procs, me) {
        return Ok(Some(lead));
    }
    let Some(pane) = pane else {
        return Ok(None);
    };
    Ok(tool_hook::lead_in(ledger, pane)?.map(|record| record.name))
}

/// Starts lead `lead`'s broker unless one runs, saying whether it started one.
pub type StartBroker = fn(&Paths, &str) -> Result<bool>;

/// Starts `loopherd broker <lead>` detached, in a session of its own, unless
/// a broker already serves `lead`; a second one would exit at once anyway.
pub fn start(paths: &Paths, lead: &str) -> Result<bool> {
    use std::os::unix::process::CommandExt;
    if running(paths, lead) {
        return Ok(false);
    }
    let exe = std::env::current_exe().context("find the loopherd binary")?;
    let mut command = std::process::Command::new(&exe);
    command
        .args(["broker", lead])
        .env_remove("HERDR_PANE_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe. A session of its own outlives the
    // shell that started it.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    command
        .spawn()
        .with_context(|| format!("start {} broker {lead}", exe.display()))?;
    Ok(true)
}

/// Whether a broker serves `lead` now.
pub fn running(paths: &Paths, lead: &str) -> bool {
    held(&lock_file(paths, lead))
}

fn held(lock: &Path) -> bool {
    // Read-only, so a sandboxed lead can probe a lock in the config dir.
    let Ok(file) = std::fs::File::open(lock) else {
        return false;
    };
    matches!(Lock::try_take(file), Ok(None))
}

fn take_lock(paths: &Paths, lead: &str) -> Result<Option<Lock>> {
    let path = lock_file(paths, lead);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    Lock::try_take(file).with_context(|| format!("lock {}", path.display()))
}

/// The request at `path`, refusing anything but a small regular file of one known op.
fn read_request(path: &Path) -> Result<Op> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("not a request: can't open it as a plain file")?;
    let meta = file.metadata().context("not a request")?;
    if !meta.is_file() {
        bail!("not a request: not a plain file");
    }
    if meta.len() > MAX_REQUEST {
        bail!("not a request: over {MAX_REQUEST} bytes");
    }
    let mut text = vec![];
    file.take(MAX_REQUEST + 1).read_to_end(&mut text)?;
    let request: Request = serde_json::from_slice(&text).context("malformed request")?;
    Ok(request.op)
}

/// Request files ready to answer, oldest name first; dotfiles are still being written.
fn pending(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut found: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

fn prune(replies: &Path) {
    let Ok(entries) = std::fs::read_dir(replies) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > REPLY_KEPT);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn write_atomically(dir: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    let partial = dir.join(format!(".{id}.part"));
    std::fs::write(&partial, bytes).with_context(|| format!("write {}", partial.display()))?;
    let done = dir.join(format!("{id}.json"));
    std::fs::rename(&partial, &done).with_context(|| format!("write {}", done.display()))
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos}-{}", std::process::id())
}

fn is_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn requests_dir(paths: &Paths, lead: &str) -> PathBuf {
    paths.state_dir().join("broker").join(lead).join("requests")
}

fn replies_dir(paths: &Paths, lead: &str) -> PathBuf {
    broker_dir(paths).join(lead).join("replies")
}

fn broker_dir(paths: &Paths) -> PathBuf {
    paths.config_dir().join("broker")
}

fn lock_file(paths: &Paths, lead: &str) -> PathBuf {
    broker_dir(paths).join(format!("{lead}.lock"))
}

#[cfg(test)]
mod tests {
    use std::thread::JoinHandle;

    use serde_json::json;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, FileLedger, Role};
    use crate::sandbox::{self, Unavailable};
    use crate::subagent_view::screens::MAIN_VIEW;

    const POLL: Duration = Duration::from_millis(10);

    /// A config and state dir holding herd `h` with lead `lead-h`.
    struct World {
        dir: tempfile::TempDir,
        paths: Paths,
    }

    impl World {
        fn new() -> World {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let world = World { dir, paths };
            world.herd("h");
            world.launch("lead-h", "h");
            world
        }

        fn herd(&self, herd: &str) {
            let repo = self.dir.path().join(format!("repos/{herd}"));
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::create_dir_all(&self.paths.herds).unwrap();
            let text = format!(
                "repo = {:?}\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
                repo.to_str().unwrap()
            );
            std::fs::write(self.paths.herds.join(format!("{herd}.toml")), text).unwrap();
        }

        /// Keeps `lead`'s launch record, as `loopherd lead` does, and records it in the ledger.
        fn launch(&self, lead: &str, herd: &str) {
            let kept = sandbox::Lead {
                herd: herd.into(),
                args: vec![],
            };
            fn no_sandbox() -> Result<(), Unavailable> {
                Err(Unavailable {
                    why: "none here".into(),
                    fix: "none".into(),
                })
            }
            sandbox::prepare(&self.paths, lead, &kept, no_sandbox).unwrap();
            let mut lead = member(lead, Some(herd));
            lead.role = Some(Role::Lead);
            self.ledger().record(lead).unwrap();
        }

        fn ledger(&self) -> FileLedger {
            FileLedger::new(self.dir.path().join("ledger.json"))
        }

        fn add(&self, record: AgentRecord) {
            self.ledger().record(record).unwrap();
        }

        fn serve(&self, herdr: FakeHerdr) -> JoinHandle<(Result<Served>, FakeHerdr)> {
            let paths = self.paths.clone();
            let mut ledger = self.ledger();
            let serving = std::thread::spawn(move || {
                let served = serve(&herdr, &mut ledger, &paths, "lead-h", POLL);
                (served, herdr)
            });
            let given_up = Instant::now() + Duration::from_secs(5);
            while !running(&self.paths, "lead-h") && !serving.is_finished() {
                assert!(Instant::now() < given_up, "the broker never started");
                std::thread::sleep(POLL);
            }
            serving
        }

        fn retire(&self, name: &str) {
            self.ledger()
                .update(name, &mut |r| {
                    r.retired = true;
                    Ok(())
                })
                .unwrap();
        }

        fn client(&self) -> Client {
            Client::for_lead(&self.paths, "lead-h")
        }
    }

    fn member(name: &str, herd: Option<&str>) -> AgentRecord {
        serde_json::from_value(json!({
            "name": name, "pane_id": format!("p-{name}"), "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd,
        }))
        .unwrap()
    }

    fn tell(name: &str, text: &str) -> Op {
        Op::Tell {
            name: name.into(),
            text: text.into(),
        }
    }

    /// A herdr whose `lh-1` shows its main view and goes working when prompted.
    fn telling_herdr() -> FakeHerdr {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        herdr.set_screen("w9:p9", MAIN_VIEW);
        herdr.set_working_on_prompt();
        herdr
    }

    #[test]
    fn a_lead_tells_its_own_worker_through_its_broker_and_calls_no_herdr_itself() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let serving = world.serve(telling_herdr());
        let own = FakeHerdr::new(&[]);

        let out = run(
            &own,
            Some(&world.client()),
            &tell("lh-1", "rebase onto main"),
        )
        .unwrap();

        assert_eq!(out, "told lh-1; it's working");
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (served, herdr) = serving.join().unwrap();
        assert_eq!(served.unwrap(), Served::Retired);
        assert_eq!(
            herdr.prompts(),
            vec![("lh-1".into(), "rebase onto main".into())]
        );
    }

    #[test]
    fn a_target_outside_the_herds_unretired_workers_is_refused_naming_why() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world.add(member("lh-8", None));
        let mut retired = member("lh-7", Some("h"));
        retired.retired = true;
        world.add(retired);
        let serving = world.serve(telling_herdr());
        let reach = "lead-h reaches only herd h's workers";

        for (target, why) in [
            ("shepherd", format!("shepherd is the shepherd; {reach}")),
            ("lh-9", format!("lh-9 is in herd g; {reach}")),
            ("lh-8", format!("lh-8 is in no herd; {reach}")),
            (
                "lh-7",
                format!("no unretired member is named lh-7; {reach}"),
            ),
            (
                "lh-6",
                format!("no unretired member is named lh-6; {reach}"),
            ),
            (
                "lead-h",
                "lead-h is the lead itself; tell reaches only herd h's workers".into(),
            ),
        ] {
            let err = world.client().call(&tell(target, "hi")).unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!("lead-h's broker refused tell: {why}")
            );
        }

        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.calls(), 0);
    }

    #[test]
    fn the_herd_comes_from_the_launch_record_not_the_ledger() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.herd = Some("g".into());
                Ok(())
            })
            .unwrap();
        let serving = world.serve(telling_herdr());

        let err = world.client().call(&tell("lh-9", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker refused tell: lh-9 is in herd g; lead-h reaches only herd h's workers"
        );
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn forged_and_malformed_requests_are_refused_and_the_broker_keeps_serving() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let requests = requests_dir(&world.paths, "lead-h");
        std::fs::create_dir_all(&requests).unwrap();
        let secret = world.dir.path().join("secret");
        std::fs::write(&secret, r#"{"op":{"tell":{"name":"lh-1","text":"hi"}}}"#).unwrap();
        std::os::unix::fs::symlink(&secret, requests.join("link.json")).unwrap();
        std::fs::write(requests.join("garbage.json"), "{ nope").unwrap();
        std::fs::write(
            requests.join("spawn.json"),
            r#"{"op":{"spawn":{"name":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            requests.join("extra.json"),
            r#"{"op":{"tell":{"name":"lh-1","text":"hi"}},"lead":"lead-g"}"#,
        )
        .unwrap();
        std::fs::create_dir(requests.join("dir.json")).unwrap();
        std::fs::write(requests.join("bad name.json"), "{}").unwrap();
        std::fs::write(
            requests.join("huge.json"),
            vec![b' '; MAX_REQUEST as usize + 1],
        )
        .unwrap();
        let serving = world.serve(telling_herdr());

        let out = world.client().call(&tell("lh-1", "hi")).unwrap();

        assert_eq!(out, "told lh-1; it's working");
        let replies = replies_dir(&world.paths, "lead-h");
        let refused = |id: &str| -> String {
            let text = std::fs::read_to_string(replies.join(format!("{id}.json"))).unwrap();
            match serde_json::from_str(&text).unwrap() {
                Reply::Refused(why) => why,
                other => panic!("{id}: {other:?}"),
            }
        };
        assert_eq!(
            refused("link"),
            "not a request: can't open it as a plain file: Too many levels of symbolic links (os error 40)"
        );
        assert!(refused("garbage").starts_with("malformed request: "));
        assert!(refused("spawn").starts_with("malformed request: unknown variant `spawn`"));
        assert!(refused("extra").starts_with("malformed request: unknown field `lead`"));
        assert_eq!(refused("dir"), "not a request: not a plain file");
        assert_eq!(refused("huge"), "not a request: over 1048576 bytes");
        assert!(!replies.join("bad name.json").exists());
        assert_eq!(std::fs::read_dir(&requests).unwrap().count(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_second_broker_for_the_same_lead_does_not_start() {
        let world = World::new();
        let serving = world.serve(FakeHerdr::new(&[]));

        let second = serve(
            &FakeHerdr::new(&[]),
            &mut world.ledger(),
            &world.paths,
            "lead-h",
            POLL,
        );

        assert_eq!(second.unwrap(), Served::AlreadyRunning);
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn with_no_broker_running_a_lead_s_command_fails_at_once_naming_it_and_revive() {
        let world = World::new();
        let started = Instant::now();

        let err = world.client().call(&tell("lh-1", "hi")).unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            format!("{err:#}"),
            "no broker is running for lead-h, so it can't tell: \
             run loopherd revive --name lead-h to start it"
        );
    }

    #[test]
    fn without_a_client_the_op_runs_against_herdr_directly() {
        let herdr = telling_herdr();

        let out = run(&herdr, None, &tell("lh-1", "hi")).unwrap();

        assert_eq!(out, "told lh-1; it's working");
        assert_eq!(herdr.prompts().len(), 1);
    }

    /// herdr → shell → claude → bash 40 → loopherd 50, where the Claude was started with `args`.
    fn procs(args: &[&str]) -> crate::process::FakeProcs {
        let mut procs = crate::process::FakeProcs::pane();
        procs.set_cmdline(30, args);
        procs.add(40, "/bin/bash", 30, 400);
        procs.add(50, "/usr/bin/loopherd", 40, 500);
        procs
    }

    #[test]
    fn the_lead_here_is_the_one_whose_sandboxed_claude_runs_this_loopherd() {
        let world = World::new();
        let settings = sandbox::settings_file(&world.paths, "lead-h");
        let procs = procs(&["claude", "--settings", settings.to_str().unwrap()]);

        let here = lead_here(&world.paths, &world.ledger(), &procs, 50, None).unwrap();

        assert_eq!(here.as_deref(), Some("lead-h"));
    }

    #[test]
    fn a_hook_only_lead_is_found_by_its_pane_and_a_worker_is_no_lead() {
        let world = World::new();
        let socket = Path::new("/run/h.sock");
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some(socket.into());
                Ok(())
            })
            .unwrap();
        let mut worker = member("lh-1", Some("h"));
        worker.socket = Some(socket.into());
        world.add(worker);
        let procs = procs(&["claude"]);
        let here = |id: &str| {
            let pane = tool_hook::Pane {
                id,
                socket: Some(socket),
                lineage: &[],
            };
            lead_here(&world.paths, &world.ledger(), &procs, 50, Some(&pane)).unwrap()
        };

        assert_eq!(here("p-lead-h").as_deref(), Some("lead-h"));
        assert_eq!(here("p-lh-1"), None);
    }
}
