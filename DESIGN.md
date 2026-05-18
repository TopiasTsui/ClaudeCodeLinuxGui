# Design: handling built-in slash commands

## Context

The GUI does not run the interactive Claude Code REPL. It spawns the CLI in
programmatic mode:

```
claude -p --input-format stream-json --output-format stream-json \
       --verbose --include-partial-messages [--resume SID] \
       [--permission-mode M] [--add-dir D]
```

In this mode the CLI does **not** interpret built-in slash commands
(`/model`, `/context`, `/cost`, `/clear`, …). They are features of the
interactive TUI client. Typing `/model` in the entry today just sends the
literal text `/model` to the model.

## Principle

**The GUI is a thin transport. It does not model Anthropic's catalog or the
CLI's value domains.** Model names, effort levels, context-window sizes —
none of these are stored in the GUI. The CLI is the source of truth and the
validator. The GUI maps a command to a flag/transport mechanism and passes
arguments through opaquely.

## Mechanism: intercept + data-driven registry + ≤4 routes

No per-command UI. One interception layer in the send path:

```
on submit(text):
  if text starts with '/':
     name, args = split(text)
     entry = registry.lookup(name)
     if entry: run entry via its route
     else:     route D (send as a normal user message)
  else:
     existing behaviour (send as user message)
```

Never swallow a command: an unrecognised `/foo` always falls through to
route D, so project/custom `.claude/commands/*` keep working.

### Registry

A table; each row describes the *mechanism only*, never a value domain:

```
{ name, route: A|C|D, arg: none|one|rest, flag?: "--model", help }
```

`/help` and any future autocomplete are generated from this table. Adding a
command is one row, zero new widgets. The table contains no model names, no
effort levels, no window sizes.

### Routes

| Route | Meaning | Commands | Implementation |
|-------|---------|----------|----------------|
| **A — respawn** | change a spawn flag, restart with `--resume <sid>` so context carries | `/model <arg>`, `/permission-mode <m>`, `/clear` (= respawn *without* `--resume` = fresh session + clear transcript) | reuse the existing respawn path (the mode dropdown and the approve flow already do exactly this); args passed through verbatim, CLI validates |
| **B — control** | runtime control without respawn | stop / interrupt | **probe**: if stream-json supports a control request, send it; otherwise degrade to A or omit. Never assumed to exist. |
| **C — local** | answered by the GUI, no model round-trip | `/status`, `/help` | rendered from the GUI's own bookkeeping into a System message |
| **D — passthrough** | unrecognised / custom | everything else `/x` | sent verbatim as a user message |

### Per-command decisions

- **`/model`**: with an argument → route A, passed to `--model` verbatim.
  Without an argument → echo the current model read from the stream-json
  init/system event (zero-maintenance; no stored list).
- **`/status`**: absorbs `/context` and `/cost`. One summary line: current
  model + session id + permission-mode + this-turn token count (from the
  `usage` aggregate). If the context-window limit is **not** present in the
  init event, show absolute tokens only — **no fabricated percentage**
  (a percentage would require hardcoding window sizes = modelling the
  catalog).
- **`/cost`**: not implemented. Additionally, the existing per-turn/session
  **USD** line in the transcript and the `total_cost_usd` parsing are
  removed — cost is noise on a subscription plan.
- **`/context`**: not implemented as its own command. The real `/context`
  category breakdown (system prompt / tools / skills / messages) is the
  interactive client's internal estimate of its assembled prompt; it is not
  exposed by the stream-json protocol. A half version would mislead.

## Hard invariants

- **Rollback on failure**: if a route-A respawn dies immediately (e.g. a bad
  `/model` argument), keep the old session id, surface the CLI's error as a
  System message, and never kill the session.
- **No mirroring of out-of-protocol state**: anything the protocol does not
  send (category context, model catalog, window size) is never fabricated.

## Known follow-up (not blocking)

The permission-mode `DropDown` is superseded by `/permission-mode`. Whether
to remove the dropdown is a separate decision that changes already-shipped
behaviour; keep the dropdown for one release alongside the command, remove
later.
