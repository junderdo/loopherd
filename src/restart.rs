//! `loopherd restart`: give a member that handed off a fresh session in the
//! same pane, which resumes from its handoff file (D20).

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::continuation::follow_recorded;
use crate::handoff;
use crate::herd::Paths;
use crate::herdr::{AgentStatus, Herdr};
use crate::lead::{LeadPrompt, rendered};
use crate::ledger::{Ledger, Process};
use crate::lock::Lock;
use crate::report::{Body, Entry, SinceHandoff, append, member_of_own_pane, since_handoff};
use crate::sandbox::{self, Running};
use crate::shell::quote;
use crate::subagent_view::ensure_main_view;

pub struct RestartRequest<'a> {
    /// Resolves a session to the one Claude continued it in.
    pub live: &'a dyn Fn(&str) -> String,
    pub name: &'a str,
    /// Restart even if the member isn't finished since its handoff.
    pub force: bool,
    /// `$HERDR_PANE_ID` of the caller; a member can't restart itself mid-turn.
    pub own_pane: Option<&'a str>,
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub own_lineage: &'a [Process],
    /// Reads a pane's shell process from herdr.
    pub pane_shell: &'a dyn Fn(&str) -> Result<Option<Process>>,
    /// loopherd's state dir, holding `reports/` and `handoffs/`.
    pub state: &'a Path,
    /// Where a lead's switches and review-round limit are read from.
    pub paths: &'a Paths,
    /// The lead whose sandboxed Claude runs this loopherd, if one does: it
    /// may restart only its own herd's members.
    pub caller: Option<&'a str>,
    /// How long to wait for the SessionStart hook to record the new session.
    pub settle_timeout: Duration,
    pub poll: Duration,
}

/// Sends `/clear` to the member, waits for its ledger entry to move to the
/// new session, records the session Claude continued that one in and the
/// pane and tab it runs in, marks a
/// start in its mailbox, then prompts it to read its handoff. Refuses a pane
/// showing a subagent's view, where `/clear` opens a dialog instead. Refuses,
/// without `force`, unless it reported a handoff since its last start, its
/// last stop since is `finished` with nothing in flight, and herdr shows it
/// idle or done.
pub fn restart(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &RestartRequest,
    now: SystemTime,
) -> Result<String> {
    let name = req.name;
    let record = ledger
        .entries()?
        .remove(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.retired {
        bail!("{name} is retired");
    }
    let guard = sandbox::Guard::new(req.paths.clone(), req.caller.map(String::from));
    if let Some(why) = guard.caller_refusal(&record)? {
        bail!("refused: {why}");
    }
    let socket = herdr.socket();
    match &record.socket {
        None => bail!(
            "{name} was recorded without a herdr socket, so loopherd can't follow its /clear; retire it and add it again"
        ),
        Some(theirs) if socket.as_ref().is_some_and(|ours| ours != theirs) => bail!(
            "{name} is in the herdr at {}, not this one",
            theirs.display()
        ),
        Some(_) => {}
    }
    let _running = take(req.state, name, "restart")?
        .ok_or_else(|| Refusal::Busy(format!("a restart of {name} is already running")))?;
    if req
        .own_pane
        .is_some_and(|pane| record.is_own_pane(pane, req.own_lineage))
    {
        match record.is_lead() {
            true => bail!(
                "{name} can't restart itself mid-turn; run `loopherd restart --self` to restart once this turn ends"
            ),
            false => {
                bail!("{name} can't restart itself; whoever reads its handoff line restarts it")
            }
        }
    }
    let Some(handoff) = handoff::find(req.state, name) else {
        let path = handoff::path(req.state, name);
        return Err(
            Refusal::NoHandoff(format!("{name} has no handoff at {}", path.display())).into(),
        );
    };
    let agent = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .ok_or_else(|| {
            Refusal::NotReady(format!(
                "no live agent is named {name}; run loopherd revive first"
            ))
        })?;
    if agent.status == AgentStatus::Blocked {
        return Err(Refusal::NotReady(format!(
            "{name} is blocked: answer it in pane {} first",
            agent.pane_id
        ))
        .into());
    }
    if !req.force
        && let Some(why) = unfinished(req.state, name, agent.status)?
    {
        // A lead can't force a restart; its broker refuses --force.
        let why = match req.caller {
            Some(_) => why,
            None => format!("{why}; pass --force to restart it anyway"),
        };
        return Err(Refusal::NotReady(why).into());
    }
    // The record kept beside its sandbox settings, which the lead can't write, wins over the ledger.
    let herd = match sandbox::Lead::kept(req.paths, name).context("sandbox")? {
        Some(kept) => Some(kept.herd),
        None if !record.is_lead() => None,
        None => Some(
            record
                .herd
                .clone()
                .with_context(|| format!("{name} is a lead with no herd recorded"))?,
        ),
    };
    let (lead, running) = match &herd {
        None => (None, None),
        Some(herd) => {
            let worktree = record.worktree.as_ref().unwrap_or(&record.repo);
            let prompt = rendered(req.paths, herd, name, worktree)
                .with_context(|| format!("{name}'s prompt, from herd {herd}"))?;
            let running = sandbox::check_running(
                req.paths,
                name,
                herd,
                &record.claude_args,
                record.sockets_unblocked.as_deref(),
            )
            .context("sandbox")?;
            (Some(prompt), Some(running))
        }
    };
    let (unsandboxed, warnings) = match running {
        None => crate::worker_sandbox::check_running(req.paths, &record).context("sandbox")?,
        Some(Running::Sandboxed(warnings)) => {
            sandbox::create_state(req.paths, name).context("sandbox")?;
            (None, warnings)
        }
        Some(Running::HookOnly(generic, mut warnings)) => {
            // /clear doesn't relaunch Claude, so the reason recorded at launch still holds.
            let why = record.unsandboxed.clone().unwrap_or(generic);
            warnings.push(sandbox::hook_only_warning(name, &why));
            (Some(why), warnings)
        }
    };
    let pane = &agent.pane_id;
    ensure_main_view(
        herdr,
        name,
        pane,
        "and /clear there doesn't reach the main conversation",
        &format!("run loopherd restart {name} again"),
    )?;
    let lead = lead.as_ref();
    let old = record.session_id;
    let old_live = old.as_deref().map(req.live);
    herdr.prompt(name, "/clear")?;
    let new = wait_for_new_session(ledger, name, old.as_deref(), old_live, req);
    if let Err(e) = &new {
        // A narrow pane wraps the dialog's text inside its box.
        let shows_dialog = herdr.visible_text(pane).is_ok_and(|t| {
            t.replace('│', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(CLEAR_DIALOG)
        });
        if shows_dialog {
            bail!(
                "{e:#}: {name}'s pane {pane} shows Claude's \"{CLEAR_DIALOG}\" dialog, so it was in a subagent's view. \
                 No handoff prompt was sent and the handoff is still at {}. Pick Cancel, switch the pane back to the \
                 main conversation, then run loopherd restart {name} again.",
                handoff.display()
            );
        }
        let resume = resume_prompt(lead, &handoff);
        bail!(
            "{e:#}: the /clear may be queued behind a running turn, or loopherd's SessionStart hook isn't installed. \
             No handoff prompt was sent and the handoff is still at {}. Once {name}'s pane shows a cleared session, \
             run: loopherd tell {} {}",
            handoff.display(),
            quote(name),
            quote(&resume)
        );
    }
    let new = new?;
    let entries = ledger.entries()?;
    let current = entries
        .get(name)
        .ok_or_else(|| anyhow!("{name} left the ledger during the restart"))?;
    let live = follow_recorded(ledger, &entries, current, req.live)?
        .unwrap_or_else(|| current.clone())
        .session_id
        .unwrap_or(new);
    // Moved aside once the new session is confirmed, so no later restart resumes from it.
    let aside = handoff::set_aside(req.state, name, &handoff)?;
    let read = aside.prev;
    let shell = (current.pane_id != agent.pane_id && current.env_pane_id.is_none())
        .then(|| (req.pane_shell)(&agent.pane_id));
    ledger.update(name, &mut |current| {
        // Its agent keeps the recorded id in `$HERDR_PANE_ID`, as after a missed move hook.
        if current.pane_id != agent.pane_id {
            let recorded = std::mem::take(&mut current.pane_id);
            if current.env_pane_id.is_none() {
                current.env_pane_id = Some(recorded);
                current.env_pane_shell = shell.as_ref().and_then(|s| *s.as_ref().ok()?);
            }
        }
        current.pane_id = agent.pane_id.clone();
        current.tab_id = Some(agent.tab_id.clone());
        current.unsandboxed = unsandboxed.clone();
        Ok(())
    })?;
    ledger.started(name, now, false)?;
    herdr.prompt(name, &resume_prompt(lead, &read))?;
    let unread = match shell {
        Some(Err(e)) => {
            format!("; its shell couldn't be read ({e:#}), so its old pane id matches no process")
        }
        Some(Ok(None)) => {
            "; herdr reports no shell for its pane, so its old pane id matches no process".into()
        }
        _ => String::new(),
    };
    let warning: String = warnings
        .iter()
        .chain(&aside.warning)
        .map(|w| format!("\nwarning: {w}"))
        .collect();
    Ok(format!(
        "restarted {name} in session {live}; prompted it to read {}{unread}{warning}",
        read.display()
    ))
}

/// Why `name` can't be restarted without `--force` yet, if anything: it must
/// have reported a handoff since its last start, stopped finished with nothing
/// in flight since, and show idle or done in herdr.
fn unfinished(state: &Path, name: &str, status: AgentStatus) -> Result<Option<String>> {
    Ok(match since_handoff(&state.join("reports"), name)? {
        SinceHandoff::None => Some(format!(
            "{name} hasn't handed off since it started (after a revive, check `loopherd reports --all {name}` for its handoff)"
        )),
        SinceHandoff::NotStopped => Some(format!("{name} hasn't stopped since its handoff")),
        SinceHandoff::Stopped(stop) if stop.error.is_some() || stop.pending().is_some() => {
            Some(format!(
                "{name} isn't finished: its last stop reads `{}`",
                stop.describe()
            ))
        }
        SinceHandoff::Stopped(_) if !matches!(status, AgentStatus::Idle | AgentStatus::Done) => {
            Some(format!("{name} is {} in herdr", status.as_str()))
        }
        SinceHandoff::Stopped(_) => None,
    })
}

/// A restart that sent nothing, for a reason a waiter acts on.
#[derive(Debug)]
enum Refusal {
    /// Another restart of the member holds its lock.
    Busy(String),
    NoHandoff(String),
    /// Blocked, unfinished or missing in herdr: may pass on a later try.
    NotReady(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let (Refusal::Busy(why) | Refusal::NoHandoff(why) | Refusal::NotReady(why)) = self;
        f.write_str(why)
    }
}

impl std::error::Error for Refusal {}

#[derive(Debug)]
pub enum Waited {
    Restarted,
    /// Another restart got there first.
    RestartedElsewhere,
    /// Left a note in the member's mailbox naming why.
    GaveUp,
}

/// A self-restart's waiter: polls every `every` until `req.name` is finished
/// since its handoff, then runs the restart with every non-forced check. Gives
/// up after `give_up`, on a failed restart, or on any error, with a note in
/// its mailbox; exits quietly once another restart has used the handoff.
pub fn wait_then_restart(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &RestartRequest,
    give_up: Duration,
    every: Duration,
) -> Result<Waited> {
    let req = RestartRequest {
        force: false,
        own_pane: None,
        own_lineage: &[],
        ..*req
    };
    match poll_then_restart(herdr, ledger, &req, give_up, every) {
        Ok(Ok(waited)) => Ok(waited),
        Ok(Err(why)) => gave_up(&req, why),
        Err(e) => gave_up(&req, format!("self-restart failed: {e:#}")),
    }
}

/// `Ok(Err(why))` when the waiter gives up for `why`.
fn poll_then_restart(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &RestartRequest,
    give_up: Duration,
    every: Duration,
) -> Result<Result<Waited, String>> {
    let name = req.name;
    let waiting_handoff = || handoff::find(req.state, name);
    let deadline = Instant::now() + give_up;
    // Room for a running restart to settle before the waiter stops on it.
    let busy_deadline = deadline + 2 * req.settle_timeout;
    let mut saw_busy = false;
    let gave_up = |waiting: &str| {
        format!(
            "self-restart gave up after {}: {waiting}. Its handoff is still in place; run loopherd restart {name} once it's finished.",
            humantime::format_duration(give_up)
        )
    };
    loop {
        if saw_busy {
            // Locked, so the handoff check can't race the other restart's rename.
            match take(req.state, name, "restart")? {
                Some(_free) if waiting_handoff().is_some() => {
                    return Ok(Err(format!(
                        "self-restart stopped: another restart of {name} ended without using its handoff, \
                         and may have sent /clear. Check its pane, then run loopherd restart {name} or \
                         send the resume prompt it printed."
                    )));
                }
                Some(_free) => return Ok(Ok(Waited::RestartedElsewhere)),
                None if Instant::now() >= busy_deadline => {
                    return Ok(Err(gave_up(&format!(
                        "a restart of {name} is already running"
                    ))));
                }
                None => {
                    std::thread::sleep(every);
                    continue;
                }
            }
        }
        // Reports before the handoff: a restart sets the handoff aside before
        // its new session reports a start, so a start then reads as a revive.
        let since = since_handoff(&req.state.join("reports"), name)?;
        let Some(handoff) = waiting_handoff() else {
            return Ok(Ok(Waited::RestartedElsewhere));
        };
        if since == SinceHandoff::None {
            return Ok(Err(format!(
                "self-restart stopped: {name} started again since its handoff (a revive?) and its handoff is \
                 still at {}. Check `loopherd reports --all {name}`; restarting it now takes --force.",
                handoff.display()
            )));
        }
        let waiting = match waiting_on(herdr, req) {
            Ok(Some(why)) => why,
            Ok(None) => match restart(herdr, ledger, req, SystemTime::now()) {
                Ok(restarted) => {
                    // Nobody reads a waiter's output, so its warnings go to the mailbox.
                    if restarted.contains("\nwarning: ") {
                        let note = Entry::new(
                            SystemTime::now(),
                            Body::Note {
                                text: format!("self-restart: {restarted}"),
                            },
                        );
                        // The restart is done; a lost note mustn't read as a failed one.
                        let _ = append(&req.state.join("reports"), name, &note);
                    }
                    return Ok(Ok(Waited::Restarted));
                }
                Err(e) => match e.downcast_ref::<Refusal>() {
                    Some(Refusal::Busy(_)) => {
                        saw_busy = true;
                        continue;
                    }
                    Some(Refusal::NoHandoff(_)) => return Ok(Ok(Waited::RestartedElsewhere)),
                    Some(Refusal::NotReady(why)) => why.clone(),
                    None => return Ok(Err(format!("self-restart failed: {e:#}"))),
                },
            },
            Err(e) => format!("{e:#}"),
        };
        if Instant::now() >= deadline {
            return Ok(Err(gave_up(&waiting)));
        }
        std::thread::sleep(every);
    }
}

fn waiting_on(herdr: &dyn Herdr, req: &RestartRequest) -> Result<Option<String>> {
    let name = req.name;
    let Some(agent) = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
    else {
        return Ok(Some(format!("no live agent is named {name}")));
    };
    if agent.status == AgentStatus::Blocked {
        return Ok(Some(format!("{name} is blocked in pane {}", agent.pane_id)));
    }
    unfinished(req.state, name, agent.status)
}

fn gave_up(req: &RestartRequest, text: String) -> Result<Waited> {
    let note = Entry::new(SystemTime::now(), Body::Note { text });
    append(&req.state.join("reports"), req.name, &note)?;
    Ok(Waited::GaveUp)
}

/// A self-restart claimed for a member, held until its waiter exits.
pub struct Scheduled {
    pub name: String,
    pub waiter: Lock,
    pub message: String,
}

/// How long a self-restart waits for the member's `finished` stop.
pub const SELF_RESTART_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Claims a self-restart for the member in `own_pane`, which must have a
/// handoff file and a `handoff` report since its last start. Only one waiter
/// per member holds the claim; the caller hands `waiter` to it.
pub fn schedule_self(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    own_pane: Option<&str>,
    own_lineage: &[Process],
    state: &Path,
) -> Result<Scheduled> {
    let pane = own_pane.ok_or_else(|| {
        anyhow!(
            "loopherd restart --self runs only in a member's own pane ($HERDR_PANE_ID is unset)"
        )
    })?;
    let entries = ledger.entries()?;
    let member = member_of_own_pane(&entries, pane, own_lineage, herdr.socket().as_deref())
        .ok_or_else(|| {
            anyhow!(
                "no member runs in pane {pane} of this herdr, and no shepherd handoff is recorded for this pane and Claude"
            )
        })?;
    schedule_member(member, state)
}

/// Claims a self-restart for `member`, as [`schedule_self`] does once it has
/// found the member in its own pane.
pub fn schedule_member(member: &crate::ledger::AgentRecord, state: &Path) -> Result<Scheduled> {
    let name = member.name.clone();
    if !member.is_lead() {
        bail!("{name} isn't a lead; whoever reads its handoff line restarts it");
    }
    if handoff::find(state, &name).is_none() {
        bail!(
            "{name} has no handoff at {}",
            handoff::path(state, &name).display()
        );
    }
    match since_handoff(&state.join("reports"), &name)? {
        SinceHandoff::None => bail!(
            "{name} hasn't reported --kind handoff since it started; write its handoff, report it, then run this"
        ),
        // Its waiter would take that earlier stop for this turn's end.
        SinceHandoff::Stopped(_) => bail!(
            "{name} already stopped since its handoff; report --kind handoff again, then run this in the same turn"
        ),
        SinceHandoff::NotStopped => {}
    }
    let waiter = take(state, &name, "waiter")?
        .ok_or_else(|| anyhow!("{name} already has a restart waiting for its stop"))?;
    let message = format!(
        "{name} restarts in this pane once its stop after the handoff reads finished and herdr shows it idle or done, \
         giving up after {} minutes with a note in its mailbox. End your turn now.",
        SELF_RESTART_TIMEOUT.as_secs() / 60
    );
    Ok(Scheduled {
        name,
        waiter,
        message,
    })
}

/// Takes over the self-restart claim for `name` that `restart --self` passed
/// down as `fd`, refusing any descriptor that isn't that held lock file.
pub fn adopt_claim(state: &Path, name: &str, fd: std::os::fd::RawFd) -> Result<Lock> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    let not_held = || anyhow!("this process doesn't hold {name}'s self-restart claim");
    // SAFETY: fstat only reads the descriptor's metadata; a closed one fails.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(not_held());
    }
    let path = state.join("restarts").join(format!("{name}.waiter.lock"));
    let lock = std::fs::metadata(&path).map_err(|_| not_held())?;
    #[allow(clippy::unnecessary_cast)] // st_dev is i32 on macOS
    let held = (stat.st_dev as u64, stat.st_ino);
    if (lock.dev(), lock.ino()) != held {
        return Err(not_held());
    }
    // SAFETY: fstat showed `fd` open, and nothing else in this process owns it.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    // Re-locking through the shared open file succeeds only for the holder.
    let claim = Lock::try_take(file)?.ok_or_else(not_held)?;
    // Kept here, but not passed on to the herdr calls the waiter makes.
    // SAFETY: fcntl only sets close-on-exec on a descriptor `claim` owns.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(claim)
}

/// `name`'s `kind` lock under the state dir, or `None` while another holds it.
pub fn take(state: &Path, name: &str, kind: &str) -> Result<Option<Lock>> {
    let dir = state.join("restarts");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.{kind}.lock"));
    let file = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    Lock::try_take(file).with_context(|| format!("lock {}", path.display()))
}

const CLEAR_DIALOG: &str = "/clear acts on the main conversation";

/// A lead stopped its watch to hand off, so it's told to re-arm it. `/clear`
/// dropped its original prompt, so that is given again, then its switches and
/// limit, which the prompt may not place.
fn resume_prompt(lead: Option<&LeadPrompt>, handoff: &Path) -> String {
    let resume = format!(
        "Read your handoff at {}, then continue from it.",
        handoff.display()
    );
    match lead {
        Some(lead) => format!(
            "{}\n\n{resume} Your switches: {}. Your review-round limit: {}. {LEAD_REARM}",
            lead.text,
            lead.switches.lines().replace('\n', ", "),
            lead.review_rounds
        ),
        None => resume,
    }
}

const LEAD_REARM: &str = "Your `loopherd watch` is no longer running: \
    run `loopherd status` and `loopherd reports --all --tail 40` (for a fuller history, \
    `--since <time>` or plain `--all`), then read the skill's Lead section \
    (`loopherd --skill --section lead`, not the whole skill) and re-arm your \
    watch as it says.";

/// `old_live` is the session Claude continued `old` in before the `/clear`; a
/// hook moving the ledger there isn't the clear.
fn wait_for_new_session(
    ledger: &dyn Ledger,
    name: &str,
    old: Option<&str>,
    old_live: Option<String>,
    req: &RestartRequest,
) -> Result<String> {
    let deadline = Instant::now() + req.settle_timeout;
    loop {
        let session = ledger.entries()?.remove(name).and_then(|r| r.session_id);
        if let Some(new) = session
            .as_ref()
            .filter(|s| Some(s.as_str()) != old && Some(*s) != old_live.as_ref())
        {
            return Ok(new.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "sent /clear, but {name}'s ledger session is still {} after {:?}",
                session.as_deref().unwrap_or("unset"),
                req.settle_timeout
            );
        }
        std::thread::sleep(req.poll);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::report::{Background, Body, Entry, Stop, Tally, append};
    use crate::subagent_view::screens::{MAIN_VIEW, SUBAGENT_PICKER, SUBAGENT_PROMPT};
    use std::path::{Path, PathBuf};

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
            session_id: Some("old".into()),
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            outcome: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: Some(FAKE_SOCKET.into()),
            process: None,
            role: None,
            attended: None,
            no_slot: false,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    /// A ledger whose member moves to session `new` once `/clear` was sent,
    /// as loopherd's SessionStart hook does.
    struct Clearing<'a> {
        ledger: MemLedger,
        herdr: &'a FakeHerdr,
        follows: bool,
        clears_before: usize,
        /// Where a hook moves the recorded session as the `/clear` lands,
        /// when no new session follows.
        continued: Option<&'static str>,
        recorded: Option<String>,
        fails_start: bool,
        /// Runs at restart's first ledger read, after a waiter's own checks.
        before_restart: std::cell::Cell<Option<&'a dyn Fn()>>,
    }

    impl Ledger for Clearing<'_> {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            if let Some(hook) = self.before_restart.take() {
                hook();
            }
            let mut entries = self.ledger.entries()?;
            let clears = self
                .herdr
                .prompts()
                .iter()
                .filter(|(_, t)| t == "/clear")
                .count();
            let cleared = clears > self.clears_before;
            let session = match cleared {
                true if self.follows => Some("new"),
                true => self.continued,
                false => None,
            };
            // Leaves alone a session restart has written since.
            let untouched = |r: &AgentRecord| r.session_id == self.recorded;
            if let Some(session) = session {
                for record in entries.values_mut().filter(|r| untouched(r)) {
                    record.session_id = Some(session.into());
                }
            }
            Ok(entries)
        }
        fn record(&mut self, r: AgentRecord) -> Result<()> {
            self.ledger.record(r)
        }
        fn update(
            &mut self,
            n: &str,
            c: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            // Sees the session the hook wrote, as a real ledger's fresh read would.
            if let Some(seen) = self.entries()?.remove(n) {
                self.ledger.record(seen)?;
            }
            self.ledger.update(n, c)
        }
        fn remove_herd(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
        ) -> anyhow::Result<crate::ledger::RemovedHerd> {
            unreachable!()
        }
        fn started(&mut self, n: &str, at: SystemTime, resumed: bool) -> Result<()> {
            if self.fails_start {
                bail!("the ledger is read-only");
            }
            self.ledger.started(n, at, resumed)
        }
    }

    struct World {
        state: tempfile::TempDir,
        herdr: FakeHerdr,
        /// The lead whose sandboxed Claude runs the restart, if one does.
        caller: Option<&'static str>,
    }

    impl World {
        fn new() -> World {
            let state = tempfile::tempdir().unwrap();
            fs::create_dir_all(state.path().join("handoffs/lh-1")).unwrap();
            fs::write(state.path().join("handoffs/lh-1/handoff.md"), "## State\n").unwrap();
            fs::create_dir_all(state.path().join("config/herds")).unwrap();
            fs::write(
                state.path().join("config/herds/h.toml"),
                "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nmerge = true\nreview_rounds = 5\n",
            )
            .unwrap();
            World {
                state,
                herdr: FakeHerdr::new(&[("w1", "loopherd")]).with_agent("lh-1"),
                caller: None,
            }
        }

        /// Keeps lead `name` of `herd` in the config dir, as `lead` does.
        fn kept_lead(&self, name: &str, herd: &str) {
            let dir = self.paths().config_dir().join("sandbox");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("{name}.lead.json")),
                format!("{{\"herd\": \"{herd}\", \"args\": []}}"),
            )
            .unwrap();
        }

        fn stop(&self, shells: u32, error: Option<&str>) {
            let stop = Stop {
                background: Tally::Known(Background {
                    shells,
                    ..Background::default()
                }),
                crons: Tally::Known(0),
                error: error.map(str::to_string),
                context: None,
                limit: None,
                window: None,
                session: None,
            };
            self.put(Body::Stop(stop));
        }

        /// Leaves lh-1's handoff only at the flat path an older release wrote.
        fn flat_handoff(&self) {
            let handoffs = self.state.path().join("handoffs");
            fs::rename(handoffs.join("lh-1/handoff.md"), handoffs.join("lh-1.md")).unwrap();
        }

        fn put(&self, body: Body) {
            self.put_at("2026-11-02T09:00:00Z", body);
        }

        fn put_at(&self, at: &str, body: Body) {
            let at = humantime::parse_rfc3339(at).unwrap();
            append(
                &self.state.path().join("reports"),
                "lh-1",
                &Entry::new(at, body),
            )
            .unwrap();
        }

        /// A finished stop after a handoff report.
        fn handed_off(&self) {
            self.put(Body::Handoff {
                text: "at step 3".into(),
            });
            self.stop(0, None);
        }

        fn restart(&self, force: bool, follows: bool) -> (Result<String>, MemLedger) {
            self.restart_as(force, follows, None, &|_| {})
        }

        fn restart_as(
            &self,
            force: bool,
            follows: bool,
            own_pane: Option<&str>,
            edit: &dyn Fn(&mut AgentRecord),
        ) -> (Result<String>, MemLedger) {
            self.restart_continued(force, follows, own_pane, edit, None, &|s| s.to_string())
        }

        fn restart_continued(
            &self,
            force: bool,
            follows: bool,
            own_pane: Option<&str>,
            edit: &dyn Fn(&mut AgentRecord),
            continued: Option<&'static str>,
            live: &dyn Fn(&str) -> String,
        ) -> (Result<String>, MemLedger) {
            let mut member = record("lh-1");
            member.claude_args = vec![
                "--settings".into(),
                self.settings().display().to_string(),
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                self.settings()
                    .with_extension("mcp.json")
                    .display()
                    .to_string(),
            ];
            edit(&mut member);
            let mut ledger = Clearing {
                recorded: member.session_id.clone(),
                ledger: MemLedger([("lh-1".to_string(), member)].into(), vec![]),
                herdr: &self.herdr,
                follows,
                continued,
                fails_start: false,
                before_restart: Default::default(),
                clears_before: self
                    .herdr
                    .prompts()
                    .iter()
                    .filter(|(_, t)| t == "/clear")
                    .count(),
            };
            let req = RestartRequest {
                own_lineage: &[crate::process::PANE_SHELL],
                pane_shell: &|_| Ok(Some(crate::process::PANE_SHELL)),
                name: "lh-1",
                force,
                own_pane,
                state: self.state.path(),
                paths: &self.paths(),
                caller: self.caller,
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
                live,
            };
            let result = restart(&self.herdr, &mut ledger, &req, SystemTime::UNIX_EPOCH);
            let mut after = ledger.ledger.clone();
            after.0 = ledger.entries().unwrap();
            (result, after)
        }

        /// Runs a self-restart's waiter for lh-1, polling every millisecond.
        fn wait(&self, give_up: Duration) -> Result<Waited> {
            self.wait_with(give_up, false, None)
        }

        fn wait_with(
            &self,
            give_up: Duration,
            fails_start: bool,
            before_restart: Option<&dyn Fn()>,
        ) -> Result<Waited> {
            let member = record("lh-1");
            let mut ledger = Clearing {
                recorded: member.session_id.clone(),
                ledger: MemLedger([("lh-1".to_string(), member)].into(), vec![]),
                herdr: &self.herdr,
                follows: true,
                continued: None,
                clears_before: 0,
                fails_start,
                before_restart: before_restart.into(),
            };
            let req = RestartRequest {
                name: "lh-1",
                force: false,
                own_pane: None,
                own_lineage: &[],
                pane_shell: &|_| Ok(Some(crate::process::PANE_SHELL)),
                state: self.state.path(),
                paths: &self.paths(),
                caller: None,
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
                live: &|s| s.to_string(),
            };
            wait_then_restart(
                &self.herdr,
                &mut ledger,
                &req,
                give_up,
                Duration::from_millis(1),
            )
        }

        fn notes(&self) -> Vec<String> {
            crate::report::history(&self.state.path().join("reports"), "lh-1")
                .unwrap()
                .0
                .into_iter()
                .filter_map(|e| match e.body {
                    Body::Note { text } => Some(text),
                    _ => None,
                })
                .collect()
        }

        /// Writes the human's `~/.claude/settings.json`, returning its path.
        fn user_settings(&self, text: &str) -> PathBuf {
            let dir = self.paths().home.join(".claude");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("settings.json"), text).unwrap();
            dir.join("settings.json")
        }

        /// Where a lead's sandbox settings are written.
        fn settings(&self) -> PathBuf {
            self.state.path().join("config/sandbox/lh-1.json")
        }

        /// Starts lh-1 as herd `h`'s lead would: its record kept and its settings written.
        fn prepared(&self) {
            let lead = sandbox::Lead {
                herd: "h".into(),
                args: vec![],
                path: None,
            };
            sandbox::prepare(&self.paths(), "lh-1", &lead, || {
                Ok(crate::sandbox::Sockets::Blocked)
            })
            .unwrap();
        }

        fn paths(&self) -> Paths {
            let config = self.state.path().join("config");
            Paths {
                herds: config.join("herds"),
                home: self.state.path().join("home"),
                feed_lock: config.join("feed.lock"),
                paused: config.join("paused"),
            }
        }

        fn handoff(&self) -> String {
            self.state
                .path()
                .join("handoffs/lh-1/prev.md")
                .display()
                .to_string()
        }
    }

    #[test]
    fn a_flat_handoff_an_older_release_wrote_is_restarted_from_and_moved_to_prev() {
        let w = World::new();
        let handoffs = w.state.path().join("handoffs");
        fs::rename(handoffs.join("lh-1/handoff.md"), handoffs.join("lh-1.md")).unwrap();
        w.handed_off();

        let (result, _) = w.restart(false, true);

        assert_eq!(
            result.unwrap(),
            format!(
                "restarted lh-1 in session new; prompted it to read {}",
                w.handoff()
            )
        );
        assert_eq!(fs::read_to_string(w.handoff()).unwrap(), "## State\n");
        assert!(!handoffs.join("lh-1.md").exists());
    }

    #[test]
    fn a_finished_member_is_cleared_then_prompted_to_read_its_handoff() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart(false, true);
        assert_eq!(
            result.unwrap(),
            format!(
                "restarted lh-1 in session new; prompted it to read {}",
                w.handoff()
            )
        );
        assert_eq!(
            w.herdr.prompts(),
            [
                ("lh-1".to_string(), "/clear".to_string()),
                (
                    "lh-1".to_string(),
                    format!(
                        "Read your handoff at {}, then continue from it.",
                        w.handoff()
                    )
                ),
            ]
        );
        assert_eq!(ledger.0["lh-1"].session_id.as_deref(), Some("new"));
        assert_eq!(
            (
                ledger.0["lh-1"].pane_id.as_str(),
                ledger.0["lh-1"].tab_id.as_deref()
            ),
            ("w9:p9", Some("w9:t9"))
        );
        assert_eq!(ledger.0["lh-1"].env_pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(
            ledger.0["lh-1"].env_pane_shell,
            Some(crate::process::PANE_SHELL)
        );
        assert_eq!(ledger.1, ["lh-1"]);
    }

    #[test]
    fn a_lead_restarts_a_member_of_its_own_herd() {
        let mut w = World::new();
        w.kept_lead("lead-h", "h");
        w.caller = Some("lead-h");
        w.handed_off();

        let (result, _) = w.restart_as(false, true, None, &|r| r.herd = Some("h".into()));

        result.unwrap();
    }

    #[test]
    fn a_lead_isn_t_told_to_pass_force_which_its_broker_refuses() {
        let mut w = World::new();
        w.kept_lead("lead-h", "h");
        w.caller = Some("lead-h");
        w.handed_off();
        w.herdr.set_status("lh-1", AgentStatus::Working);

        let (result, _) = w.restart_as(false, true, None, &|r| r.herd = Some("h".into()));

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "lh-1 is working in herdr"
        );
    }

    #[test]
    fn a_lead_cant_restart_a_member_of_another_herd_and_nothing_is_cleared() {
        let mut w = World::new();
        w.kept_lead("lead-o", "o");
        w.caller = Some("lead-o");
        w.handed_off();

        let (result, ledger) = w.restart_as(false, true, None, &|r| r.herd = Some("h".into()));

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "refused: lh-1 isn't a member of lead-o's herd, and lead-o runs this loopherd"
        );
        assert!(w.herdr.prompts().is_empty());
        assert!(w.state.path().join("handoffs/lh-1/handoff.md").exists());
        assert_eq!(ledger.0["lh-1"].session_id, record("lh-1").session_id);
    }

    #[test]
    fn a_restart_in_the_recorded_pane_sets_no_env_pane_id() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart_as(false, true, None, &|r| r.pane_id = "w9:p9".into());
        result.unwrap();
        let lh1 = &ledger.0["lh-1"];
        assert_eq!(
            (lh1.pane_id.as_str(), lh1.env_pane_id.as_deref()),
            ("w9:p9", None)
        );
    }

    #[test]
    fn a_restart_keeps_the_pane_id_the_agent_was_started_with() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart_as(false, true, None, &|r| {
            r.env_pane_id = Some("w1:p1".into());
            r.pane_id = "w3:p2".into();
        });
        result.unwrap();
        let lh1 = &ledger.0["lh-1"];
        assert_eq!(
            (lh1.pane_id.as_str(), lh1.env_pane_id.as_deref()),
            ("w9:p9", Some("w1:p1"))
        );
    }

    fn continued(session: &str) -> String {
        match session {
            "stub" => "old".into(),
            "new" => "newer".into(),
            other => other.into(),
        }
    }

    #[test]
    fn a_cleared_session_that_continued_is_restarted_in_and_recorded_as_the_live_one() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart_continued(false, true, None, &|_| {}, None, &continued);
        assert!(
            result
                .unwrap()
                .starts_with("restarted lh-1 in session newer;"),
        );
        assert_eq!(ledger.0["lh-1"].session_id.as_deref(), Some("newer"));
    }

    #[test]
    fn a_clear_that_continues_the_old_session_into_the_new_one_is_confirmed() {
        let w = World::new();
        w.handed_off();
        let cleared = || w.herdr.prompts().iter().any(|(_, t)| t == "/clear");
        let live = |s: &str| match s {
            "old" if cleared() => "new".to_string(),
            other => other.to_string(),
        };
        let (result, _) = w.restart_continued(false, true, None, &|_| {}, None, &live);
        assert!(
            result
                .unwrap()
                .starts_with("restarted lh-1 in session new;")
        );
    }

    #[test]
    fn a_recorded_stub_moving_to_its_live_session_is_not_taken_for_the_clear() {
        let w = World::new();
        w.handed_off();
        let stub = |m: &mut AgentRecord| m.session_id = Some("stub".into());
        let (result, _) = w.restart_continued(false, false, None, &stub, Some("old"), &continued);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("ledger session is still old after"),
            "{error}"
        );
        assert_eq!(
            w.herdr.prompts(),
            [("lh-1".to_string(), "/clear".to_string())]
        );
    }

    #[test]
    fn a_clear_that_never_lands_names_the_session_the_ledger_still_records() {
        let w = World::new();
        w.handed_off();
        let stub = |m: &mut AgentRecord| m.session_id = Some("stub".into());
        let (result, _) = w.restart_continued(false, false, None, &stub, None, &continued);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("ledger session is still stub after"),
            "{error}"
        );
    }

    #[test]
    fn a_member_not_finished_since_its_handoff_is_refused_without_force_and_sent_nothing() {
        let handoff = || Body::Handoff { text: "x".into() };
        type Setup<'a> = &'a dyn Fn(&World);
        let cases: [(Setup, &str); 7] = [
            (
                &|w| {
                    w.put(handoff());
                    w.stop(1, None);
                },
                "lh-1 isn't finished: its last stop reads `paused: 1 shell`",
            ),
            (
                &|w| {
                    w.put(handoff());
                    w.stop(0, Some("rate_limit"));
                },
                "lh-1 isn't finished: its last stop reads `error rate_limit; finished`",
            ),
            (
                &|w| {
                    w.stop(0, None);
                    w.put(handoff());
                },
                "lh-1 hasn't stopped since its handoff",
            ),
            (
                &|w| {
                    w.handed_off();
                    w.put(Body::Start { resumed: false });
                    w.stop(0, None);
                },
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|w| w.stop(0, None),
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|_| {},
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|w| {
                    w.handed_off();
                    w.herdr.set_status("lh-1", AgentStatus::Working);
                },
                "lh-1 is working in herdr",
            ),
        ];
        for (setup, message) in cases {
            let w = World::new();
            setup(&w);
            let (result, _) = w.restart(false, true);
            assert_eq!(
                format!("{:#}", result.unwrap_err()),
                format!("{message}; pass --force to restart it anyway")
            );
            assert!(w.herdr.prompts().is_empty());

            let (result, _) = w.restart(true, true);
            assert!(result.is_ok(), "{message}");
        }
    }

    #[test]
    fn a_member_it_cant_follow_or_its_own_pane_is_refused_even_with_force() {
        type Edit<'a> = &'a dyn Fn(&mut AgentRecord);
        let cases: [(Option<&str>, Edit, &str); 4] = [
            (
                None,
                &|r| r.socket = None,
                "lh-1 was recorded without a herdr socket",
            ),
            (
                None,
                &|r| r.socket = Some("/run/other.sock".into()),
                "lh-1 is in the herdr at /run/other.sock, not this one",
            ),
            (Some("w1:p1"), &|_| {}, "lh-1 can't restart itself"),
            (
                Some("w1:p1"),
                &|r| {
                    r.env_pane_id = Some("w1:p1".into());
                    r.env_pane_shell = Some(crate::process::PANE_SHELL);
                    r.pane_id = "w3:p2".into();
                },
                "lh-1 can't restart itself",
            ),
        ];
        for (own_pane, edit, message) in cases {
            let w = World::new();
            w.handed_off();
            let (result, _) = w.restart_as(true, true, own_pane, edit);
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.starts_with(message), "{err}");
            assert!(w.herdr.prompts().is_empty());
        }
    }

    #[test]
    fn a_stranger_with_a_moved_members_old_pane_id_under_another_shell_restarts_it() {
        let w = World::new();
        w.handed_off();
        let moved_away = |r: &mut AgentRecord| {
            r.env_pane_id = Some("w1:p9".into());
            r.env_pane_shell = Some(Process {
                pid: 20,
                start: 910,
            });
        };

        let (result, _) = w.restart_as(true, true, Some("w1:p9"), &moved_away);

        result.unwrap();
        assert!(!w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_handoff_is_used_once_even_by_a_forced_restart() {
        let w = World::new();
        w.handed_off();
        assert!(w.restart(false, true).0.is_ok());
        let (again, _) = w.restart(true, true);
        assert!(format!("{:#}", again.unwrap_err()).starts_with("lh-1 has no handoff at "),);
        assert_eq!(w.herdr.prompts().len(), 2);
    }

    /// The member's Stop hook asks for a handoff in `session`, past its limit.
    fn asked(w: &World, session: &str) -> String {
        let transcript = w.state.path().join("t.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":1,"cache_read_input_tokens":200}}}"#,
        )
        .unwrap();
        let stop = format!(
            r#"{{"hook_event_name":"Stop","session_id":"{session}","transcript_path":"{}"}}"#,
            transcript.display()
        );
        crate::context::handoff_block(
            &record("lh-1"),
            stop.as_bytes(),
            &crate::context::fixed_limit(100),
            w.state.path(),
            &|_| Ok(()),
        )
        .reason
        .unwrap()
        .expect("asked")
    }

    fn handoff_written_at(w: &World, at: &str) {
        fs::File::options()
            .write(true)
            .open(w.state.path().join("handoffs/lh-1/handoff.md"))
            .unwrap()
            .set_modified(humantime::parse_rfc3339(at).unwrap())
            .unwrap();
    }

    #[test]
    fn a_handoff_written_before_the_context_ask_is_still_there_to_restart_from() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        asked(&w, "old");
        w.handed_off();

        let (result, _) = w.restart(false, true);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fs::read_to_string(w.handoff()).unwrap(), "## State\n");
    }

    #[test]
    fn a_handoff_written_before_a_revive_is_still_there_to_restart_from() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        w.put_at("2026-11-02T08:45:00Z", Body::Start { resumed: true });
        asked(&w, "old");
        w.handed_off();

        let (result, _) = w.restart(false, true);

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn after_a_restart_that_timed_out_the_next_ask_moves_the_consumed_handoff() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        w.handed_off();
        let (timed_out, _) = w.restart(false, false);
        assert!(timed_out.is_err());

        let reason = asked(&w, "new");
        w.put_at(
            "2026-11-02T10:00:00Z",
            Body::Handoff {
                text: "again".into(),
            },
        );
        w.stop(0, None);
        let (result, _) = w.restart(false, true);

        assert!(reason.contains("write a new one"), "{reason}");
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 has no handoff at "),);
    }

    #[test]
    fn without_a_handoff_file_nothing_is_sent() {
        let w = World::new();
        w.stop(0, None);
        fs::remove_file(w.state.path().join("handoffs/lh-1/handoff.md")).unwrap();
        let (result, _) = w.restart(true, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 has no handoff at "),);
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_session_that_never_moves_fails_before_the_handoff_prompt_and_keeps_the_handoff() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart(false, false);
        let md = w
            .state
            .path()
            .join("handoffs/lh-1/handoff.md")
            .display()
            .to_string();
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sent /clear, but lh-1's ledger session is still old after 50ms: the /clear may be \
                 queued behind a running turn, or loopherd's SessionStart hook isn't installed. No \
                 handoff prompt was sent and the handoff is still at {md}. Once lh-1's pane shows a \
                 cleared session, run: loopherd tell 'lh-1' 'Read your handoff at {md}, then \
                 continue from it.'"
            )
        );
        assert_eq!(w.herdr.prompts().len(), 1);
        assert!(ledger.1.is_empty());

        let (retry, _) = w.restart(false, true);
        assert!(retry.is_ok(), "{retry:?}");
    }

    #[test]
    fn a_leads_resume_prompt_has_it_re_arm_its_watch_and_a_workers_doesnt() {
        let rearm = "Your `loopherd watch` is no longer running: run `loopherd status` and \
                     `loopherd reports --all --tail 40` (for a fuller history, `--since <time>` \
                     or plain `--all`), then read the skill's Lead section \
                     (`loopherd --skill --section lead`, not the whole skill) and re-arm \
                     your watch as it says.";
        for (lead, wants) in [(true, true), (false, false)] {
            let w = World::new();
            w.handed_off();
            let (result, _) = w.restart_as(false, true, None, &|r| {
                r.role = lead.then_some(crate::ledger::Role::Lead);
                r.herd = Some("h".into());
            });
            result.unwrap();
            let resume = &w.herdr.prompts()[1].1;
            assert!(
                resume.contains(&format!(
                    "Read your handoff at {}, then continue from it.",
                    w.handoff()
                )),
                "{resume}"
            );
            assert_eq!(resume.contains(rearm), wants, "{resume}");
        }
    }

    #[test]
    fn a_leads_resume_prompt_carries_its_resolved_switches_and_review_round_limit() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.with_file_name("config.toml"),
            "[lead_defaults]\nnew_cards = true\n",
        )
        .unwrap();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("h".into());
        });
        result.unwrap();
        let resume = &w.herdr.prompts()[1].1;
        assert!(
            resume.contains(
                "Your switches: merge on, install off, rebase off, new_cards on, \
                 design_escalation off, blocked_dialogs off, follow_ups_ready off. Your review-round limit: 5."
            ),
            "{resume}"
        );
    }

    #[test]
    fn a_leads_resume_prompt_re_renders_its_prompt_file_ahead_of_the_handoff_line() {
        let w = World::new();
        w.handed_off();
        let herds = w.paths().herds;
        fs::write(
            herds.join("lead.md"),
            "{name} leads {herd} in {worktree}.\n\
             Merge with GH_TOKEN=$(gh auth token --user me) gh pr merge.\n\
             {switches}\n",
        )
        .unwrap();
        fs::write(
            herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt_file = \"lead.md\"\nmerge = true\nreview_rounds = 5\n",
        )
        .unwrap();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("h".into());
            r.worktree = Some("/src/worktrees/lead-h".into());
        });
        result.unwrap();
        let resume = &w.herdr.prompts()[1].1;
        let prompt = "lh-1 leads h in /src/worktrees/lead-h.\n\
                      Merge with GH_TOKEN=$(gh auth token --user me) gh pr merge.\n\
                      merge on\ninstall off\nrebase off\nnew_cards off\n\
                      design_escalation off\nblocked_dialogs off\nfollow_ups_ready off\n\n\
                      Your review-round limit: 5.";
        assert!(
            resume.starts_with(&format!(
                "{prompt}\n\nRead your handoff at {}, then continue from it.",
                w.handoff()
            )),
            "{resume}"
        );
    }

    fn as_lead(r: &mut AgentRecord) {
        r.role = Some(crate::ledger::Role::Lead);
        r.herd = Some("h".into());
        r.worktree = Some("/src/worktrees/lh-1".into());
    }

    /// lh-1 as a worker of herd `h`, whose `[worker]` is `worker`, kept by
    /// its sandbox; `launched` says whether its Claude got its settings.
    fn as_worker(w: &World, worker: &str, launched: bool) -> impl Fn(&mut AgentRecord) {
        fs::write(
            w.paths().herds.join("h.toml"),
            format!(
                "repo = \"/src/repo\"\n[template.t]\nparams = []\nprompt = \"go\"\n\
                 [worker]\n{worker}\n"
            ),
        )
        .unwrap();
        let dir = w.paths().config_dir().join("sandbox");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("lh-1.member.json"),
            r#"{"herd": "h", "template": "t", "args": []}"#,
        )
        .unwrap();
        let settings = w.settings().display().to_string();
        move |r: &mut AgentRecord| {
            r.herd = Some("h".into());
            r.template = Some("t".into());
            r.claude_args = match launched {
                true => vec![
                    "--settings".into(),
                    settings.clone(),
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    "w1.mcp.json".into(),
                ],
                false => vec![],
            };
        }
    }

    /// A worker running in its sandbox, kept with `args`, in a worktree whose
    /// local settings hold `local`.
    fn as_worker_in_worktree(
        w: &World,
        worker: &str,
        args: &[&str],
        local: &str,
    ) -> impl Fn(&mut AgentRecord) {
        let launched = as_worker(w, worker, true);
        let worktree = w.paths().herds.parent().unwrap().join("wt-lh-1");
        fs::create_dir_all(worktree.join(".claude")).unwrap();
        fs::write(worktree.join(".claude/settings.local.json"), local).unwrap();
        let dir = w.paths().config_dir().join("sandbox");
        fs::write(
            dir.join("lh-1.member.json"),
            serde_json::json!({
                "herd": "h",
                "template": "t",
                "args": args,
                "worktree": worktree,
            })
            .to_string(),
        )
        .unwrap();
        launched
    }

    #[test]
    fn a_required_worker_whose_kept_args_or_settings_loosen_its_sandbox_is_refused() {
        let w = World::new();
        w.handed_off();
        let worker =
            as_worker_in_worktree(&w, "sandbox = \"require\"", &["--add-dir", "/tmp"], "{}");

        let (result, _) = w.restart_as(false, true, None, &worker);

        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains(
                "lh-1 can't run sandboxed: its args may loosen its sandbox or permissions: \
                 --add-dir /tmp, from the args it was started with;"
            ),
            "{err}"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_required_worker_whose_worktree_settings_loosen_its_sandbox_is_refused() {
        let w = World::new();
        w.handed_off();
        let local = r#"{"permissions": {"allow": ["Write"]}}"#;
        let worker = as_worker_in_worktree(&w, "sandbox = \"require\"", &[], local);

        let (result, _) = w.restart_as(false, true, None, &worker);

        let file = (w.paths().herds.parent().unwrap()).join("wt-lh-1/.claude/settings.local.json");
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sandbox: lh-1 can't run sandboxed: your Claude settings loosen its sandbox: {}: \
                 permissions.allow opens Write to writes; remove those, or set [worker] sandbox \
                 = \"prefer\" in herd h to run it with them as warnings",
                file.display()
            )
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_worker_running_sandboxed_without_its_mcp_lock_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        let launched = as_worker(&w, "sandbox = \"require\"", true);
        let worker = |r: &mut AgentRecord| {
            launched(r);
            r.claude_args.retain(|a| a != "--strict-mcp-config");
        };

        let (result, _) = w.restart_as(false, true, None, &worker);

        let out = result.unwrap();
        assert!(
            out.ends_with(
                "\nwarning: lh-1 was started with every MCP server you configure, not only its \
                 herd's [worker] mcp_servers; quit its Claude and run loopherd revive --name lh-1"
            ),
            "{out}"
        );
        assert!(!out.contains("unsandboxed"), "{out}");
    }

    #[test]
    fn a_preferred_worker_whose_worktree_settings_loosen_its_sandbox_is_restarted_with_warnings() {
        let w = World::new();
        w.handed_off();
        let local = r#"{"permissions": {"allow": ["Write"]}}"#;
        let worker =
            as_worker_in_worktree(&w, "sandbox = \"prefer\"", &["--add-dir", "/tmp"], local);

        let (result, _) = w.restart_as(false, true, None, &worker);

        let out = result.unwrap();
        let worktree = w.paths().herds.parent().unwrap().join("wt-lh-1");
        let file = worktree.join(".claude/settings.local.json");
        assert!(
            out.contains(&format!(
                "warning: lh-1's sandbox is loosened by {}: permissions.allow opens Write to \
                 writes",
                file.display()
            )),
            "{out}"
        );
        assert!(
            out.contains(
                "warning: lh-1 starts with an arg that may loosen its sandbox or permissions: \
                 --add-dir /tmp, from the args it was started with"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_worker_running_in_its_sandbox_is_restarted() {
        let w = World::new();
        w.handed_off();
        let worker = as_worker(&w, "sandbox = \"require\"", true);

        let (result, _) = w.restart_as(false, true, None, &worker);

        assert!(!result.unwrap().contains("warning"));
        assert!(!w.settings().exists(), "restart writes no settings");
    }

    #[test]
    fn a_required_worker_started_without_its_sandbox_is_refused_and_nothing_is_cleared() {
        let w = World::new();
        w.handed_off();
        let worker = as_worker(&w, "sandbox = \"require\"", false);

        let (result, _) = w.restart_as(false, true, None, &worker);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't run sandboxed: it was started without its sandbox; quit its \
             Claude and run loopherd revive --name lh-1 to resume it sandboxed, or set [worker] \
             sandbox = \"prefer\" in herd h to run it unsandboxed"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_sandboxed_worker_whose_herd_is_gone_is_refused_naming_the_herd() {
        let w = World::new();
        w.handed_off();
        let worker = as_worker(&w, "sandbox = \"require\"", true);
        fs::remove_file(w.paths().herds.join("h.toml")).unwrap();

        let (result, _) = w.restart_as(false, true, None, &worker);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 was started in herd h's worker sandbox, and herd h is gone; restore \
             its herd file, or run loopherd retire lh-1"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_preferred_worker_started_without_its_sandbox_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        let worker = as_worker(&w, "sandbox = \"prefer\"", false);

        let (result, _) = w.restart_as(false, true, None, &worker);

        assert!(
            result.as_ref().unwrap().ends_with(
                "\nwarning: lh-1 runs unsandboxed (it was started without its sandbox)\n\
                     warning: lh-1 was started with every MCP server you configure, not only its \
                     herd's [worker] mcp_servers; quit its Claude and run loopherd revive --name \
                     lh-1"
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_credentials_herds_worker_started_without_its_sandbox_is_refused_even_under_prefer() {
        let w = World::new();
        w.handed_off();
        let worker = as_worker(
            &w,
            "sandbox = \"prefer\"\n[lead]\nprompt = \"go\"\nmerge_guard = \"credentials\"",
            false,
        );

        let (result, _) = w.restart_as(false, true, None, &worker);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't run sandboxed: it was started without its sandbox; quit its \
             Claude and run loopherd revive --name lh-1 to resume it sandboxed, or set [lead] \
             merge_guard = \"hook\" in herd h to run it unsandboxed with your GitHub \
             credentials"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_running_in_its_sandbox_is_restarted() {
        let w = World::new();
        w.handed_off();
        w.prepared();

        let (result, ledger) = w.restart_as(false, true, None, &as_lead);

        assert!(!result.unwrap().contains("warning"));
        assert_eq!(ledger.0["lh-1"].unsandboxed, None);
    }

    /// Plants the zero-byte, read-only files Claude's sandbox leaves in a
    /// lead worktree's `.claude/` for protected paths that didn't exist.
    fn plant_sandbox_placeholders(worktree: &Path) -> Vec<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let dir = worktree.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        [
            "settings.json",
            "settings.local.json",
            "agents",
            "hooks",
            "launch.json",
        ]
        .iter()
        .map(|name| {
            let file = dir.join(name);
            fs::write(&file, "").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
            file
        })
        .collect()
    }

    #[test]
    fn a_lead_whose_worktree_holds_sandbox_placeholders_is_restarted_and_they_stay() {
        let w = World::new();
        w.handed_off();
        let repo = w.state.path().join("repo");
        fs::write(
            w.paths().herds.join("h.toml"),
            format!("repo = \"{}\"\n[lead]\nprompt = \"go\"\n", repo.display()),
        )
        .unwrap();
        w.prepared();
        let worktree = crate::worktree::worktree_path(&repo, "lh-1");
        let placeholders = plant_sandbox_placeholders(&worktree);

        let (result, _) = w.restart_as(false, true, None, &|r| {
            as_lead(r);
            r.worktree = Some(worktree.clone());
        });

        let out = result.unwrap();
        assert!(!out.contains("warning"), "{out}");
        for file in placeholders {
            assert_eq!(fs::read(&file).unwrap(), b"", "{} stays", file.display());
        }
    }

    #[test]
    fn a_lead_whose_herd_switches_changed_since_it_started_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\ninstall = true\n",
        )
        .unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert!(
            result.as_ref().unwrap().ends_with(
                "\nwarning: lh-1's sandbox was set before herd h's current switches, which reach \
                 it only when its Claude starts again; quit its Claude and run loopherd revive \
                 --name lh-1"
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_lead_s_writable_state_dirs_exist_when_it_is_cleared() {
        let mut w = World::new();
        w.handed_off();
        w.prepared();
        let state = w.paths().state_dir().to_path_buf();
        fs::remove_dir_all(state.join("broker")).unwrap();
        let dirs = ["broker/lh-1/requests", "handoffs/lh-1"].map(|d| state.join(d));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let record = (seen.clone(), dirs.clone());
        w.herdr.on_prompt = Some(Box::new(move |text| {
            let (seen, dirs) = &record;
            if text == "/clear" {
                *seen.lock().unwrap() = dirs.iter().map(|d| d.is_dir()).collect();
            }
        }));

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        result.unwrap();
        assert_eq!(*seen.lock().unwrap(), [true, true], "{dirs:?}");
    }

    #[test]
    fn a_lead_s_writable_state_dir_that_can_t_be_made_refuses_the_restart_naming_it() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        let broker = w.paths().state_dir().join("broker");
        fs::remove_dir_all(&broker).unwrap();
        fs::write(&broker, "").unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains(&broker.join("lh-1/requests").display().to_string()),
            "{error}"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn another_lead_starting_doesn_t_tell_a_running_lead_its_sandbox_is_stale() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        fs::create_dir_all(w.paths().state_dir().join("broker/lead-g/requests")).unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert!(!result.as_ref().unwrap().contains("warning"), "{result:?}");
    }

    #[test]
    fn a_lead_started_with_all_of_broker_denied_is_told_its_sandbox_is_stale() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        let mut settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(w.settings()).unwrap()).unwrap();
        let blanket = format!("Edit(/{}/broker/**)", w.paths().state_dir().display());
        settings["permissions"]["deny"]
            .as_array_mut()
            .unwrap()
            .push(blanket.into());
        fs::write(w.settings(), settings.to_string()).unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert!(
            result
                .as_ref()
                .unwrap()
                .contains("lh-1's sandbox was set before herd h's current switches"),
            "{result:?}"
        );
    }

    #[test]
    fn a_lead_the_ledger_calls_a_worker_is_still_checked_as_the_lead_loopherd_kept() {
        let w = World::new();
        w.handed_off();
        w.prepared();

        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.claude_args = vec![];
        });

        assert!(
            format!("{:#}", result.unwrap_err()).starts_with(
                "sandbox: lh-1 can't run sandboxed: it was started without its sandbox;"
            )
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_whose_user_settings_loosen_its_sandbox_is_not_cleared_under_require() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        let settings = w.user_settings(r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sandbox: lh-1 can't run sandboxed: your Claude settings loosen its sandbox: \
                 {}: sandbox.excludedCommands runs \"docker *\" outside it; remove those, or set \
                 [lead] sandbox = \"prefer\" in herd h to start it with them as warnings",
                settings.display()
            )
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_on_prefer_whose_user_settings_loosen_its_sandbox_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
        )
        .unwrap();
        w.prepared();
        let settings = w.user_settings(r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);

        let (result, ledger) = w.restart_as(false, true, None, &as_lead);

        let out = result.unwrap();
        assert!(
            out.ends_with(&format!(
                "\nwarning: lh-1's sandbox is loosened by {}: sandbox.excludedCommands runs \
                 \"docker *\" outside it",
                settings.display()
            )),
            "{out}"
        );
        assert_eq!(ledger.0["lh-1"].unsandboxed, None);
    }

    #[test]
    fn a_lead_started_with_an_arg_outside_the_allowlist_is_not_cleared_under_require() {
        let w = World::new();
        w.handed_off();
        let herd = w.paths().herds.join("h.toml");
        let require = fs::read_to_string(&herd).unwrap();
        fs::write(&herd, format!("{require}sandbox = \"prefer\"\n")).unwrap();
        let lead = sandbox::Lead {
            herd: "h".into(),
            args: vec!["--add-dir".into(), "/srv".into()],
            path: None,
        };
        sandbox::prepare(&w.paths(), "lh-1", &lead, || {
            Ok(crate::sandbox::Sockets::Blocked)
        })
        .unwrap();
        fs::write(&herd, require).unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert!(format!("{:#}", result.unwrap_err()).starts_with(
            "sandbox: lh-1 can't run sandboxed: its args may loosen its sandbox or \
                 permissions: --add-dir /srv, from the args it was started with; a lead takes \
                 only --model, --effort, --verbose and --permission-mode default, manual, auto or plan, \
                 so retire it and start it again with loopherd lead h, without those,"
        ));
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_on_prefer_started_with_an_arg_outside_the_allowlist_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
        )
        .unwrap();
        let lead = sandbox::Lead {
            herd: "h".into(),
            args: vec!["--permission-mode".into(), "bypassPermissions".into()],
            path: None,
        };
        sandbox::prepare(&w.paths(), "lh-1", &lead, || {
            Ok(crate::sandbox::Sockets::Blocked)
        })
        .unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        let out = result.unwrap();
        assert!(
            out.contains(
                "\nwarning: lh-1 starts with an arg that may loosen its sandbox or permissions: \
                 --permission-mode bypassPermissions, from the args it was started with"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_lead_kept_with_loopherds_own_mcp_flags_from_before_is_cleared_under_require() {
        let w = World::new();
        w.handed_off();
        w.prepared();
        fs::write(
            w.paths().config_dir().join("sandbox/lh-1.lead.json"),
            r#"{"herd": "h", "args": ["--mcp-config", "a.json", "--strict-mcp-config",
                "--model", "opus"]}"#,
        )
        .unwrap();

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn a_lead_started_with_every_mcp_server_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        w.prepared();

        let (result, _) = w.restart_as(false, true, None, &|r| {
            as_lead(r);
            r.claude_args.truncate(2);
        });

        let out = result.unwrap();
        assert!(
            out.ends_with(
                "\nwarning: lh-1 was started with every MCP server you configure, not only its \
                 herd's [lead] mcp_servers; quit its Claude and run loopherd revive --name lh-1"
            ),
            "{out}"
        );
    }

    fn unsandboxed(r: &mut AgentRecord) {
        as_lead(r);
        r.claude_args = vec!["--model".into(), "opus".into()];
    }

    #[test]
    fn a_lead_running_without_its_sandbox_is_not_cleared_under_require() {
        let w = World::new();
        w.handed_off();

        let (result, _) = w.restart_as(false, true, None, &unsandboxed);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't run sandboxed: it was started without its sandbox; quit its \
             Claude and run loopherd revive --name lh-1 to resume it sandboxed, or set [lead] \
             sandbox = \"prefer\" in herd h to run it guarded by its tool hook alone"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_on_prefer_running_without_its_sandbox_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
        )
        .unwrap();

        let (result, ledger) = w.restart_as(false, true, None, &unsandboxed);

        let out = result.unwrap();
        assert!(
            out.ends_with(
                "\nwarning: lh-1 was started with every MCP server you configure, not only its \
                 herd's [lead] mcp_servers; quit its Claude and run loopherd revive --name lh-1\n\
                 warning: lh-1 runs without its sandbox (it was started without its sandbox), \
                 guarded by its tool hook alone"
            ),
            "{out}"
        );
        assert_eq!(
            ledger.0["lh-1"].unsandboxed.as_deref(),
            Some("it was started without its sandbox")
        );
    }

    #[test]
    fn a_lead_on_prefer_keeps_its_recorded_reason_for_running_without_its_sandbox() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n",
        )
        .unwrap();

        let (result, ledger) = w.restart_as(false, true, None, &|r| {
            unsandboxed(r);
            r.unsandboxed = Some("bubblewrap not installed".into());
        });

        let out = result.unwrap();
        assert!(
            out.ends_with(
                "\nwarning: lh-1 runs without its sandbox (bubblewrap not installed), \
                 guarded by its tool hook alone"
            ),
            "{out}"
        );
        assert_eq!(
            ledger.0["lh-1"].unsandboxed.as_deref(),
            Some("bubblewrap not installed")
        );
    }

    #[test]
    fn a_credentials_lead_running_without_its_sandbox_is_not_cleared_even_under_prefer() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nsandbox = \"prefer\"\n\
             merge_guard = \"credentials\"\n",
        )
        .unwrap();

        let (result, _) = w.restart_as(false, true, None, &unsandboxed);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't start with merge_guard = \"credentials\": it was started \
             without its sandbox, so it can read your GitHub credentials; quit its Claude and \
             run loopherd revive --name lh-1 to resume it sandboxed"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    fn sockets_unblocked(r: &mut AgentRecord) {
        as_lead(r);
        r.sockets_unblocked = Some("Claude Code finds no seccomp filter".into());
    }

    fn herd_file(w: &World, lead: &str) {
        fs::write(
            w.paths().herds.join("h.toml"),
            format!("repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\n{lead}"),
        )
        .unwrap();
    }

    #[test]
    fn a_lead_started_with_sockets_open_is_not_cleared_under_require() {
        let w = World::new();
        w.handed_off();
        w.prepared();

        let (result, _) = w.restart_as(false, true, None, &sockets_unblocked);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't run sandboxed: it was started with a sandbox that can't block \
             Unix sockets (Claude Code finds no seccomp filter); quit its Claude and run \
             loopherd revive --name lh-1 to resume it with them blocked, or set [lead] sandbox = \
             \"prefer\" in herd h to run it with them open"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_credentials_lead_started_with_sockets_open_is_not_cleared_even_under_prefer() {
        let w = World::new();
        w.handed_off();
        herd_file(&w, "sandbox = \"prefer\"\n");
        w.prepared();
        herd_file(&w, "sandbox = \"prefer\"\nmerge_guard = \"credentials\"\n");

        let (result, _) = w.restart_as(false, true, None, &sockets_unblocked);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't start with merge_guard = \"credentials\": it was started with \
             a sandbox that can't block Unix sockets (Claude Code finds no seccomp filter), so \
             it could use your ssh agent; quit its Claude and run loopherd revive --name lh-1 \
             to resume it with them blocked, or set [lead] merge_guard = \"hook\" in herd h"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_whose_herd_now_pairs_credentials_with_every_socket_open_is_not_cleared() {
        let w = World::new();
        w.handed_off();
        herd_file(&w, "");
        w.prepared();
        herd_file(
            &w,
            "merge_guard = \"credentials\"\nunix_sockets = \"all\"\n\
             [worker]\nsandbox = \"require\"\n",
        );

        let (result, _) = w.restart_as(false, true, None, &as_lead);

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sandbox: lh-1 can't start with merge_guard = \"credentials\": unix_sockets = \
             \"all\" opens your ssh agent, Git Credential Manager through WSL2 interop and the \
             Secret Service keyring to it, each of which acts as you; run loopherd herd unset h \
             lead.unix_sockets, or loopherd herd set h lead.merge_guard '\"hook\"'"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_on_prefer_started_with_sockets_open_is_restarted_with_a_warning() {
        let w = World::new();
        w.handed_off();
        herd_file(&w, "sandbox = \"prefer\"\n");
        w.prepared();

        let (result, ledger) = w.restart_as(false, true, None, &sockets_unblocked);

        let out = result.unwrap();
        assert!(
            out.ends_with(
                "\nwarning: lh-1's sandbox can't block Unix sockets (Claude Code finds no \
                 seccomp filter), so herdr, your ssh agent and WSL2's Windows interop stay open \
                 to it; fix the sandbox check's cause, then quit its Claude and run loopherd \
                 revive --name lh-1 to resume it with them blocked"
            ),
            "{out}"
        );
        assert_eq!(
            ledger.0["lh-1"].sockets_unblocked.as_deref(),
            Some("Claude Code finds no seccomp filter")
        );
    }

    #[test]
    fn a_lead_started_with_sockets_open_is_restarted_under_require_when_its_herd_opens_them() {
        let w = World::new();
        w.handed_off();
        herd_file(&w, "unix_sockets = \"all\"\n");
        w.prepared();

        let (result, _) = w.restart_as(false, true, None, &sockets_unblocked);

        let out = result.unwrap();
        assert!(!out.contains("can't block Unix sockets"), "{out}");
    }

    #[test]
    fn a_lead_whose_prompt_file_is_gone_is_not_cleared() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.join("h.toml"),
            "repo = \"/src/repo\"\n[lead]\nprompt_file = \"gone.md\"\n",
        )
        .unwrap();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("h".into());
        });
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("gone.md"), "{err}");
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_lead_whose_herd_file_cant_be_read_is_not_cleared() {
        let w = World::new();
        w.handed_off();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("gone".into());
        });
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("lh-1's prompt"), "{err}");
        assert!(w.herdr.prompts().is_empty());
    }

    const PANE: &str = "w9:p9";

    const MAIN_VIEW_QUOTING_A_SUBAGENT_VIEW: &str = "\
● Bash(cat src/restart.rs)
  ⎿  ────────────────────────────────────────
     ❯ Message @reviewer…
     ────────────────────────────────────────
       ◯ main
       ● local_agent · Review PR #97 …
● Next up:
  ◯ main loop rewrite
  ● local_agent cleanup

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ? for shortcuts
";

    const SUBAGENT_ROUNDED_BOX: &str = "\
● Reviewing PR #97 against the card.

╭──────────────────────────────────────╮
│ ❯ Message @reviewer…                 │
╰──────────────────────────────────────╯
  ← 1 agent
";

    const SUBAGENT_LABELLED_RULE: &str = "\
● Reviewing PR #97 against the card.

──────────────── reviewer ──────────────
❯ Message @reviewer…
────────────────────────────────────────
  ← 1 agent
";

    const SUBAGENT_PICKER_NO_RULES: &str = "\
● Reviewing PR #97 against the card.

❯ 
  ◯ main
  ● local_agent · Review PR #97 …
";

    const MAIN_VIEW_WITH_A_STATUS_BULLET: &str = "\
● Handoff written; finished.

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ● 1 background task
  ⏵⏵ auto mode on · ← 1 agent
  ◯ local_agent · Review PR #97 …
  ● main
";

    const BOXED_WRAPPED_CLEAR_DIALOG: &str = "\
╭──────────────────────────╮
│ /clear acts on the main  │
│ conversation, not this   │
│ agent                    │
│                          │
│ ❯ Cancel                 │
╰──────────────────────────╯
";

    const WRAPPED_CLEAR_DIALOG: &str = "\
 /clear acts on the main
 conversation, not this agent

 ❯ Cancel
   Run /clear on the main
   conversation
";

    const CLEAR_DIALOG: &str = "\
 /clear acts on the main conversation, not this agent

 ❯ Cancel
   Run /clear on the main conversation
";

    #[test]
    fn a_pane_showing_a_subagent_view_is_refused_even_with_force_and_sent_nothing() {
        for screen in [
            SUBAGENT_PROMPT,
            SUBAGENT_PICKER,
            SUBAGENT_ROUNDED_BOX,
            SUBAGENT_LABELLED_RULE,
            SUBAGENT_PICKER_NO_RULES,
        ] {
            for force in [false, true] {
                let w = World::new();
                w.handed_off();
                w.herdr.set_screen(PANE, screen);
                let (result, ledger) = w.restart(force, true);
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    "lh-1's pane w9:p9 is showing a subagent's view, and /clear there doesn't reach \
                     the main conversation: switch the pane back to the main conversation, then run \
                     loopherd restart lh-1 again"
                );
                assert!(w.herdr.prompts().is_empty());
                assert!(w.state.path().join("handoffs/lh-1/handoff.md").is_file());
                assert!(ledger.1.is_empty());
            }
        }
    }

    #[test]
    fn a_pane_showing_the_main_conversation_restarts() {
        for screen in [
            MAIN_VIEW,
            MAIN_VIEW_QUOTING_A_SUBAGENT_VIEW,
            MAIN_VIEW_WITH_A_STATUS_BULLET,
        ] {
            let w = World::new();
            w.handed_off();
            w.herdr.set_screen(PANE, screen);
            let (result, _) = w.restart(false, true);
            assert!(result.is_ok(), "{screen}: {result:?}");
            assert_eq!(w.herdr.prompts().len(), 2);
        }
    }

    #[test]
    fn a_clear_dialog_in_the_pane_after_a_timeout_is_named() {
        for dialog in [
            CLEAR_DIALOG,
            WRAPPED_CLEAR_DIALOG,
            BOXED_WRAPPED_CLEAR_DIALOG,
        ] {
            a_clear_dialog_after_a_timeout_is_named(dialog);
        }
    }

    fn a_clear_dialog_after_a_timeout_is_named(dialog: &str) {
        let w = World::new();
        w.handed_off();
        w.herdr.set_screen_on_prompt(PANE, dialog);
        let (result, _) = w.restart(false, false);
        let md = w
            .state
            .path()
            .join("handoffs/lh-1/handoff.md")
            .display()
            .to_string();
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sent /clear, but lh-1's ledger session is still old after 50ms: lh-1's pane w9:p9 \
                 shows Claude's \"/clear acts on the main conversation\" dialog, so it was in a \
                 subagent's view. No handoff prompt was sent and the handoff is still at {md}. Pick \
                 Cancel, switch the pane back to the main conversation, then run loopherd restart \
                 lh-1 again."
            )
        );
        assert_eq!(w.herdr.prompts().len(), 1);
    }

    #[test]
    fn a_blocked_or_missing_agent_is_refused() {
        let w = World::new();
        w.handed_off();
        w.herdr.set_status("lh-1", AgentStatus::Blocked);
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 is blocked"));
        w.herdr.remove_agent("lh-1");
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("no live agent is named lh-1"));
        assert!(w.herdr.prompts().is_empty());
    }

    fn schedule(w: &World, own_pane: Option<&str>) -> Result<Scheduled> {
        let mut lead = record("lh-1");
        lead.role = Some(crate::ledger::Role::Lead);
        lead.herd = Some("h".into());
        let ledger = MemLedger([("lh-1".to_string(), lead)].into(), vec![]);
        schedule_self(&w.herdr, &ledger, own_pane, &[], w.state.path())
    }

    #[test]
    fn a_member_that_reported_its_handoff_schedules_its_own_restart() {
        let w = World::new();
        w.put(Body::Handoff {
            text: "at step 3".into(),
        });
        let scheduled = schedule(&w, Some("w1:p1")).unwrap();
        assert_eq!(scheduled.name, "lh-1");
        assert!(
            scheduled
                .message
                .starts_with("lh-1 restarts in this pane once"),
            "{}",
            scheduled.message
        );
    }

    #[test]
    fn self_restart_is_refused_with_nothing_scheduled() {
        type Setup<'a> = &'a dyn Fn(&World);
        let cases: [(Option<&str>, Setup, &str); 5] = [
            (
                None,
                &|w| w.put(Body::Handoff { text: "x".into() }),
                "loopherd restart --self runs only in a member's own pane",
            ),
            (
                Some("w7:p7"),
                &|w| w.put(Body::Handoff { text: "x".into() }),
                "no member runs in pane w7:p7",
            ),
            (
                Some("w1:p1"),
                &|w| {
                    w.put(Body::Handoff { text: "x".into() });
                    fs::remove_file(w.state.path().join("handoffs/lh-1/handoff.md")).unwrap();
                },
                "lh-1 has no handoff at ",
            ),
            (
                Some("w1:p1"),
                &|_| {},
                "lh-1 hasn't reported --kind handoff since it started",
            ),
            (
                Some("w1:p1"),
                &|w| {
                    w.put(Body::Handoff { text: "x".into() });
                    w.put(Body::Start { resumed: false });
                },
                "lh-1 hasn't reported --kind handoff since it started",
            ),
        ];
        for (own_pane, setup, message) in cases {
            let w = World::new();
            setup(&w);
            let err = format!("{:#}", schedule(&w, own_pane).err().expect(message));
            assert!(err.starts_with(message), "{err}");
            w.put(Body::Handoff { text: "x".into() });
            if fs::write(
                w.state.path().join("handoffs/lh-1/handoff.md"),
                "## State\n",
            )
            .is_ok()
            {
                assert!(
                    schedule(&w, Some("w1:p1")).is_ok(),
                    "{message}: a waiter was left behind"
                );
            }
        }
    }

    #[test]
    fn a_flat_handoff_schedules_a_self_restart_whose_waiter_resumes_from_it() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        w.flat_handoff();
        let scheduled = schedule(&w, Some("w1:p1")).unwrap();
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                w.stop(0, None);
            });
            w.wait(Duration::from_secs(5))
        });
        drop(scheduled);
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        let handoffs = w.state.path().join("handoffs");
        assert!(w.herdr.prompts()[1].1.contains("handoffs/lh-1/prev.md"));
        assert_eq!(
            fs::read_to_string(handoffs.join("lh-1/prev.md")).unwrap(),
            "## State\n"
        );
        assert!(!handoffs.join("lh-1.md").exists());
    }

    #[test]
    fn a_self_restart_notes_the_older_handoff_it_couldnt_move_aside() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let handoffs = w.state.path().join("handoffs");
        fs::write(handoffs.join("lh-1.md"), "older").unwrap();
        fs::File::options()
            .write(true)
            .open(handoffs.join("lh-1.md"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        fs::create_dir_all(handoffs.join("lh-1.prev.md/x")).unwrap();
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                w.stop(0, None);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        let notes = w.notes();
        let [note] = notes.as_slice() else {
            panic!("{notes:?}");
        };
        let unmoved = format!(
            "\nwarning: couldn't move the older {} to {}",
            handoffs.join("lh-1.md").display(),
            handoffs.join("lh-1.prev.md").display()
        );
        assert!(
            note.starts_with("self-restart: restarted lh-1 in session "),
            "{note}"
        );
        assert!(note.contains(&unmoved), "{note}");
    }

    #[test]
    fn a_second_self_restart_is_refused_while_the_first_waits() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let first = schedule(&w, Some("w1:p1")).unwrap();
        let err = format!("{:#}", schedule(&w, Some("w1:p1")).err().unwrap());
        assert_eq!(err, "lh-1 already has a restart waiting for its stop");
        drop(first);
        assert!(schedule(&w, Some("w1:p1")).is_ok());
    }

    #[test]
    fn a_waiting_self_restart_restarts_once_the_stop_after_its_handoff_is_finished() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                assert!(w.herdr.prompts().is_empty(), "restarted before its stop");
                w.stop(0, None);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        let prompts = w.herdr.prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].1, "/clear");
        assert!(w.notes().is_empty());
    }

    #[test]
    fn a_self_restart_waits_for_a_stop_with_nothing_in_flight_and_herdr_idle() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        w.stop(1, None);
        w.herdr.set_status("lh-1", AgentStatus::Working);
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                w.stop(0, None);
                std::thread::sleep(Duration::from_millis(20));
                assert!(w.herdr.prompts().is_empty(), "restarted while working");
                w.herdr.set_status("lh-1", AgentStatus::Idle);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
    }

    #[test]
    fn a_self_restart_that_times_out_leaves_the_handoff_and_a_note_naming_why() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        w.stop(1, None);
        let waited = w.wait(Duration::from_millis(30)).unwrap();
        assert!(matches!(waited, Waited::GaveUp));
        assert!(w.herdr.prompts().is_empty());
        assert!(w.state.path().join("handoffs/lh-1/handoff.md").is_file());
        assert_eq!(
            w.notes(),
            [
                "self-restart gave up after 30ms: lh-1 isn't finished: its last stop reads \
              `paused: 1 shell`. Its handoff is still in place; run loopherd restart lh-1 once it's finished."
            ]
        );
    }

    #[test]
    fn a_self_restart_overtaken_by_another_restart_exits_without_clearing() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                fs::rename(
                    w.state.path().join("handoffs/lh-1/handoff.md"),
                    w.state.path().join("handoffs/lh-1/prev.md"),
                )
                .unwrap();
                w.put(Body::Start { resumed: false });
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::RestartedElsewhere));
        assert!(w.herdr.prompts().is_empty());
        assert!(w.notes().is_empty());
    }

    #[test]
    fn a_revive_while_a_self_restart_waits_ends_it_with_a_note() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                w.put(Body::Start { resumed: true });
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::GaveUp));
        assert!(w.herdr.prompts().is_empty());
        let notes = w.notes();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("self-restart stopped: lh-1 started again since its handoff"),
            "{notes:?}"
        );
    }

    #[test]
    fn a_self_restart_is_refused_once_the_member_stopped_since_its_handoff() {
        let w = World::new();
        w.handed_off();
        let err = format!("{:#}", schedule(&w, Some("w1:p1")).err().unwrap());
        assert!(
            err.starts_with("lh-1 already stopped since its handoff"),
            "{err}"
        );
    }

    #[test]
    fn a_restart_refuses_while_another_restart_of_the_same_member_runs() {
        let w = World::new();
        w.handed_off();
        let _running = take(w.state.path(), "lh-1", "restart").unwrap().unwrap();
        let (result, _) = w.restart(true, true);
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "a restart of lh-1 is already running"
        );
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_self_restart_that_saw_another_restart_leave_the_handoff_stops_with_a_note() {
        let w = World::new();
        w.handed_off();
        let running = take(w.state.path(), "lh-1", "restart").unwrap().unwrap();
        let (reached, restarting) = std::sync::mpsc::channel();
        let reached_restart = move || reached.send(()).unwrap();
        let waited = std::thread::scope(|s| {
            s.spawn(move || {
                // Timed from the waiter's own restart, so a slow first poll
                // can't outlast the running restart and take the lock itself.
                restarting.recv().unwrap();
                std::thread::sleep(Duration::from_millis(500));
                drop(running);
            });
            w.wait_with(Duration::from_secs(5), false, Some(&reached_restart))
        });
        assert!(matches!(waited.unwrap(), Waited::GaveUp));
        assert!(w.herdr.prompts().is_empty());
        let notes = w.notes();
        assert!(
            notes[0].starts_with(
                "self-restart stopped: another restart of lh-1 ended without using its handoff"
            ),
            "{notes:?}"
        );
    }

    #[test]
    fn a_self_restart_stops_waiting_on_a_restart_that_never_ends() {
        let w = World::new();
        w.handed_off();
        let _running = take(w.state.path(), "lh-1", "restart").unwrap().unwrap();
        let started = Instant::now();
        let waited = w.wait(Duration::from_millis(10)).unwrap();
        assert!(matches!(waited, Waited::GaveUp));
        assert!(started.elapsed() < Duration::from_secs(2));
        let notes = w.notes();
        assert!(
            notes[0].starts_with(
                "self-restart gave up after 10ms: a restart of lh-1 is already running"
            ),
            "{notes:?}"
        );
    }

    #[test]
    fn a_self_restart_whose_restart_fails_after_using_the_handoff_leaves_a_note() {
        let w = World::new();
        w.handed_off();
        let waited = w.wait_with(Duration::from_secs(5), true, None).unwrap();
        assert!(matches!(waited, Waited::GaveUp));
        assert!(!w.state.path().join("handoffs/lh-1/handoff.md").exists());
        assert_eq!(w.notes(), ["self-restart failed: the ledger is read-only"]);
    }

    #[test]
    fn a_self_restart_keeps_waiting_when_the_restart_finds_the_member_busy_again() {
        let w = World::new();
        w.handed_off();
        let woken = || w.herdr.set_status("lh-1", AgentStatus::Working);
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                assert!(w.herdr.prompts().is_empty(), "restarted while working");
                w.herdr.set_status("lh-1", AgentStatus::Idle);
            });
            w.wait_with(Duration::from_secs(5), false, Some(&woken))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        assert!(w.notes().is_empty());
    }

    #[test]
    fn only_a_lead_schedules_its_own_restart() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let ledger = MemLedger([("lh-1".to_string(), record("lh-1"))].into(), vec![]);
        let err = format!(
            "{:#}",
            schedule_self(&w.herdr, &ledger, Some("w1:p1"), &[], w.state.path())
                .err()
                .unwrap()
        );
        assert_eq!(
            err,
            "lh-1 isn't a lead; whoever reads its handoff line restarts it"
        );
    }

    #[test]
    fn a_waiter_adopts_only_the_claim_its_self_restart_took() {
        use std::os::fd::IntoRawFd;
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let scheduled = schedule(&w, Some("w1:p1")).unwrap();
        let shared = scheduled.waiter.try_clone().unwrap().into_raw_fd();
        assert!(adopt_claim(w.state.path(), "lh-1", shared).is_ok());
        let other = fs::File::create(w.state.path().join("other"))
            .unwrap()
            .into_raw_fd();
        let err = format!(
            "{:#}",
            adopt_claim(w.state.path(), "lh-1", other).unwrap_err()
        );
        assert!(
            err.starts_with("this process doesn't hold lh-1's self-restart claim"),
            "{err}"
        );
    }

    #[test]
    fn the_own_pane_refusal_points_only_a_lead_at_self() {
        let w = World::new();
        w.handed_off();
        let (worker, _) = w.restart_as(false, true, Some("w1:p1"), &|_| {});
        assert_eq!(
            format!("{:#}", worker.unwrap_err()),
            "lh-1 can't restart itself; whoever reads its handoff line restarts it"
        );
        let (lead, _) = w.restart_as(false, true, Some("w1:p1"), &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("h".into());
        });
        assert_eq!(
            format!("{:#}", lead.unwrap_err()),
            "lh-1 can't restart itself mid-turn; run `loopherd restart --self` to restart once this turn ends"
        );
    }
}
