# Findings: teammate spawns silently die at `respawn-pane` (2026-07-29)

Local investigation notes, written to survive until this is forked and turned
into an upstream PR. Working tree currently holds the fix, uncommitted, on
`main` at `9469ab2` (v2.2.0).

Three independent bugs. Bug 1 breaks teammate spawning. Bug 2 is pre-existing and
unrelated to the shim — it breaks `spawn`/`msg` verification for anyone on
current herdr. Bug 3 was found while verifying the other two, is **not fixed
here**, and is the one to report first: messaging a blocked worker silently
approves whatever permission prompt it was waiting on.

---

## Environment

| | |
|---|---|
| OS | CachyOS, kernel `7.1.5-1-cachyos`, x86-64 |
| Login shell | fish 4.8.1 — **with greeting enabled** and a multi-segment prompt (kubectl context, AWS region, GCP account, clock) |
| herdr | 0.7.5, channel `stable`, socket protocol 17, binary `~/.local/bin/herdr` |
| herdmates | 2.2.0 (this repo) |
| Claude Code | 2.1.220, `teammateMode: "tmux"`, `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` |
| Launch path | `~/.dotfiles/fish/functions/claude.fish` wrapper → `herdmates teammux-launch` when `$HERDR_PANE_ID` is set |

The fish greeting plus the prompt's cloud/VCS segments is what makes this
reproduce every time here. A bare `sh` prompt would narrow the window enough to
look flaky or absent — likely why upstream hasn't hit it.

---

## Bug 1 — `pane run` races the freshly split pane's shell

### Symptom

Spawning a teammate reports success. No teammate ever appears.

- `herdr pane list` shows the new pane, correctly labelled (e.g. `scout`), but
  `terminal_title` stays `~/some/dir - fish` and `agent` is `null`.
- No `claude --agent-id <name>` process exists.
- `~/.claude/teams/<team>/inboxes/<name>.json` holds the task with
  `"read": false` forever — nobody consumed it.
- The pane sits at an ordinary interactive fish prompt.
- Reading the pane's scrollback shows the launch command line printed *above*
  fish's greeting, then the greeting, then a clean prompt.

### Root cause

`teammux` translates the tmux verbs Claude Code drives:

1. `split-window` → `herdr pane split` (`src/teammux.rs::split_window`). The new
   pane comes up running the user's login shell; no command runs here.
2. `respawn-pane -k` → `herdr pane run` (`src/teammux.rs::respawn_pane`).

`herdr pane run` writes bytes straight to the pty — the existing doc comment
already said as much ("submits `CMD` to the pane the same way a human typing it
would"). Step 2 follows step 1 immediately, while the shell is still running
its init, so the shell never reads those bytes and its own redraw wipes them
off the screen. The command is destroyed.

`respawn_pane` was fire-and-forget: `pane_run` returning `Ok` only means herdr
accepted the keystrokes, never that anything ran. So the shim reported success
and Claude Code reported "Spawned successfully".

Reproduced standalone, no Claude involved:

```sh
PANE=$(herdr pane split <target> --direction down --no-focus --cwd /tmp | jq -r .result.pane.pane_id)
herdr pane run "$PANE" "echo MARKER > /tmp/marker.txt"   # immediately
sleep 3; cat /tmp/marker.txt        # No such file — command was swallowed
```

**This is a herdr-level race.** herdmates only fails to guard against it.

### Two fixes that did NOT work (don't retry these)

1. **Verify via `herdr agent wait` that an agent started.** Dead end. herdr
   identifies agents from their terminal title — detection manifest
   `~/.local/state/herdr/agent-detection/remote/claude.toml`, id `claude`,
   version `2026.07.13.1`, `min_engine_version = 2` — and a Claude Code
   *teammate* sets a different one than a plain session. Live-checked: a
   teammate that is
   running fine and answering tasks reports `agent: null`, and
   `herdr agent explain <pane>` answers `agent_not_found`; a bare `claude` in
   the same tab resolves as `agent: claude` with `agent_status: idle`. Gating on
   detection fails every teammate spawn, including successful ones.

2. **Verify the shell echoed the command back.** Also wrong, and subtly so.
   herdr's terminal renders `pane run`'s bytes on arrival, so the command text
   appears in `herdr pane read` whether or not any shell read it. Instrumented:
   the needle matched on the very first read of a pane that then swallowed the
   command. Echo presence is not evidence of acceptance.

### The fix that works

Wait for the pane to stop drawing *before* typing at it. Poll
`herdr pane read --source recent-unwrapped` until two consecutive reads are
identical and non-empty, then submit once.

- Shell- and prompt-agnostic — no prompt pattern to match, which matters given
  how varied prompts are. `pane wait-output --regex` was rejected for that
  reason.
- Unwrapped source specifically: a wrapped read splits long lines at the pane
  width.
- Measured settle time here: ~1.0s, after which the identical command runs.
- On timeout (10s) it submits anyway. A never-quiet pane may just be an
  animated prompt; failing a spawn that would have worked is the worse error.

Then verify what is actually executing, and fail loudly if nothing is:

```
herdr pane process-info --pane <id>
```

`foreground_process_group_id != shell_pid` ⟺ a job is running in the pane. Live
values on herdr 0.7.5:

| pane | fg_pgid | shell_pid | job running |
|---|---|---|---|
| running a teammate | 255139 | 254798 | yes |
| idle fish prompt | 258476 | 258476 | no |

This is the one spawn signal herdr offers that pane *text* cannot fake, and it
is shell- and command-agnostic — it asks the pty who owns the foreground
process group, not what the screen shows. Polled to a 10s deadline; on timeout
`respawn-pane` exits nonzero, so a spawn that did not spawn is never reported as
success.

> Found late, and only because a fresh agent suggested it. It was dismissed
> early in this investigation after `herdr pane process-info <pane>` returned
> `unknown option` — the pane id is **`--pane <id>`**, not positional. That
> single misread cost a whole design compromise: an earlier draft of this
> document claimed no post-submit signal existed and called the gap deliberate.
> Re-read the `--help` before concluding a herdr verb can't do something.

### Refuted theory — fish brace expansion

A closed-book agent, asked to diagnose this cold, made **fish brace expansion of
`--settings {"teammateMode":"tmux"}`** its primary hypothesis, with a real
demonstration (`fish -c 'echo --settings {"teammateMode":"tmux"}'` →
`--settings {teammateMode:tmux}`). It is still wrong here, twice over:

- The generated argv is **single-quoted** — `--settings '{"teammateMode":"tmux"}'`
  — so expansion never applies.
- The standalone repro uses `echo MARKER > /tmp/marker.txt`, which contains no
  braces, and is swallowed identically.

Worth recording because it is the theory a competent reader reaches first, and
it sends you into shell quoting instead of timing.

### Diagnostics worth knowing

- `~/.config/herdr/herdr-server.log` — full API request trace
  (`method="pane.split"`, `pane.rename`, …) with timestamps. A `pane.split` +
  `pane.rename` pair with nothing after it is this bug, visible directly.
  **Read `agent changed … pgid=…` lines carefully**: they are *detection*
  transitions, and `pgid` is incidental context, not a pgid-change event. A
  reviewer took the gap between two of them as 27s of legitimate startup and
  concluded the 10s job budget was too tight. Measured directly, pgid flips
  **0.11s** after submit — it tracks the shell's fork, not the agent's boot
  (`pane run "sleep 30"` flips it at once and holds it). The log also shows herdr
  transiently misdetecting an idle fish prompt as `agent=Some(Kiro)`.
- `pgrep -f -- '--agent-id <name>@<team>'` **false-positives**: it matches the
  invoking shell's own command line. Use `pane process-info`, which is scoped to
  the pane's tty.

### Possible future direction — stop typing entirely

`herdr agent start <name> --kind claude --pane <id> -- <args…>` waits for prompt
readiness and returns nonzero unless the agent is detected — the whole problem,
solved by herdr. Two blockers for the teammate path: it runs the **canonical**
executable (not Claude Code's versioned
`~/.local/share/claude/versions/<v>` binary) and takes no `env` prefix or `cd`,
which would have to move to `pane split --cwd/--env` — and `split-window` does
not yet know them, since they only arrive with the later `respawn-pane` command
string. Also gated on detection, which per dead end 1 does not see teammates.

### Files touched

| File | Change |
|---|---|
| `src/teammux.rs` | `SubmitPolicy` (settle timeout / poll interval / quiet reads / job timeout), `wait_until_quiet()`, `job_started()`, `respawn_pane_with()` taking an injectable policy so tests don't pay the production clock; `respawn_pane()` delegates with `SubmitPolicy::default()` |
| `src/herdr.rs` | `PaneProcessInfo` + `job_running()`; `HerdrApi::pane_read_unwrapped` and `HerdrApi::pane_process_info` with `HerdrClient` impls and parser; `FakeHerdr` gains `pane_reads` / `last_pane_read` (queue whose last value repeats, modelling a terminal that goes quiet), `pane_never_settles`, `pane_idle_after_submit` |

### Tests

In `src/teammux.rs`:

- `respawn_pane_waits_for_the_shell_to_stop_drawing_before_typing` — the
  regression test. Asserts ≥4 reads precede the submit.
- `respawn_pane_types_the_command_exactly_once`
- `respawn_pane_still_submits_when_the_pane_never_goes_quiet`
- `respawn_pane_fails_loudly_when_no_job_started_in_the_pane` — the
  silent-success regression test.
- `respawn_pane_succeeds_once_a_job_is_running_in_the_pane`

Confirmed the regression test genuinely fails without the fix: commenting out
the `wait_until_quiet` call gives
`must watch the pane until it repeats itself, not type at once: ["pane_run:w1A:p6:claude"]`.

---

## Bug 2 — `herdr agent wait` is called with a flag that does not exist

### Symptom

Silent. Nothing visibly breaks, because the error is indistinguishable from a
generic failure. Surfaced only because bug 1's first fix attempt called
`agent_wait` on a path that reports its errors:

```
herdr agent wait failed: `"herdr" "agent" "wait" "w2D:p3" "--status" "idle" "--timeout" "90000"`
exited with status Some(2): unknown option: --status
```

### Root cause

`herdr agent wait --help` (herdr 0.7.5, protocol 17): the status selector is
`--until`, not `--status`. `HerdrClient::agent_wait` builds `--status`.

```
Usage: herdr agent wait <TARGET> [OPTIONS]
      --until <STATUS>   State to match; repeat for more than one state
                         [possible values: idle, working, blocked, done, unknown]
      --timeout <MS>     Fail after this many milliseconds
```

An unknown option exits **2**. `agent_wait` maps exit **1** to
`WaitOutcome::TimedOut` and everything else to `Err`. So every call is an error,
never a wait — degrading each verify path in `src/spawn.rs` and `src/msg.rs`
(the ADR-0006/ADR-0008 submission-verification discipline) into a hard failure.

Whether herdr renamed the flag or it was never right, I didn't establish. Worth
checking `~/Projects/herdr-upstream` history before writing the PR — it changes
whether this reads as "regression after a herdr update" or "always broken".

### Why the test suite missed it

Every `agent_wait` test goes through `FakeHerdr`, which records
`agent_wait:<pane>:<status>` — a semantic tuple, never real argv. `src/hook.rs`'s
test helper reconstructs an argv-shaped vector from those recordings, so it
mirrored the wrong flag rather than catching it. Nothing in the suite ever
executed a real `herdr` binary for this call.

### Files touched

| File | Change |
|---|---|
| `src/herdr.rs` | `--status` → `--until` in `HerdrClient::agent_wait`; `FakeBinary` now records invocation argv, plus an `argv()` accessor |
| `src/hook.rs` | test argv mirror updated to `--until` |

### Test

`src/herdr.rs::agent_wait_names_the_status_with_herdrs_until_flag` — runs the
real `HerdrClient` against a stub binary and asserts on recorded argv. Failed
before the fix with
`herdr takes --until, sent: ["agent", "wait", "opaque-pane", "--status", "idle", "--timeout", "1000"]`.

This is the general gap worth mentioning upstream: **no test executes real argv**,
so any flag drift against the herdr CLI is invisible. `FakeBinary::argv()` is
the hook for closing it more broadly.

---

## Verification

Four spawns of the same teammate command, one binary revision apart:

| probe | binary | outcome |
|---|---|---|
| `scout` | stock | silent failure; only ran after pasting the command by hand |
| `probe` / `probe2` | agent-detection gate | loud failure — wrong gate (see dead end 1) |
| `probe3` | echo gate | silent failure — gate matched rendered bytes (dead end 2) |
| `probe4` | settle gate | works, but a dead pane would still have reported success |
| `probe5` | settle gate + `process-info` job check | **works, and a dead pane now fails loudly** |
| `probe6` | scratch build, settle gate removed on purpose | fails loudly — the silent-success path, live-verified |
| `probe7` | restored build | works — confirms the restore |

`probe4` end-to-end: pane `w2D:pC`, title `✳ general-purpose`, live process,
inbox message consumed, task executed, reported
`probe4 alive in pane w2D:pC`. Spawn → delivery → execution → report, no manual
intervention.

Gates: 461 tests pass, `cargo fmt --check` clean,
`cargo clippy --all-targets -- -D warnings` clean.

The failure branch is verified live too, not only in unit tests. Since the race
stops reproducing once the fix is in, it was forced: a scratch build with the
`wait_until_quiet` call commented out re-armed the race, then a real teammate
spawn. `respawn-pane` exited nonzero:

```
teammux: respawn-pane: no process started in pane w2D:pM after running
`cd … --agent-id probe6@session-6f8f38a2 …` — the pane is still at its shell prompt
```

Same physical condition that previously reported "Spawned successfully". Source
was then restored from a checksummed copy (`md5` re-matched), gates re-run green,
and a normal spawn (`probe7`) confirmed working.

### `spawn` and `msg` — live A/B, and two more race sites

Both were exercised live against a one-worker `herdr-team.toml`, each run twice:
once with the fixed binary, once with `~/.local/bin/herdmates.pre-respawnfix.bak`.

| path | pre-fix binary | fixed binary |
|---|---|---|
| `spawn` | `worker 'w1' failed during agent startup: "herdr" "agent" "wait" "w2F:p1" "--status" "idle" "--timeout" "89999" exited with status Some(2): unknown option: --status` | `worker 'w1' timed out waiting for agent status 'idle' during agent startup` |
| `msg` | `"herdr" "agent" "wait" "w2E:p1" "--status" "working" "--timeout" "2000" exited with status Some(2): unknown option: --status` | `message submission to 'w1' timed out waiting for agent status 'working'` |

Bug 2 confirmed on both paths: pre-fix the wait dies instantly on an unknown
flag, post-fix it is a real wait that can time out. **The fix works.**

The runs also exposed that the same race lives in two more places, neither
covered by the `teammux.rs` fix:

1. **`spawn.rs` has bug 1 too.** Its worker pane was left at a fish prompt —
   `process-info` reported `foreground_process_group_id == shell_pid` with
   `/bin/fish` in the foreground. It calls `pane_run(launcher.command)` on a
   freshly created pane with no settle wait, exactly as `respawn_pane` used to.
   The startup timeout is now *correct behaviour reporting a real failure* — but
   the failure is this race, not a slow agent.
2. **`msg.rs` races the agent's TUI, not the shell.** The fixed run's message
   body never reached the composer (read back empty); the later pre-fix run's
   body landed and the worker answered it. The difference was elapsed time since
   the agent started. `msg`'s existing empty-`pane_run` retry does not help here:
   it is designed for a swallowed *Enter* on text already in the composer, and
   submits an empty composer when the whole body was swallowed.

Manually starting the worker with the settle technique before messaging worked
first time (`quiet=2`, job started 1.11s later, `agent: claude / idle`), which is
the same evidence that fixed `respawn-pane`.

**Both are now fixed too**, at the maintainer-facing cost of a wider diff.
`SubmitPolicy` / `wait_until_quiet` / `job_started` moved out of `teammux.rs`
into **`src/panesubmit.rs`**, and:

- `spawn.rs::launch_worker` settles the pane before submitting the launcher
  command (`SubmitPolicy::default()`, 10s).
- `msg.rs::deliver_message` settles the target before typing the body, on a
  deliberately short 3s budget: a target mid-turn animates and never goes quiet,
  and messaging a busy worker is supported and must stay prompt.

The `msg` race was verified independently of the plugin, against a bare pane:
a body submitted 1s after an agent starts never appears (`EARLY landed: False`),
the same body after the UI settles does (`LATE landed: True`).

Live end-to-end after the fix, same one-worker spec that previously failed:

- `herdmates spawn` → `team run created: …/bug2-verify-1785315471606`, worker
  pane `w2G:p1` came up as `agent: claude` and executed its brief, reporting
  `w1 alive in pane w2G:p1` and writing `inbox/w1.md`. Previously this pane sat
  at a fish prompt and `spawn` timed out.
- `herdmates msg w1 …` → exit 0, body delivered and answered in-pane
  (`❯ Reply with exactly: MSG-FIXED-OK` / `● MSG-FIXED-OK`). Previously it timed
  out waiting for `working`.
- Worker → god `msg` also delivered, arriving unprompted in the god session.

One unrelated defect surfaced during these runs and is **not** fixed here — see
bug 3 below. It is orthogonal to the race and wants its own issue.

Also observed working: the star-topology status hook. `inbox/events.jsonl`
recorded `working` → `done` for `bug2-verify/tester` and fired the report-pointer
notification into the god session unprompted.

---

## Local install state

Built from this tree and installed to `~/.local/bin/{herdmates,teammux}`.
`~/.local/state/herdr/plugins/caioniehues.herdmates/teammux/bin/tmux` is a
symlink to `~/.local/bin/teammux`, so it picks up the new binary automatically.

Backups of the pre-fix binaries: `~/.local/bin/{herdmates,teammux}.pre-respawnfix.bak`.

No commit, no push, no PR, no upstream issue — deliberately left for the fork.

---

## Bug 3 — `msg` to a blocked target silently approves its permission prompt

**Not fixed here.** Found while verifying the `msg` fix; unrelated to the race,
and a different class of problem. Worth its own upstream issue, and worth
reporting even if the rest is rejected.

### Symptom

Sending a routine message to a worker that happens to be sitting on a permission
prompt **approves that prompt** and runs the pending tool call. The message
itself is discarded. `msg` reports success.

### Why

`delivery_decision(queues_midturn, status)` gives claude `queues_midturn = true`,
so per ADR-0008 the message is delivered immediately regardless of the target's
status — including `blocked`. But a blocked Claude Code pane is showing a modal
dialog, not a composer. `herdr pane run` writes the body plus a newline to the
pty; the modal consumes the keystrokes and the newline **activates the
highlighted option**, which defaults to *Yes*.

### Reproduced deliberately, outside the plugin

A pane running `claude`, driven to a permission prompt for
`touch /tmp/PERMTEST_MARKER`, then sent a message body exactly the way
`deliver_message` sends one:

```
marker before:            False      # dialog up, awaiting approval
herdr pane run <pane> "Reply with exactly: BLOCKED_MSG_MARKER"
marker AFTER pane_run:    True       # ← the pending command was approved and ran
body visible in pane:     False      # ← the message itself was discarded
dialog still up:          False
```

So the delivery both loses the message and auto-approves an arbitrary pending
tool call. Any coordinator that messages workers on a timer can approve prompts
it never saw.

### Fix direction (not implemented)

Gate delivery on the target not being `blocked`, regardless of `queues_midturn` —
`blocked` means "a human is being asked something", which is categorically
different from the mid-turn queueing `queues_midturn` describes. The outbox
machinery ADR-0008 already builds for non-queueing agents is the natural place
to park the message until the target leaves `blocked`; the
`pane.agent_status_changed` hook already drains on status flips.

Left alone here because it changes delivery *policy*, not a race, and the
maintainer owns that decision.

---

## When forking for the PR

Repo hard rules (`CLAUDE.md`): pushes to `main` are releases, gate every push on
fmt/clippy/tests, bump the manifest `version` on behaviour change, tag releases,
and **no push without Caio's ask**. Split the fixes into two commits — the bugs are
independent and bug 2 stands on its own:

```
fix: settle a pane's shell before typing a command into it
fix: herdr agent wait takes --until, not --status
```

Bug 3 is **not** in the diff: report it as its own issue. It is a delivery-policy
decision, and the safety consequence (auto-approving a pending permission
prompt) deserves to be read on its own rather than buried in a race fix.
