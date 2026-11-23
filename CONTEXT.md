# loopherd

loopherd groups coding agents into herds and keeps them working together across
sessions, while the human directs the work by talking to a single agent.

## People and agents

**Human**:
The person who owns the work. They set the goals and limits, answer questions,
and merge, and they talk only to the shepherd.

**Shepherd**:
The agent the human talks to. It looks after every herd and passes questions and
merge requests up to the human and the human's answers back down. It also does
the wayfinding and grilling that need the human in the room. For a herd
without a lead, it also does the lead's job. Past its context limit it writes its
own handoff and asks the human to clear the session and continue from it.
_Avoid_: orchestrator, manager

**Lead**:
The member that runs one herd on the shepherd's behalf. It feeds the herd,
watches its workers, verifies their work, restarts workers that hand off, and
closes out tasks after they merge. It acts within the switches the human sets and
asks the human, through the shepherd, for anything switched off. The shepherd
restarts it when it hands off.
_Avoid_: tech lead, supervisor

**Broker**:
A process loopherd starts beside each lead, outside its sandbox, that runs the
lead's loopherd commands that need herdr and refuses anything outside the lead's
own herd.
_Avoid_: proxy, daemon

**Member**:
An agent session that loopherd started and tracks, each with its own terminal
pane, worktree and branch. A member is either a worker or a lead. It usually
belongs to a herd but doesn't have to.
_Avoid_: agent (too broad)

**Worker**:
A member doing one task from its herd's queue. It reports to its herd's lead, or
to the shepherd if the herd has no lead.

**Subagent**:
A short-lived helper an agent launches inside its own session, such as a builder,
reviewer or researcher. A subagent is not a member: it has no pane of its own, and
loopherd only lists a member's running ones, from the starts and stops in its
mailbox.

## Herds and work

**Herd**:
A group of members working toward one goal from one queue. A herd file defines
it, and it may or may not have a lead.

**Herd file**:
The definition of a herd: the repo it works in, its queue, its templates, the
most workers it runs at once, and its lead's settings.

**Queue**:
Where a herd's tasks come from: a command that lists the tasks ready to work, so
any issue tracker can serve as one. loopherd's own herd reads cards from a Trello
board.

**Task**:
One piece of work from a queue, such as a Trello card.
_Avoid_: job, ticket

**Wayfinding**:
Work that settles direction rather than building something: research, a
prototype, a recorded decision, or grilling. Its output is a decision or a
document, not a feature.

**Grilling**:
Questioning the human about a plan until the open questions are settled. A
grill, an attended member in its own pane, runs it live, one question at a
time or as one batch the human accepts or overrides; a batch grill can also
run unattended and post its batch on the card. The shepherd starts and closes
out a grill but never grills in its own pane. Grilling always needs the
human's answers.

**Template**:
The pattern that turns a task into a worker: its name, its branch, its prompt,
and the checks for when its work is merged and when it is done.

**Claimable**:
A task the queue offers that the next feed would turn into a worker if there is
room.

**Feeding**:
Turning claimable tasks into workers, up to the herd's limit, after first
retiring workers whose tasks are done. The lead feeds its own herd; the shepherd
feeds herds that have no lead.

**Verify**:
Checking a worker's output against its task before it goes to the human to
merge.

**Switches**:
The permissions the human gives a lead: merge, install, rebase, new cards, design
escalation, blocked dialogs and follow-ups ready. Anything switched off becomes a question to the
human.

## Communication

**Mailbox**:
Each member's record of what it has said and what has happened to it: its
reports, its stops, the subagents it starts and finishes, herdr seeing it
blocked or gone, and the tokens it spent, recorded when it retires. Messages go up only, to
the lead or the shepherd. Nothing is deleted; each reader keeps its place
instead, so the history survives handoffs and retirement. Messages down to a
member are typed into its session, not mailed.
_Avoid_: inbox

**Report**:
A message a member leaves in its mailbox. There are four kinds: done (work ready,
usually a pull request), question (needs an answer), note (information only) and
handoff.

**Stop**:
A member ending its turn. A stop is paused if work is still running in the
background, finished if nothing is, and failed if it ended on an error.

**Watch**:
The background wait that wakes the shepherd or a lead when a member reports,
stops, gets blocked or disappears. The shepherd watches leads and the members of
herds without a lead; a lead watches its own workers.

**Blocked**:
A member waiting on a dialog that only the human can answer.

## Sessions

**Context limit**:
How much conversation a session may hold before it must hand off, set by the
human.

**Handoff**:
The note a session writes once it passes its context limit, covering its state,
its next steps and its gotchas, so a fresh session can carry on from it.

**Restart**:
Giving a member that has handed off a fresh session in the same pane, worktree
and branch, starting from its handoff.

**Retire**:
Marking a member finished. Closing it as well removes its pane, worktree and
branch once its work is merged.

**Revive**:
Bringing members back after herdr itself restarts.

**Ledger**:
loopherd's record of every member, live or retired: where it runs and which
session it is.

**herdr**:
The terminal multiplexer that the agents run in. loopherd is a layer over it.
