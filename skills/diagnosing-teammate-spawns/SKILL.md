---
name: diagnosing-teammate-spawns
description: Use when a Claude Code teammate spawned through herdr/herdmates reports success but never runs — pane appears with the right label yet sits at a shell prompt, the task in ~/.claude/teams/*/inboxes/*.json stays unread, no --agent-id process exists, or a pane shows agent: null / agent_not_found. Also use before writing code that verifies a teammate started.
---

# Diagnosing teammate spawns

herdr panes always start a login shell; `herdr pane run` then **types** a command
into it. So a pane can exist, be labelled, and report success while nothing ran.
Most obvious success signals are false. This skill says which signal is real.

## The one reliable liveness signal

```bash
herdr pane process-info --pane <pane_id>     # NOT positional — `--pane` or it errors
```

`foreground_process_group_id != shell_pid` ⟺ something is running in the pane.
Idle prompt has them equal. Shell- and command-agnostic, and it cannot be faked
by bytes herdr wrote to the pty.

Matching `foreground_processes[].argv` against `--agent-id <name>@<team>` also
works and additionally proves *which* teammate — but it breaks on any command
shape change. Prefer the pgid comparison for code; use argv when you need identity.

## Signals that lie

| Signal | Why it lies |
|---|---|
| `pane run` / `respawn-pane` exit 0 | Means "bytes written to the pty", never "a shell read them" |
| Pane text via `pane read` | herdr renders `pane run` bytes **on arrival**; the command appears whether or not any shell took it |
| `agent`, `agent_status` from `pane list` / `agent list` | Teammates are never detected — see below |
| `herdr agent wait` / `agent explain` | Same detection; blocks to timeout on a healthy teammate |
| Pane label, `config.json` member, inbox file | All written at spawn-*request* time, before anything runs |
| `pgrep -f -- '--agent-id X'` | Matches your own invoking shell's command line. False positive |

## Never `pane run` at a pane whose status is `blocked`

A blocked Claude pane is showing a modal permission dialog, not a composer. The
text is discarded and the trailing newline **activates the highlighted option**
— which defaults to *Yes*, approving whatever tool call was waiting. Verified:
a pane awaiting approval for `touch /tmp/PERMTEST_MARKER`, sent an unrelated
message body, created the file. Check `agent_status` first, or send nothing.

## Why teammates read as `agent: null`

herdr identifies agents by **terminal title** (manifest
`~/.local/state/herdr/agent-detection/remote/claude.toml`). A bare `claude`
titles its pane `<cwd>: claude - claude` and is detected. A Claude *teammate*
titles it `✳ <agent-type>` and is not. `agent explain` answers
`agent_not_found` for a teammate that is running and answering.

This is **not** about pane registration: a bare `claude` typed into an
unregistered split pane is still detected.

## Diagnose in this order

```bash
herdr pane list                                   # label set? title still "- fish"?
herdr pane process-info --pane <id>               # the real answer
herdr pane read <id> --source recent-unwrapped    # unwrapped, or long lines break matching
grep -E 'pane\.(split|run|rename)' ~/.config/herdr/herdr-server.log | tail -20
python3 -m json.tool < ~/.claude/teams/<team>/inboxes/<name>.json   # "read": false → never consumed
```

A `pane.split` with no later evidence of the command running means the shell
swallowed it: `pane run` reached the pty before the shell's init finished. Wait
for two consecutive identical `pane read --source recent-unwrapped` snapshots
before submitting. Slow interactive prompts (greetings, VCS/cloud segments)
widen this window to whole seconds.

## Wrong theories to skip

Capable agents reach both of these unaided. Both are refuted:

- **"fish brace expansion mangles `--settings {...}`."** The generated argv is
  single-quoted, so expansion never applies — and a brace-free
  `echo MARKER > file` is swallowed identically.
- **"detection fails because the pane wasn't registered."** See above.
- **"the log shows startup takes ~30s, so poll for that long."** The server
  log's `agent changed … pgid=…` lines are *detection* transitions; `pgid` is
  incidental context, not a pgid-change event. The gap between two such lines is
  not startup latency. Measured: pgid flips **0.11s** after submit, because it
  tracks the shell's fork, not the agent's boot — `pane run "sleep 30"` flips it
  immediately and keeps it flipped.

## Launching, not just checking

`herdr pane split` takes no command — launching is unavoidably split-then-type.
`herdr agent start <name> --kind claude --pane <id> -- <args>` waits for prompt
readiness and verifies detection, but runs the *canonical* executable and cannot
carry a versioned binary path, an `env` prefix, or a `cd`. Set those with
`pane split --cwd/--env` if you go that route.
