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

---

# Management panel: MCP / Skills / Plugins

## Capability is asymmetric (from `claude --help`, v2.1.143)

| | browse local | browse network | install | JSON |
|---|---|---|---|---|
| **Plugin** | `plugin list --json` | `marketplace add` + `plugin list --available --json` | `plugin install <p@market>` / `enable` | yes |
| **MCP** | `mcp list` / `get` | none (no registry/search) | `mcp add <url\|cmd>` (known address only) | no |
| **Skill** | from plugins + `.claude/skills/` FS | none (no `claude skill`, no registry) | only via a plugin that bundles them, or manual FS | no |

Only **plugins** have a real browse-network-and-install story. MCP can only
list-configured / add-by-known-address / remove. Skills have no network
source at all. The UI must not pretend otherwise.

## Same principle as the command layer

The GUI is a thin transport. The panel does not build a catalog: it shells
out to `claude plugin …` (JSON where available) and parses defensively; the
CLI stays the source of truth. Reads run as one-shot `Command`s off the GTK
thread (existing `async_channel` + `spawn_future_local` pattern), wholly
separate from the `claude -p` stream-json session process.

## v1 scope (decided): read-only browse

No config writes, no installs. The panel lists:

- installed plugins — `claude plugin list --json`
- available plugins — `claude plugin list --available --json` (network read)
- configured marketplaces — `claude plugin marketplace list` (raw text ok)
- MCP servers — parsed **directly from `~/.claude.json`** (and a project
  `.mcp.json` when known), **not** via `claude mcp list`
- skills — `~/.claude/skills/` + project `.claude/skills/` (SKILL.md
  frontmatter); plugin-bundled skills are shown under their plugin

Defensive parsing: if JSON shape differs from expectations, fall back to
showing trimmed raw stdout/stderr rather than breaking.

## Security constraints (baked in, not optional)

- **Listing must not execute.** `claude mcp list` / `doctor` spawn stdio
  servers from `.mcp.json` for health checks. v1 therefore reads the JSON
  config files directly and never spawns a server just to display it.
- **Never print secrets.** MCP `env` / `headers` values (tokens, keys) are
  redacted in the UI; only name, transport, and command/URL host are shown.
- **Install (future, out of v1 scope) is explicit.** Any install/enable/
  remove action must show provenance (marketplace / URL) and require an
  explicit confirmation. Installing a network MCP server or plugin runs
  third-party code on the user's machine; no silent one-click installs.

## v2 scope: mutating actions

Still a thin transport — v2 only adds *explicitly-confirmed* mutating
`Command` invocations. The read-only list views stay; each relevant tab
gets an action bar. Every action resolves to an exact `claude …` command
that is shown verbatim in a confirmation dialog before it runs; on confirm
it runs off-thread, its stdout+stderr is shown, then the list refreshes.

- **Plugins**: action bar with a `plugin@marketplace` field and
  Install / Enable / Disable / Uninstall / Update → `claude plugin …`.
- **Marketplaces**: a `<url|github|path>` field and Add / Remove / Update →
  `claude plugin marketplace …` (adding a marketplace is itself a trust
  decision; same confirmation).
- **MCP**: a form (name, transport, command-or-URL, optional env/headers,
  scope = user|project|local) → `claude mcp add` / `add-json`, and
  Remove by name. No network browse (the CLI has no MCP registry). The
  stdio-spawns-a-process warning is shown in the confirmation.
- **Skills**: no first-party install path. The tab states this and points
  to the Plugins tab (skills ship inside plugins). An optional
  clone/unzip-into-`.claude/skills` entry is explicitly out of default
  scope (unbacked manual code execution).

Cross-cutting:

- Confirmation dialog shows the resolved command and its provenance; no
  silent one-click installs.
- Secrets entered for MCP `env`/`headers` are passed to the CLI but never
  echoed back in the UI, the confirmation, or any log.
- Failures surface the CLI's stderr verbatim — never report success on a
  non-zero exit.
- Changes take effect on the next session spawn (some need a full
  restart). The panel says so; applying is the existing respawn path.
- Still no curated catalog/ratings — marketplaces and the CLI are the
  only source of truth.
- v2 builds on a runtime-verified v1.
