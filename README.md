# Claude Code — Linux GUI (v0.0.1)

A minimal, free, open-source desktop GUI for the official **Claude Code** CLI on Linux.

> Not affiliated with, endorsed by, or sponsored by Anthropic. This is an
> independent wrapper around the official `claude` command-line tool.

## What this is (and is not)

v0.0.1 is intentionally tiny. It does exactly one thing end to end:

1. Pick a working folder.
2. Type a message.
3. It runs the official `claude` CLI in that folder and shows the reply.
4. Context is kept across messages within the session.

**v0.0.1 known boundaries (deliberate, not bugs):**

- **Chat only — tools are disabled** (`--tools ""`). Claude will not edit
  files or run commands yet. This avoids the GUI hanging on a permission
  prompt it can't answer. Tool support is future work.
- **No streaming.** Each turn waits for the full reply (`--output-format
  json`). Token-by-token streaming is unverified and deferred.
- The model's raw reply text may occasionally contain stray markup; v0.0.1
  shows it verbatim.

## How it works (grounded in tested CLI behavior)

- Each turn = one `claude -p <message> --output-format json` process.
- First turn uses `--session-id <uuid>`; later turns use `--resume <uuid>`.
  Context persistence across turns was verified empirically against the
  installed CLI before this code was written.
- The `claude` binary is resolved from explicit known locations, not just the
  inherited `PATH`. This is a deliberate response to the most common failure
  of existing Claude Code GUIs (desktop-launched process missing `PATH`,
  e.g. `env: node: No such file or directory`). Override with `CLAUDE_BIN`.

## Requirements

- Linux
- Node.js + npm
- The official Claude Code CLI installed and authenticated (`claude`)

## Run

```bash
npm install
npm start
```

## License

**Not chosen yet (TBD).** No license file is included on purpose; until one
is added, default copyright applies. Pick one before publishing/accepting
contributions.

## Status

Early scaffold. The CLI-invocation logic is grounded in empirical probes of
the installed `claude` CLI. The GUI itself has not been verified to render in
the environment where it was scaffolded — run `npm start` on a Linux desktop
to confirm.
