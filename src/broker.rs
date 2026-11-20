//! A lead's broker (D98): one process per lead, outside its sandbox, that
//! carries out the lead's herdr-touching operations within its herd's reach.
//!
//! The lead writes requests to its own folder in the state dir; the broker
//! answers in the config dir, which the lead can read but not write.

use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
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
use crate::spawn::validate_name;
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
    validate_name(lead)?;
    let Some(_lock) = take_lock(paths, lead)? else {
        return Ok(Served::AlreadyRunning);
    };
    let herd = sandbox::Lead::kept(paths, lead)?
        .ok_or_else(|| anyhow!("{lead} has no launch record, so its broker has no herd"))?
        .herd;
    let broker = Broker { lead, herd: &herd };
    let replies = replies_dir(paths, lead);
    std::fs::create_dir_all(&replies).with_context(|| format!("create {}", replies.display()))?;
    loop {
        let unretired = ledger
            .entries()
            .map(|entries| entries.get(lead).is_some_and(|r| !r.retired))
            // An unreadable ledger is no proof of retirement.
            .unwrap_or(true);
        if !unretired {
            return Ok(Served::Retired);
        }
        // Reopened each round: the lead can replace any folder on the way.
        if let Ok(requests) = Dir::open(paths.state_dir(), &["broker", lead, "requests"]) {
            for name in requests.names() {
                broker.answer(herdr, ledger, &requests, &name, &replies);
            }
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
    /// Writes a reply for request `name`, then removes it. A request whose
    /// name can't name a reply is only removed.
    fn answer(
        &self,
        herdr: &dyn Herdr,
        ledger: &dyn Ledger,
        requests: &Dir,
        name: &str,
        replies: &Path,
    ) {
        if let Some(id) = name.strip_suffix(".json").filter(|id| is_id(id)) {
            let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.reply(herdr, ledger, requests, name)
            }))
            .unwrap_or_else(|_| Reply::Failed("the broker panicked on this request".into()));
            let _ = write_atomically(replies, id, &serde_json::to_vec(&reply).unwrap_or_default());
        }
        requests.remove(name);
    }

    fn reply(&self, herdr: &dyn Herdr, ledger: &dyn Ledger, requests: &Dir, name: &str) -> Reply {
        let op = match requests.read_request(name) {
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
            Some(_) if record.may_be_lead() => Some(format!(
                "{target}'s role is from another loopherd version; {reach}"
            )),
            Some(_) => None,
        })
    }
}

/// A folder opened one part at a time without following symlinks, so a
/// lead that can write the parts can't point the broker anywhere else.
struct Dir(OwnedFd);

impl Dir {
    /// Opens `root`/`parts`…, creating missing parts; `root` is trusted.
    fn open(root: &Path, parts: &[&str]) -> std::io::Result<Dir> {
        let mut dir = Dir(std::fs::File::open(root)?.into());
        for part in parts {
            let name = CString::new(*part)?;
            // SAFETY: both calls take a descriptor `dir` keeps open and a C string.
            let fd = unsafe {
                libc::mkdirat(dir.0.as_raw_fd(), name.as_ptr(), 0o700);
                libc::openat(
                    dir.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: openat just returned this descriptor, owned by nothing else.
            dir = Dir(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        Ok(dir)
    }

    /// Entries ready to answer, oldest name first; dotfiles are still being written.
    fn names(&self) -> Vec<String> {
        let mut names = vec![];
        // SAFETY: fdopendir takes ownership of the dup, which closedir closes;
        // each entry is read before the next readdir.
        unsafe {
            let fd = libc::fcntl(self.0.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
            if fd < 0 {
                return names;
            }
            let dir = libc::fdopendir(fd);
            if dir.is_null() {
                libc::close(fd);
                return names;
            }
            // The dup shares the descriptor's offset, which an earlier listing moved.
            libc::rewinddir(dir);
            loop {
                let entry = libc::readdir(dir);
                if entry.is_null() {
                    break;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr());
                if let Ok(name) = name.to_str()
                    && !name.starts_with('.')
                {
                    names.push(name.to_string());
                }
            }
            libc::closedir(dir);
        }
        names.sort();
        names
    }

    /// Request `name`, refusing anything but a small regular file of one known op.
    fn read_request(&self, name: &str) -> Result<Op> {
        let c_name = CString::new(name)?;
        // SAFETY: openat on a descriptor `self` keeps open and a C string.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            bail!("not a request: not a plain file");
        }
        // SAFETY: openat just returned this descriptor, owned by nothing else.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let meta = file.metadata().context("not a request")?;
        // A hard link would hand the broker a file the lead can't read itself.
        if !meta.is_file() || meta.nlink() > 1 {
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

    /// Removes entry `name` itself, never what a symlink names; a folder only when empty.
    fn remove(&self, name: &str) {
        let Ok(c_name) = CString::new(name) else {
            return;
        };
        // SAFETY: unlinkat on a descriptor `self` keeps open and a C string.
        unsafe {
            if libc::unlinkat(self.0.as_raw_fd(), c_name.as_ptr(), 0) != 0 {
                libc::unlinkat(self.0.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR);
            }
        }
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
    /// no broker runs, and as soon as one stops without answering, then
    /// withdraws the request so no later broker carries it out.
    pub fn call(&self, op: &Op) -> Result<String> {
        let lead = &self.lead;
        if !held(&self.lock) {
            bail!(
                "no broker is running for {lead}, so it can't {}: \
                 the shepherd's loopherd revive --name {lead} starts it",
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
        let answer = self.answer(&id, op);
        if answer.is_err() {
            let _ = std::fs::remove_file(self.requests.join(format!("{id}.json")));
        }
        answer
    }

    fn answer(&self, id: &str, op: &Op) -> Result<String> {
        let lead = &self.lead;
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
                    "{lead}'s broker stopped before answering {}: \
                     the shepherd's loopherd revive --name {lead} starts it again",
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

/// The broker of the lead running this loopherd, process `me`: the lead
/// whose sandboxed Claude is an ancestor, else the unretired lead the ledger
/// has in `pane`. Asks herdr nothing; a ledger it can't read names no lead.
pub fn route(
    paths: &Paths,
    ledger: &dyn Ledger,
    procs: &dyn Procs,
    me: u32,
    pane: Option<&tool_hook::Pane>,
) -> Option<Client> {
    let lead = sandbox::calling_lead(paths, procs, me).or_else(|| {
        let record = tool_hook::lead_in(ledger, pane?).ok()??;
        (!record.retired).then_some(record.name)
    })?;
    Some(Client::for_lead(paths, &lead))
}

/// Starts lead `lead`'s broker unless one runs, saying whether it started one.
pub type StartBroker = fn(&Paths, &str) -> Result<bool>;

/// How long `start` waits for the broker it started to take its lock.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts `loopherd broker <lead>` detached, in a session of its own, unless
/// a broker already serves `lead`, and waits until it serves.
pub fn start(paths: &Paths, lead: &str) -> Result<bool> {
    use std::os::unix::process::CommandExt;
    validate_name(lead)?;
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
    let mut child = command
        .spawn()
        .with_context(|| format!("start {} broker {lead}", exe.display()))?;
    let given_up = Instant::now() + START_TIMEOUT;
    loop {
        if running(paths, lead) {
            return Ok(true);
        }
        if let Some(status) = child.try_wait()? {
            bail!("it exited at once ({status}); run loopherd broker {lead} to see why");
        }
        if Instant::now() >= given_up {
            bail!(
                "it didn't take {} within {}",
                lock_file(paths, lead).display(),
                humantime::format_duration(START_TIMEOUT)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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

/// How long a starting broker retries its lock: a probe holds it for an instant.
const LOCK_GRACE: Duration = Duration::from_secs(1);

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
    let given_up = Instant::now() + LOCK_GRACE;
    loop {
        let attempt = file
            .try_clone()
            .with_context(|| format!("open {}", path.display()))?;
        let lock = Lock::try_take(attempt).with_context(|| format!("lock {}", path.display()))?;
        if lock.is_some() || Instant::now() >= given_up {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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

/// Where lead `lead` hands its broker requests, the one folder of the
/// broker's that the lead's sandbox lets it write.
pub fn requests_dir(paths: &Paths, lead: &str) -> PathBuf {
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
    fn a_target_outside_the_herds_unretired_workers_is_refused_naming_why() {
        let world = World::new();
        world.herd("g");
        world.add(member("lh-9", Some("g")));
        world.add(member("lh-8", None));
        let mut unknown_role = member("lh-5", Some("h"));
        unknown_role.role = Some(crate::ledger::Role::Unknown("reviewer".into()));
        world.add(unknown_role);
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
                "lh-5",
                format!("lh-5's role is from another loopherd version; {reach}"),
            ),
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
        std::fs::hard_link(&secret, requests.join("hard.json")).unwrap();
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
        assert_eq!(refused("link"), "not a request: not a plain file");
        assert_eq!(refused("hard"), "not a request: not a plain file");
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
             the shepherd's loopherd revive --name lead-h starts it"
        );
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
    fn in_a_sandboxed_lead_s_pane_tell_goes_through_its_broker_and_calls_no_herdr() {
        let world = World::new();
        world.add(member("lh-1", Some("h")));
        let settings = sandbox::settings_file(&world.paths, "lead-h");
        let procs = procs(&["claude", "--settings", settings.to_str().unwrap()]);
        let serving = world.serve(telling_herdr());
        let own = FakeHerdr::new(&[]);

        let client = route(&world.paths, &world.ledger(), &procs, 50, None);
        let out = run(&own, client.as_ref(), &tell("lh-1", "hi")).unwrap();

        assert_eq!(out, "told lh-1; it's working");
        assert_eq!(own.calls(), 0);
        world.retire("lead-h");
        let (_, herdr) = serving.join().unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_hook_only_lead_routes_by_its_pane_and_others_go_to_herdr_directly() {
        let world = World::new();
        let socket = Path::new("/run/h.sock");
        let on_socket = |mut record: AgentRecord| {
            record.socket = Some(socket.into());
            record
        };
        world
            .ledger()
            .update("lead-h", &mut |r| {
                r.socket = Some(socket.into());
                Ok(())
            })
            .unwrap();
        world.add(on_socket(member("lh-1", Some("h"))));
        let mut gone = on_socket(member("lead-g", Some("g")));
        gone.role = Some(Role::Lead);
        gone.retired = true;
        world.add(gone);
        let procs = procs(&["claude"]);
        let routed = |id: &str| {
            let pane = tool_hook::Pane {
                id,
                socket: Some(socket),
                lineage: &[],
            };
            route(&world.paths, &world.ledger(), &procs, 50, Some(&pane)).map(|c| c.lead)
        };

        assert_eq!(routed("p-lead-h").as_deref(), Some("lead-h"));
        assert_eq!(routed("p-lh-1"), None);
        assert_eq!(routed("p-lead-g"), None);
        let herdr = telling_herdr();
        run(&herdr, None, &tell("lh-1", "hi")).unwrap();
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn an_unreadable_ledger_routes_nowhere_rather_than_failing() {
        let world = World::new();
        std::fs::write(world.dir.path().join("ledger.json"), "{ nope").unwrap();
        let pane = tool_hook::Pane {
            id: "p-lead-h",
            socket: Some(Path::new("/run/h.sock")),
            lineage: &[],
        };

        let client = route(
            &world.paths,
            &world.ledger(),
            &procs(&["claude"]),
            50,
            Some(&pane),
        );

        assert!(client.is_none());
    }

    #[test]
    fn a_requests_folder_swapped_for_a_symlink_is_never_followed() {
        let world = World::new();
        let victim = world.dir.path().join("victim");
        std::fs::create_dir_all(victim.join("projects")).unwrap();
        std::fs::write(victim.join("notes.json"), "{}").unwrap();
        let serving = world.serve(FakeHerdr::new(&[]));
        let requests = requests_dir(&world.paths, "lead-h");
        std::fs::remove_dir_all(&requests).unwrap();
        std::os::unix::fs::symlink(&victim, &requests).unwrap();
        let broker = world.dir.path().join("broker");
        let moved = world.dir.path().join("broker-moved");
        std::thread::sleep(POLL * 10);
        std::fs::rename(&broker, &moved).unwrap();
        std::os::unix::fs::symlink(&victim, &broker).unwrap();
        std::fs::create_dir_all(victim.join("lead-h/requests")).unwrap();
        std::fs::write(victim.join("lead-h/requests/kept.json"), "{}").unwrap();

        std::thread::sleep(POLL * 10);

        assert!(victim.join("projects").is_dir());
        assert!(victim.join("notes.json").is_file());
        assert!(victim.join("lead-h/requests/kept.json").is_file());
        world.retire("lead-h");
        serving.join().unwrap().0.unwrap();
    }

    #[test]
    fn a_lead_name_that_is_not_a_member_name_gets_no_broker() {
        let world = World::new();
        for lead in ["../x", "/tmp/x", "Lead"] {
            let served = serve(
                &FakeHerdr::new(&[]),
                &mut world.ledger(),
                &world.paths,
                lead,
                POLL,
            );
            assert!(
                format!("{:#}", served.unwrap_err()).contains("must match"),
                "{lead}"
            );
            assert!(format!("{:#}", start(&world.paths, lead).unwrap_err()).contains("must match"));
        }
    }

    #[test]
    fn a_request_the_lead_gave_up_on_is_withdrawn() {
        let world = World::new();
        let _held = take_lock(&world.paths, "lead-h").unwrap().unwrap();
        let client = Client {
            answer_within: Duration::from_millis(50),
            ..world.client()
        };
        std::fs::create_dir_all(requests_dir(&world.paths, "lead-h")).unwrap();

        let err = client.call(&tell("lh-1", "hi")).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "lead-h's broker didn't answer tell within 50ms; check it with loopherd doctor"
        );
        let left = std::fs::read_dir(requests_dir(&world.paths, "lead-h")).unwrap();
        assert_eq!(left.count(), 0);
    }

    #[test]
    fn a_broker_starting_while_its_lock_is_probed_still_starts() {
        let world = World::new();
        let probing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe = {
            let (paths, probing) = (world.paths.clone(), probing.clone());
            std::thread::spawn(move || {
                while probing.load(std::sync::atomic::Ordering::Relaxed) {
                    running(&paths, "lead-h");
                }
            })
        };

        let serving = world.serve(FakeHerdr::new(&[]));

        probing.store(false, std::sync::atomic::Ordering::Relaxed);
        probe.join().unwrap();
        assert!(!serving.is_finished());
        world.retire("lead-h");
        assert_eq!(serving.join().unwrap().0.unwrap(), Served::Retired);
    }

    #[test]
    fn a_folder_lists_its_ready_entries_in_order_each_time_it_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.json", "a.json", ".c.part"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let opened = Dir::open(dir.path(), &[]).unwrap();

        let first = opened.names();
        let second = opened.names();

        assert_eq!(first, ["a.json", "b.json"]);
        assert_eq!(second, first);
    }
}
