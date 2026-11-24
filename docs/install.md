# Installing loopherd

The [README](../README.md#install) has the one-line install. This page has
the rest: both install paths, their flags, updating, `loopherd setup`,
uninstalling, and loopherd's herdr plugin.

## Requirements

- Linux or macOS, x86_64 or aarch64.
- [herdr](https://herdr.dev/), running.
- [Claude Code](https://code.claude.com/docs/en/overview) (`claude`).
- On Linux and WSL2, leads also need [bubblewrap and
  socat](https://code.claude.com/docs/en/sandboxing#set-up-linux-and-wsl2)
  for the sandbox they run in.

Both install paths put `~/.local/bin/loopherd` in place (put `~/.local/bin`
on `PATH`), add loopherd's Claude hooks and skill, run `loopherd setup`, and
finish with `loopherd doctor`, whose failures (say, herdr not started yet)
are printed but don't fail the install.

## From a release

The latest release, with no clone and no Rust. Also needs `curl` (or
`wget`), `tar`, and `sha256sum` or `shasum`; the offers below also need
`bash` and [`jq`](https://jqlang.github.io/jq/download/).

```bash
curl -fsSL https://junderdo.dev/loopherd/install.sh | sh
```

It picks the build for your OS and arch and checks it against the
release's `SHA256SUMS`. To pin a release:

```bash
curl -fsSL https://junderdo.dev/loopherd/install.sh | LOOPHERD_VERSION=v0.2.0 sh
```

The junderdo.dev script fetches and runs the release's own installer, served
as `https://github.com/junderdo/loopherd/releases/latest/download/install.sh`.

Like `scripts/install`, the installer plays the splash and, asking on the
terminal, offers the agent types, the `mattpocock-skills` plugin, the herdr
plugin and its key bindings (each default no); without a terminal it skips
each and names its flag. It takes `--no-splash` (or `LOOPHERD_NO_SPLASH=1`),
`--with-agents`, `--with-skills`, `--with-herdr-plugin`, `--with-herdr-keys`
and `--no-extras`, meaning what they mean to `scripts/install`. The
junderdo.dev script passes on only `LOOPHERD_VERSION`, so give flags to the
GitHub copy, after `sh -s --`:

```bash
curl -fsSL https://github.com/junderdo/loopherd/releases/latest/download/install.sh | sh -s -- --no-splash --with-agents
```

**Update:** `loopherd update` (`--check` to only look, `--version vX.Y.Z`
to pick a release, older ones included).

## From source

For contributors, or for the latest `main`. Also needs `git`, a
[Rust toolchain](https://rustup.rs/) and
[`jq`](https://jqlang.github.io/jq/download/).

```bash
git clone https://github.com/junderdo/loopherd.git
cd loopherd
scripts/install
```

On a terminal it also offers loopherd's agent types (`builder`,
`reviewer`, `researcher`, `scout`), the `mattpocock-skills` plugin and
loopherd's herdr plugin (below), each default no. `--with-agents`,
`--with-skills` and `--with-herdr-plugin` install one without asking;
`scripts/install --help` lists its flags.

**Update:** `loopherd update` (`--check` to only look), or
`git pull && scripts/install --update` by hand.

## `loopherd setup`

`loopherd setup` asks for `config.toml`'s machine-wide settings while
they're unset: what a lead may do by default, the handoff limits, then
whether herdr's startup hook also resumes members (alive leads' brokers
start either way). The context window comes from each session's model; set
`context_window` only to override it. Token counts take `250k` or `1.5M`,
and a handoff limit may be a percentage of the window (`65%`). Run it again
with `--again`.

## What the hooks do

The hooks act only in a member's pane: they record each stop and each
subagent it starts or finishes in its mailbox, keep its session current
across `/clear` and resume, and deny a lead the commands its switches leave
to you. `install-hook` backs up `settings.json` before changing it.

The guide a shepherd or lead follows is the loopherd skill: `loopherd
--skill` prints it (`--section lead` prints one section), and `install-hook`
installs it for Claude: the file in `~/.agents/skills/loopherd/`, and
`~/.claude/skills/loopherd` a relative symlink to it, as the `skills` CLI
lays out a skill.

## Uninstall

```bash
loopherd install-hook --remove      # add --settings <file> if you installed with one
rm ~/.local/bin/loopherd
```

Agent types from `scripts/install` are yours to remove: the files listed in
`$XDG_STATE_HOME/loopherd/agents.sha256`, then that file.

## herdr plugin

Optional, once loopherd is installed. Needs herdr 0.7.5 or later. A source
install asks "Install loopherd's herdr plugin?" when herdr is on `PATH` and
the plugin is missing or older than the checkout's, and `scripts/install
--with-herdr-plugin` installs or updates it without asking. Updating needs
herdr 0.9.4; on an older herdr, or when an update leaves the version where
it was, the installer prints the reinstall to run by hand. Or by hand:

```bash
herdr plugin install junderdo/loopherd/herdr-plugin
```

It adds a `loopherd: herd status` action and a `loopherd: open herd view`
action, which opens `loopherd view` in a popup over most of the screen;
`q` or Escape quits the view and closes the popup.

### Key bindings

Once the plugin is installed, a source install asks "Add herdr key
bindings for loopherd's plugin?", and `scripts/install --with-herdr-keys`
(or `loopherd install-herdr-keys`) adds them without asking: `prefix+m`
opens the view and `prefix+shift+m` opens it beside the shepherd. Both are
unbound in herdr 0.9.5's defaults (`herdr --default-config`). It writes them
as `[[keys.command]]` entries at the end of the `config.toml` herdr reads,
after backing it up beside itself, and reloads a running server. A key your
config or herdr's defaults already hold, or an action you already bound, is
skipped with a line saying why. herdr's keybinds help (`prefix+?`) lists
them under "custom" by their descriptions. `loopherd install-herdr-keys
--remove` takes out only those entries, matched by command and description,
so a binding you wrote by hand from the snippet below, or an earlier
README's, stays. `status` gets no key: its output goes to herdr's plugin
log, not the screen.

Or bind it by hand and run `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "loopherd.view"
description = "herd view"
```

A popup that closes as soon as it opens means the view couldn't start; run
`loopherd view` in a pane to see why.

### View beside the shepherd

`loopherd: open view beside shepherd` (`loopherd.view-beside-shepherd`)
splits the view into a pane beside the shepherd's, the one its last
handoff recorded, leaving focus where it was. With no shepherd recorded,
or its pane gone or no longer the shepherd's, it splits beside the
focused pane (the popup when that fails too), and the view says why. Bind
it the same way, with its own key (install uses `prefix+shift+m`).

`view.layout` in `config.toml` sets the side, for this action and the
view's `s` alike: `side-by-side`, the default, puts the shepherd left and
the view right; `stacked` puts the view on top and the shepherd below.
The view takes 40% of the split. `v` in the view switches a shepherd in
the view's tab to the other layout, moving its pane without restarting
it, and saves the choice as `view.layout`.

```sh
loopherd config set view.layout stacked
```

### Events and PR links

The plugin's event hooks record a member going blocked, or its pane
exiting or closing, or its tab or workspace closing, so the next
`loopherd watch` reports it even when none was running. They follow a
member's pane moved to another tab or workspace.

A Ctrl-click on a GitHub PR link in any pane shows the member that owns
the PR in a popup (herd, live status, template, review round, last
reports; q or Esc closes it). A PR no open member owns opens in the
browser. Clicks reach the plugin only while herdr's `ui.mouse_capture`
is on; with it off, the terminal handles them.

The plugin runs the `loopherd` on `PATH`, else `~/.local/bin/loopherd`, and
never installs loopherd or its hooks; without one, its commands fail with a
pointer to the README's install section. herdr shows no action output on
screen: read it, errors included, with `herdr plugin log list --plugin
loopherd`.

### Startup revive

After herdr restores a session, the plugin's startup hook starts a broker
for each alive lead recorded on that herdr server. It does this whatever
the config says. To have it also run the full `loopherd revive`
(resuming members herdr didn't bring back and prompting leads to re-arm
their watch), answer yes when `loopherd setup` asks, or run
`loopherd config set herdr_plugin.revive_on_startup true`. That writes:

```toml
[herdr_plugin]
revive_on_startup = true
```

The full revive is off without that key. herdr shows nothing of its run,
so the full revive leaves a `note` in each lead's mailbox instead, which
the shepherd's watch shows.

Remove the plugin with `herdr plugin uninstall loopherd`.
