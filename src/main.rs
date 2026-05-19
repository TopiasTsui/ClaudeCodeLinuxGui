// Claude Code — Linux GUI (v0.5.0) — native GTK4, no Electron.
//
// Persistent process + STREAMING render (the real fix for "feels slow"):
//   console streams tokens so it feels instant even when a turn takes long;
//   we now do the same — consume `content_block_delta` text deltas and render
//   progressively (debounced ~150ms), then finalize on the `result` event.
//   Requires `--include-partial-messages`.
//
// Verified by probes: persistent stream-json process keeps one session and
// carries context; input line schema
//   {"type":"user","message":{"role":"user","content":"..."}}
// stream events: stream_event/content_block_delta(text_delta) -> incremental
// text; `result` ends the turn (text/cost/session_id/permission_denials).
//
// One long-lived `claude` per tab; permission-mode/--add-dir are launch flags
// so approve & mode-change restart with `--resume <sid>` (context carries).
//
// UNVERIFIED at runtime: concurrency + streaming render. Compiles; runtime
// needs `cargo run`.
//
// Not affiliated with, endorsed by, or sponsored by Anthropic.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::gdk::prelude::TextureExt;
use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow};
use webkit6::prelude::*;

const APP_ID: &str = "dev.local.claude_code_linux_gui";

const MODE_LABELS: [&str; 4] = ["Ask (default)", "Plan", "Accept edits", "Auto"];
fn mode_flag(idx: u32) -> Option<&'static str> {
    match idx {
        1 => Some("plan"),
        2 => Some("acceptEdits"),
        3 => Some("auto"),
        _ => None,
    }
}

// ── Built-in command handling ────────────────────────────────────────────
//
// See DESIGN.md. The GUI is a thin transport: this table describes only the
// *mechanism* of each command, never a value domain (no model names, no
// effort levels, no window sizes — the CLI is the validator). `/help` is
// generated from this table; adding a command is one row, zero new widgets.
// Unrecognised `/x` falls through to Route::Passthrough (sent verbatim).

enum Route {
    /// Set a spawn flag from the verbatim argument, respawn with `--resume`
    /// (context carries). The CLI validates the value.
    RespawnFlag(&'static str),
    /// Respawn WITHOUT `--resume`: fresh session + cleared transcript.
    Clear,
    /// Answered locally by the GUI; no model round-trip.
    Local(Local),
}

#[derive(Clone, Copy)]
enum Local {
    Status,
    Help,
}

struct Cmd {
    name: &'static str,
    route: Route,
    /// Shown in `/help` only — never used to reject a value (CLI validates).
    usage: &'static str,
    help: &'static str,
}

const COMMANDS: &[Cmd] = &[
    Cmd {
        name: "/model",
        route: Route::RespawnFlag("--model"),
        usage: "/model [alias|full-id]",
        help: "switch model (no arg: show current); value passed to the CLI as-is",
    },
    Cmd {
        name: "/permission-mode",
        route: Route::RespawnFlag("--permission-mode"),
        usage: "/permission-mode <mode>",
        help: "set permission mode (overrides the dropdown); CLI validates",
    },
    Cmd {
        name: "/clear",
        route: Route::Clear,
        usage: "/clear",
        help: "start a fresh session (conversation context dropped)",
    },
    Cmd {
        name: "/status",
        route: Route::Local(Local::Status),
        usage: "/status",
        help: "show model / session id / permission-mode / last-turn tokens",
    },
    Cmd {
        name: "/help",
        route: Route::Local(Local::Help),
        usage: "/help",
        help: "list these commands",
    },
];

fn lookup_cmd(name: &str) -> Option<&'static Cmd> {
    COMMANDS.iter().find(|c| c.name == name)
}

#[derive(Clone, Copy, Default)]
struct Usage {
    input: u64,
    cache_create: u64,
    cache_read: u64,
    output: u64,
}

fn resolve_claude() -> String {
    if let Ok(v) = std::env::var("CLAUDE_BIN") {
        if !v.is_empty() {
            return v;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for c in [
        format!("{home}/.local/bin/claude"),
        "/usr/local/bin/claude".to_string(),
        "/usr/bin/claude".to_string(),
        format!("{home}/.npm-global/bin/claude"),
    ] {
        if Path::new(&c).exists() {
            return c;
        }
    }
    "claude".to_string()
}

struct TurnResult {
    result: String,
    session_id: Option<String>,
    usage: Usage,
    denials: Vec<String>,
    denied_dirs: Vec<String>,
}

enum Ev {
    /// stream-json `system`/`init`: carries the model actually in use.
    Init {
        model: Option<String>,
        session_id: Option<String>,
    },
    Delta(String),
    Tool(String),
    Thinking,
    Turn(TurnResult),
    Ended(String),
}

#[derive(Default)]
struct Session {
    workdir: Option<PathBuf>,
    session_id: Option<String>,
    pending_approval: bool,
    pending_dirs: Vec<String>,
    allowed_dirs: Vec<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    // Route-A state. `overrides` are extra (flag, value) pairs applied at
    // spawn time (e.g. ("--model","opus")); a repeated flag replaces, not
    // accumulates. `good_overrides` is the last config that spawned without
    // immediately dying — restored if a command's respawn fails. `cmd_pending`
    // marks a respawn triggered by a command so an immediate process death is
    // treated as "bad argument → roll back" rather than a normal end.
    overrides: Vec<(String, String)>,
    good_overrides: Vec<(String, String)>,
    cmd_pending: bool,
    cmd_recovering: bool,
    // Filled from the stream-json init event / result usage; powers /status.
    model: Option<String>,
    last_usage: Option<Usage>,
}

/// Set/replace an override flag (verbatim value; the CLI validates it).
fn set_override(s: &mut Session, flag: &str, val: &str) {
    s.overrides.retain(|(f, _)| f != flag);
    s.overrides.push((flag.to_string(), val.to_string()));
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn md_to_html(md: &str) -> String {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    opts.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(md, opts);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

fn render(tab: &Tab) {
    let msgs = tab.msgs.borrow();
    let live = tab.stream.borrow();
    let mut body = String::new();
    for (who, text) in msgs.iter() {
        if who == "Tool" {
            // Compact live activity line, no header.
            body.push_str(&format!("<div class=\"tool\">{}</div>", esc(text)));
            continue;
        }
        let cls = match who.as_str() {
            "You" => "user",
            "Claude" => "claude",
            _ => "system",
        };
        let inner = if who == "Claude" {
            md_to_html(text)
        } else {
            format!("<pre>{}</pre>", esc(text))
        };
        body.push_str(&format!(
            "<div class=\"msg {cls}\"><div class=\"who\">{}</div>{}</div>",
            esc(who),
            inner
        ));
    }
    if !live.is_empty() {
        body.push_str(&format!(
            "<div class=\"msg claude\"><div class=\"who\">Claude</div>{}</div>",
            md_to_html(&live)
        ));
    }
    let doc = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
         body{{background:#1e1e1e;color:#e0e0e0;font-family:system-ui,sans-serif;\
         margin:0;padding:16px;font-size:14px;line-height:1.55}}\
         .msg{{margin-bottom:18px}}.who{{font-size:12px;color:#888;margin-bottom:4px}}\
         .user pre,.system pre{{white-space:pre-wrap;word-break:break-word;margin:0;\
         font-family:ui-monospace,monospace}}\
         .user pre{{color:#9cdcfe}}.system{{color:#c5915f;font-style:italic}}\
         .tool{{color:#7fae7f;font-family:ui-monospace,monospace;font-size:12px;\
         margin:2px 0;white-space:pre-wrap;word-break:break-word}}\
         code{{background:#2d2d2d;padding:1px 4px;border-radius:3px;\
         font-family:ui-monospace,monospace}}\
         pre code{{display:block;padding:10px;overflow-x:auto}}\
         table{{border-collapse:collapse;margin:8px 0}}\
         th,td{{border:1px solid #444;padding:4px 8px}}th{{background:#2d2d2d}}\
         a{{color:#4ea1ff}}</style></head><body>{body}\
         <script>window.scrollTo(0,1e9);</script></body></html>"
    );
    tab.web.load_html(&doc, None);
}

// Debounced render: coalesce streaming deltas to ~150ms to avoid reload storms.
fn schedule_render(tab: &Tab) {
    if tab.render_pending.get() {
        return;
    }
    tab.render_pending.set(true);
    let tab = tab.clone();
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        tab.render_pending.set(false);
        render(&tab);
    });
}

#[derive(Clone)]
struct Tab {
    sess: Rc<RefCell<Session>>,
    bin: Rc<String>,
    msgs: Rc<RefCell<Vec<(String, String)>>>,
    stream: Rc<RefCell<String>>,
    render_pending: Rc<Cell<bool>>,
    // Process generation: bumped on every (re)spawn. A receiver loop for a
    // superseded process stops silently instead of disabling the UI / printing
    // a spurious "session ended".
    gen: Rc<Cell<u64>>,
    web: webkit6::WebView,
    entry: gtk::Entry,
    img: gtk::Button,
    file: gtk::Button,
    send: gtk::Button,
    stop: gtk::Button,
    approve: gtk::Button,
    mode: gtk::DropDown,
    status: gtk::Label,
}

fn push_msg(tab: &Tab, who: &str, text: &str) {
    tab.msgs.borrow_mut().push((who.to_string(), text.to_string()));
    render(tab);
}

// Three input states. `stop` is the inverse of the send controls: live only
// while a turn is in flight.
fn ui_idle(tab: &Tab) {
    tab.entry.set_sensitive(true);
    tab.img.set_sensitive(true);
    tab.file.set_sensitive(true);
    tab.send.set_sensitive(true);
    tab.stop.set_sensitive(false);
}
fn ui_busy(tab: &Tab) {
    tab.entry.set_sensitive(false);
    tab.img.set_sensitive(false);
    tab.file.set_sensitive(false);
    tab.send.set_sensitive(false);
    tab.stop.set_sensitive(true);
}
fn ui_dead(tab: &Tab) {
    tab.entry.set_sensitive(false);
    tab.img.set_sensitive(false);
    tab.file.set_sensitive(false);
    tab.send.set_sensitive(false);
    tab.stop.set_sensitive(false);
}

fn parse_result(v: &serde_json::Value) -> TurnResult {
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .unwrap_or("(empty response)")
        .to_string();
    let session_id = v.get("session_id").and_then(|x| x.as_str()).map(str::to_string);
    let u = v.get("usage");
    let tok = |k: &str| {
        u.and_then(|x| x.get(k)).and_then(|x| x.as_u64()).unwrap_or(0)
    };
    let usage = Usage {
        input: tok("input_tokens"),
        cache_create: tok("cache_creation_input_tokens"),
        cache_read: tok("cache_read_input_tokens"),
        output: tok("output_tokens"),
    };
    let mut denials = Vec::new();
    let mut denied_dirs = Vec::new();
    if let Some(arr) = v.get("permission_denials").and_then(|x| x.as_array()) {
        for d in arr {
            let tool = d.get("tool_name").and_then(|x| x.as_str()).unwrap_or("?");
            let inp = d.get("tool_input");
            let fp = inp
                .and_then(|i| i.get("file_path"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let cmdline = inp
                .and_then(|i| i.get("command"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if !cmdline.is_empty() {
                denials.push(format!("{tool}: {cmdline}"));
            } else if !fp.is_empty() {
                denials.push(format!("{tool} -> {fp}"));
                if let Some(p) = Path::new(fp).parent() {
                    let p = p.to_string_lossy().to_string();
                    if !p.is_empty() && !denied_dirs.contains(&p) {
                        denied_dirs.push(p);
                    }
                }
            } else {
                denials.push(tool.to_string());
            }
        }
    }
    TurnResult { result, session_id, usage, denials, denied_dirs }
}

fn spawn_proc(tab: &Tab, force_accept_edits: bool) {
    let (workdir, resume_sid, allowed_dirs) = {
        let s = tab.sess.borrow();
        match &s.workdir {
            Some(w) => (w.clone(), s.session_id.clone(), s.allowed_dirs.clone()),
            None => return,
        }
    };
    // New generation; supersedes any previous process's receiver loop.
    let my_gen = tab.gen.get().wrapping_add(1);
    tab.gen.set(my_gen);
    {
        let mut s = tab.sess.borrow_mut();
        s.stdin = None;
        if let Some(mut c) = s.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    let overrides: Vec<(String, String)> = tab.sess.borrow().overrides.clone();
    // A `/permission-mode` override beats the dropdown (DESIGN: command and
    // dropdown coexist for now).
    let pm_override = overrides
        .iter()
        .find(|(f, _)| f == "--permission-mode")
        .map(|(_, v)| v.clone());
    let pm: Option<String> = if force_accept_edits {
        Some("acceptEdits".to_string())
    } else if let Some(v) = pm_override {
        Some(v)
    } else {
        mode_flag(tab.mode.selected()).map(str::to_string)
    };

    let mut cmd = Command::new(&**tab.bin);
    cmd.arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .current_dir(&workdir);
    if let Some(sid) = &resume_sid {
        cmd.arg("--resume").arg(sid);
    }
    if let Some(m) = &pm {
        cmd.arg("--permission-mode").arg(m);
    }
    for d in &allowed_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    // Apply remaining route-A overrides verbatim (permission-mode already
    // applied above). The CLI validates; a bad value just makes the process
    // exit, which we detect below and roll back.
    for (flag, val) in &overrides {
        if flag == "--permission-mode" {
            continue;
        }
        cmd.arg(flag).arg(val);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            push_msg(tab, "System", &format!("Failed to launch claude: {e}"));
            return;
        }
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Keep the tail of stderr so a failed respawn (e.g. bad `/model`) can
    // surface the CLI's own error instead of a bare "stream closed".
    let errbuf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(err) = stderr {
        let errbuf = errbuf.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut b) = errbuf.lock() {
                    b.push_str(&line);
                    b.push('\n');
                    let len = b.len();
                    if len > 2048 {
                        *b = b[len - 2048..].to_string();
                    }
                }
            }
        });
    }

    let (tx, rx) = async_channel::unbounded::<Ev>();
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            let reader = BufReader::new(out);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("system")
                        if v.get("subtype").and_then(|s| s.as_str()) == Some("init") =>
                    {
                        let model =
                            v.get("model").and_then(|x| x.as_str()).map(str::to_string);
                        let session_id = v
                            .get("session_id")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                        if tx.send_blocking(Ev::Init { model, session_id }).is_err() {
                            break;
                        }
                    }
                    Some("result") => {
                        if tx.send_blocking(Ev::Turn(parse_result(&v))).is_err() {
                            break;
                        }
                    }
                    Some("assistant") => {
                        if let Some(content) = v
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_array())
                        {
                            for blk in content {
                                if blk.get("type").and_then(|t| t.as_str())
                                    != Some("tool_use")
                                {
                                    continue;
                                }
                                let name =
                                    blk.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                                let inp = blk.get("input");
                                let tgt = inp
                                    .and_then(|i| i.get("command"))
                                    .and_then(|x| x.as_str())
                                    .or_else(|| {
                                        inp.and_then(|i| i.get("file_path"))
                                            .and_then(|x| x.as_str())
                                    })
                                    .or_else(|| {
                                        inp.and_then(|i| i.get("path"))
                                            .and_then(|x| x.as_str())
                                    })
                                    .unwrap_or("");
                                let mut tgt = tgt.replace('\n', " ");
                                if tgt.chars().count() > 120 {
                                    tgt = tgt.chars().take(120).collect::<String>() + "…";
                                }
                                let label = if tgt.is_empty() {
                                    format!("🔧 {name}")
                                } else {
                                    format!("🔧 {name}: {tgt}")
                                };
                                if tx.send_blocking(Ev::Tool(label)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some("stream_event") => {
                        let delta = v
                            .get("event")
                            .filter(|e| {
                                e.get("type").and_then(|t| t.as_str())
                                    == Some("content_block_delta")
                            })
                            .and_then(|e| e.get("delta"));
                        match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                            Some("text_delta") => {
                                if let Some(t) = delta
                                    .and_then(|d| d.get("text"))
                                    .and_then(|x| x.as_str())
                                {
                                    if !t.is_empty()
                                        && tx
                                            .send_blocking(Ev::Delta(t.to_string()))
                                            .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                            Some("thinking_delta") => {
                                let _ = tx.send_blocking(Ev::Thinking);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            let _ = tx.send_blocking(Ev::Ended("stream closed".into()));
        });
    }

    {
        let mut s = tab.sess.borrow_mut();
        s.child = Some(child);
        s.stdin = stdin;
    }

    let tab = tab.clone();
    let errbuf_r = errbuf.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            if tab.gen.get() != my_gen {
                break; // a newer process superseded this one — stop silently
            }
            match ev {
                Ev::Init { model, session_id } => {
                    // A live init means this config is good: clear the
                    // command-respawn rollback guard.
                    let mut s = tab.sess.borrow_mut();
                    if let Some(m) = model {
                        s.model = Some(m);
                    }
                    if let Some(sid) = session_id {
                        s.session_id = Some(sid);
                    }
                    if s.cmd_pending {
                        s.cmd_pending = false;
                        s.cmd_recovering = false;
                        s.good_overrides = s.overrides.clone();
                    }
                }
                Ev::Delta(t) => {
                    tab.stream.borrow_mut().push_str(&t);
                    schedule_render(&tab);
                }
                Ev::Tool(label) => {
                    tab.status.set_text("🔧 working…");
                    push_msg(&tab, "Tool", &label);
                }
                Ev::Thinking => {
                    tab.status.set_text("💭 thinking…");
                }
                Ev::Turn(o) => {
                    tab.stream.borrow_mut().clear();
                    {
                        let mut s = tab.sess.borrow_mut();
                        if let Some(sid) = o.session_id {
                            s.session_id = Some(sid);
                        }
                        s.last_usage = Some(o.usage);
                        // Output proves the current config works.
                        if s.cmd_pending {
                            s.cmd_pending = false;
                            s.cmd_recovering = false;
                            s.good_overrides = s.overrides.clone();
                        }
                    }
                    push_msg(&tab, "Claude", &o.result);
                    if !o.denials.is_empty() {
                        {
                            let mut s = tab.sess.borrow_mut();
                            s.pending_approval = true;
                            s.pending_dirs = o.denied_dirs.clone();
                        }
                        push_msg(
                            &tab,
                            "System",
                            &format!(
                                "Claude needs permission for:\n  {}\n\
                                 >>> Click [Approve] to allow and continue. Typing does NOT grant it. <<<",
                                o.denials.join("\n  ")
                            ),
                        );
                        tab.approve.set_sensitive(true);
                    } else {
                        tab.sess.borrow_mut().pending_approval = false;
                    }
                    tab.status.set_text("");
                    ui_idle(&tab);
                }
                Ev::Ended(why) => {
                    tab.stream.borrow_mut().clear();
                    // Rollback invariant (DESIGN): if a command-triggered
                    // respawn dies before producing anything, the argument was
                    // bad. Restore the last-good config and respawn once;
                    // never leave the session dead over a typo.
                    let (rollback, recovering) = {
                        let s = tab.sess.borrow();
                        (s.cmd_pending && !s.cmd_recovering, s.cmd_recovering)
                    };
                    let errtail = errbuf_r
                        .lock()
                        .ok()
                        .map(|b| b.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if rollback {
                        {
                            let mut s = tab.sess.borrow_mut();
                            s.overrides = s.good_overrides.clone();
                            s.cmd_pending = false;
                            s.cmd_recovering = true;
                        }
                        let detail = errtail
                            .as_deref()
                            .unwrap_or("the process exited immediately");
                        push_msg(
                            &tab,
                            "System",
                            &format!(
                                "Command rejected by the CLI — reverting to the \
                                 previous config and continuing:\n{detail}"
                            ),
                        );
                        spawn_proc(&tab, false);
                        break;
                    }
                    let extra = if recovering {
                        " (recovery also failed)"
                    } else {
                        ""
                    };
                    let msg = match &errtail {
                        Some(e) => format!("(session process ended: {why}{extra})\n{e}"),
                        None => format!("(session process ended: {why}{extra})"),
                    };
                    push_msg(&tab, "System", &msg);
                    tab.status.set_text("");
                    ui_dead(&tab);
                    break;
                }
            }
        }
    });
}

fn send_turn(tab: &Tab, message: &str) {
    let line = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": message}
    })
    .to_string();
    let mut s = tab.sess.borrow_mut();
    let ok = if let Some(si) = s.stdin.as_mut() {
        writeln!(si, "{line}").and_then(|_| si.flush()).is_ok()
    } else {
        false
    };
    drop(s);
    if ok {
        tab.status.set_text("⏳ working…");
        ui_busy(tab);
        tab.approve.set_sensitive(false);
    } else {
        push_msg(tab, "System", "Error: session process not running. Re-choose the folder.");
    }
}

fn cmd_help_text() -> String {
    let mut s = String::from(
        "Commands — anything else starting with / is sent to Claude verbatim:\n",
    );
    for c in COMMANDS {
        s.push_str(&format!("  {:<22} {}\n", c.usage, c.help));
    }
    s.push_str(
        "\nThe GUI does not validate values — the CLI does. /context and /cost \
         are intentionally not provided (see DESIGN.md).",
    );
    s
}

fn status_text(tab: &Tab) -> String {
    let s = tab.sess.borrow();
    let model = s
        .model
        .clone()
        .unwrap_or_else(|| "(unknown — send a message first)".into());
    let sid = s.session_id.clone().unwrap_or_else(|| "(none yet)".into());
    let pm = s
        .overrides
        .iter()
        .find(|(f, _)| f == "--permission-mode")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            MODE_LABELS
                .get(tab.mode.selected() as usize)
                .copied()
                .unwrap_or("?")
                .to_string()
        });
    let ov = if s.overrides.is_empty() {
        "(none)".to_string()
    } else {
        s.overrides
            .iter()
            .map(|(f, v)| format!("{f} {v}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let usage = match s.last_usage {
        Some(u) => format!(
            "last turn — input {} (cache: {} read / {} created), output {}\n  \
             (no context-window size in the stream-json protocol → no % shown)",
            u.input, u.cache_read, u.cache_create, u.output
        ),
        None => "last turn — (no usage yet)".to_string(),
    };
    format!(
        "Status\n  model: {model}\n  session: {sid}\n  \
         permission-mode: {pm}\n  overrides: {ov}\n  {usage}"
    )
}

/// Returns true if the input was a recognised command (handled here); false
/// for an unrecognised `/x`, which the caller sends as a normal message
/// (Route D — custom `.claude/commands/*` keep working).
fn handle_command(tab: &Tab, line: &str) -> bool {
    let mut it = line.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("");
    let arg = it.next().unwrap_or("").trim().to_string();
    let cmd = match lookup_cmd(name) {
        Some(c) => c,
        None => return false,
    };
    match &cmd.route {
        Route::Local(Local::Help) => push_msg(tab, "System", &cmd_help_text()),
        Route::Local(Local::Status) => {
            let t = status_text(tab);
            push_msg(tab, "System", &t);
        }
        Route::Clear => {
            if tab.sess.borrow().workdir.is_none() {
                push_msg(tab, "System", "Choose a folder first.");
                return true;
            }
            {
                let mut s = tab.sess.borrow_mut();
                s.session_id = None;
                s.pending_approval = false;
                s.pending_dirs.clear();
                s.cmd_pending = false;
                s.cmd_recovering = false;
            }
            tab.msgs.borrow_mut().clear();
            tab.stream.borrow_mut().clear();
            tab.approve.set_sensitive(false);
            push_msg(
                tab,
                "System",
                "Cleared — fresh session (previous context dropped).",
            );
            spawn_proc(tab, false);
        }
        Route::RespawnFlag(flag) => {
            let flag: &str = flag;
            if arg.is_empty() {
                let cur = {
                    let s = tab.sess.borrow();
                    if flag == "--model" {
                        s.model.clone()
                    } else {
                        s.overrides
                            .iter()
                            .find(|(f, _)| f == flag)
                            .map(|(_, v)| v.clone())
                    }
                };
                let cur = cur.unwrap_or_else(|| "(CLI default)".into());
                push_msg(
                    tab,
                    "System",
                    &format!("{name}: current = {cur}\nUsage: {}", cmd.usage),
                );
                return true;
            }
            if tab.sess.borrow().workdir.is_none() {
                push_msg(tab, "System", "Choose a folder first.");
                return true;
            }
            {
                let mut s = tab.sess.borrow_mut();
                set_override(&mut s, flag, &arg);
                s.cmd_pending = true;
                s.cmd_recovering = false;
            }
            push_msg(
                tab,
                "System",
                &format!("{name} → {arg} (restarting session; context kept)"),
            );
            spawn_proc(tab, false);
        }
    }
    true
}

fn build_session_tab(
    window: &ApplicationWindow,
    bin: Rc<String>,
    resume: Option<(PathBuf, String)>,
) -> gtk::Widget {
    let sess = Rc::new(RefCell::new(Session::default()));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pick = gtk::Button::with_label("Choose folder…");
    let dir_label = gtk::Label::new(Some("No folder"));
    let mode = gtk::DropDown::from_strings(&MODE_LABELS);
    mode.set_selected(0);
    let status = gtk::Label::new(Some(""));
    top.append(&pick);
    top.append(&dir_label);
    top.append(&gtk::Label::new(Some("  mode:")));
    top.append(&mode);
    top.append(&status);

    let web = webkit6::WebView::new();
    web.set_vexpand(true);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some(
        "Message Claude Code…  (Enter to send · /help for commands)",
    ));
    entry.set_sensitive(false);
    let img = gtk::Button::with_label("📎 Image");
    img.set_tooltip_text(Some("Paste an image from the clipboard and send it"));
    img.set_sensitive(false);
    let file = gtk::Button::with_label("📄 File");
    file.set_tooltip_text(Some("Insert a file path into the message"));
    file.set_sensitive(false);
    let approve = gtk::Button::with_label("Approve");
    approve.set_sensitive(false);
    let stop = gtk::Button::with_label("■ Stop");
    stop.set_tooltip_text(Some(
        "Interrupt the current turn (restarts the session with --resume; context kept)",
    ));
    stop.set_sensitive(false);
    let send = gtk::Button::with_label("Send");
    send.set_sensitive(false);
    bottom.append(&entry);
    bottom.append(&img);
    bottom.append(&file);
    bottom.append(&approve);
    bottom.append(&stop);
    bottom.append(&send);

    root.append(&top);
    root.append(&web);
    root.append(&bottom);

    let tab = Tab {
        sess: sess.clone(),
        bin,
        msgs: Rc::new(RefCell::new(Vec::new())),
        stream: Rc::new(RefCell::new(String::new())),
        render_pending: Rc::new(Cell::new(false)),
        gen: Rc::new(Cell::new(0)),
        web: web.clone(),
        entry: entry.clone(),
        img: img.clone(),
        file: file.clone(),
        send: send.clone(),
        stop: stop.clone(),
        approve: approve.clone(),
        mode: mode.clone(),
        status: status.clone(),
    };
    render(&tab);

    {
        let tab_fp = tab.clone();
        let dir_label = dir_label.clone();
        let window = window.clone();
        pick.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose folder").build();
            let tab_fp = tab_fp.clone();
            let dir_label = dir_label.clone();
            dialog.select_folder(Some(&window), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        {
                            let mut s = tab_fp.sess.borrow_mut();
                            s.stdin = None;
                            if let Some(mut c) = s.child.take() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            *s = Session {
                                workdir: Some(path.clone()),
                                ..Session::default()
                            };
                        }
                        tab_fp.msgs.borrow_mut().clear();
                        tab_fp.stream.borrow_mut().clear();
                        dir_label.set_text(&path.to_string_lossy());
                        ui_idle(&tab_fp);
                        tab_fp.approve.set_sensitive(false);
                        push_msg(
                            &tab_fp,
                            "System",
                            "Folder set. Persistent session started; streaming on.",
                        );
                        spawn_proc(&tab_fp, false);
                    }
                }
            });
        });
    }

    {
        let tab_s = tab.clone();
        send.connect_clicked(move |_| {
            let msg = tab_s.entry.text().to_string();
            if msg.trim().is_empty() {
                return;
            }
            // Route a leading `/` through the command dispatcher. A recognised
            // command is consumed here; an unknown one falls through and is
            // sent verbatim (Route D).
            if msg.trim_start().starts_with('/') && handle_command(&tab_s, msg.trim()) {
                tab_s.entry.set_text("");
                return;
            }
            if tab_s.sess.borrow().stdin.is_none() {
                push_msg(&tab_s, "System", "No running session. Choose a folder first.");
                return;
            }
            tab_s.entry.set_text("");
            push_msg(&tab_s, "You", &msg);
            send_turn(&tab_s, &msg);
        });
    }
    {
        let send = send.clone();
        entry.connect_activate(move |_| send.emit_clicked());
    }

    {
        let tab_a = tab.clone();
        approve.connect_clicked(move |_| {
            let ok = {
                let mut s = tab_a.sess.borrow_mut();
                if !s.pending_approval {
                    false
                } else {
                    let dirs = std::mem::take(&mut s.pending_dirs);
                    for d in dirs {
                        if !s.allowed_dirs.contains(&d) {
                            s.allowed_dirs.push(d);
                        }
                    }
                    s.pending_approval = false;
                    true
                }
            };
            if !ok {
                return;
            }
            push_msg(&tab_a, "You", "[Approved — restarting session with access, continuing]");
            spawn_proc(&tab_a, true);
            send_turn(&tab_a, "Approved. Proceed with the action you described.");
        });
    }

    // "📎 Image": read an image off the clipboard, write it into the session
    // workdir as a dotfile (Read inside the workdir needs no permission), then
    // send a turn that points Claude at the absolute path. Any text already in
    // the entry rides along as the accompanying question.
    {
        let tab_i = tab.clone();
        img.connect_clicked(move |b| {
            let wd = match tab_i.sess.borrow().workdir.clone() {
                Some(w) => w,
                None => {
                    push_msg(&tab_i, "System", "Choose a folder first.");
                    return;
                }
            };
            if tab_i.sess.borrow().stdin.is_none() {
                push_msg(&tab_i, "System", "No running session.");
                return;
            }
            let cb = b.clipboard();
            let tab_c = tab_i.clone();
            cb.read_texture_async(gtk::gio::Cancellable::NONE, move |res| match res {
                Ok(Some(tex)) => {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let fname = format!(".ccgui-paste-{ts}.png");
                    let path = wd.join(&fname);
                    match tex.save_to_png(&path) {
                        Ok(()) => {
                            let extra = tab_c.entry.text().to_string();
                            tab_c.entry.set_text("");
                            push_msg(
                                &tab_c,
                                "You",
                                &format!("[pasted image: {fname}] {extra}"),
                            );
                            let p = path.to_string_lossy();
                            let msg = format!(
                                "我粘贴了一张图片，请先用 Read 工具查看这个文件，再回答：\n{p}\n{extra}"
                            );
                            send_turn(&tab_c, &msg);
                        }
                        Err(e) => push_msg(
                            &tab_c,
                            "System",
                            &format!("Failed to save pasted image: {e}"),
                        ),
                    }
                }
                Ok(None) => {
                    push_msg(&tab_c, "System", "Clipboard has no image.")
                }
                Err(e) => {
                    push_msg(&tab_c, "System", &format!("Clipboard read failed: {e}"))
                }
            });
        });
    }

    {
        let tab_m = tab.clone();
        mode.connect_selected_notify(move |_| {
            if tab_m.sess.borrow().workdir.is_some() {
                push_msg(&tab_m, "System", "Mode changed — restarting session (context kept).");
                spawn_proc(&tab_m, false);
            }
        });
    }

    // ② @file: pick a file, insert its path (workdir-relative when inside the
    // workdir) into the entry. NOT the client's @-mention engine — `-p` mode
    // treats it as literal text; reads stay under the existing permissions.
    {
        let tab_f = tab.clone();
        let window = window.clone();
        file.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Insert file path").build();
            let tab_f = tab_f.clone();
            dialog.open(Some(&window), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        let txt = {
                            let s = tab_f.sess.borrow();
                            match &s.workdir {
                                Some(wd) => p
                                    .strip_prefix(wd)
                                    .map(|r| r.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| p.to_string_lossy().to_string()),
                                None => p.to_string_lossy().to_string(),
                            }
                        };
                        let e = &tab_f.entry;
                        let mut pos = e.position();
                        let ins = if e.text().is_empty() {
                            txt
                        } else {
                            format!(" {txt} ")
                        };
                        e.insert_text(&ins, &mut pos);
                        e.set_position(pos);
                        e.grab_focus();
                    }
                }
            });
        });
    }

    // ① Stop: reliable path only — kill + respawn with `--resume` (session_id
    // kept). Control-protocol interrupt is intentionally NOT done here: it
    // would need a runtime probe we cannot do, and the DESIGN's A fallback is
    // correct and sufficient. A turn killed mid-flight may not persist.
    {
        let tab_st = tab.clone();
        stop.connect_clicked(move |_| {
            if tab_st.sess.borrow().child.is_none() {
                return;
            }
            spawn_proc(&tab_st, false);
            tab_st.stream.borrow_mut().clear();
            tab_st.status.set_text("");
            push_msg(
                &tab_st,
                "System",
                "■ Stopped — session restarted (context kept).",
            );
            ui_idle(&tab_st);
            tab_st.approve.set_sensitive(false);
        });
    }

    // ③ `/` autocomplete from the COMMANDS registry only. autohide=false so
    // the popover never steals focus from the entry while typing.
    {
        let pop = gtk::Popover::new();
        pop.set_parent(&entry);
        pop.set_autohide(false);
        pop.set_has_arrow(false);
        pop.set_position(gtk::PositionType::Top);
        let list = gtk::ListBox::new();
        list.set_activate_on_single_click(true);
        let sc = gtk::ScrolledWindow::new();
        sc.set_min_content_width(380);
        sc.set_max_content_height(180);
        sc.set_propagate_natural_height(true);
        sc.set_child(Some(&list));
        pop.set_child(Some(&sc));
        let names: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let pop = pop.clone();
            let list = list.clone();
            let names = names.clone();
            entry.connect_changed(move |e| {
                let t = e.text();
                let t = t.as_str();
                let tok = t.split_whitespace().next().unwrap_or("");
                if !t.starts_with('/') || tok.is_empty() {
                    pop.popdown();
                    return;
                }
                while let Some(c) = list.first_child() {
                    list.remove(&c);
                }
                names.borrow_mut().clear();
                let mut any = false;
                for c in COMMANDS {
                    if c.name.starts_with(tok) && c.name != tok {
                        let lbl = gtk::Label::new(Some(&format!(
                            "{}   —   {}",
                            c.name, c.usage
                        )));
                        lbl.set_xalign(0.0);
                        list.append(&lbl);
                        names.borrow_mut().push(c.name);
                        any = true;
                    }
                }
                if any {
                    pop.popup();
                } else {
                    pop.popdown();
                }
            });
        }
        {
            let entry = entry.clone();
            let pop = pop.clone();
            list.connect_row_activated(move |_, row| {
                let i = row.index();
                if i < 0 {
                    return;
                }
                if let Some(name) = names.borrow().get(i as usize).copied() {
                    entry.set_text(&format!("{name} "));
                    entry.set_position(-1);
                    pop.popdown();
                    entry.grab_focus();
                }
            });
        }
    }

    // ④ Resume bootstrap: pre-load workdir + session id and start with
    // `--resume` (the existing spawn path).
    if let Some((wd, sid)) = resume {
        {
            let mut s = tab.sess.borrow_mut();
            *s = Session {
                workdir: Some(wd.clone()),
                session_id: Some(sid.clone()),
                ..Session::default()
            };
        }
        dir_label.set_text(&wd.to_string_lossy());
        ui_idle(&tab);
        tab.approve.set_sensitive(false);
        push_msg(
            &tab,
            "System",
            &format!("Resuming session {sid}\n{}", wd.to_string_lossy()),
        );
        spawn_proc(&tab, false);
    }

    root.upcast()
}

fn add_tab(
    notebook: &gtk::Notebook,
    window: &ApplicationWindow,
    bin: Rc<String>,
    resume: Option<(PathBuf, String)>,
) {
    let page = build_session_tab(window, bin, resume);
    let n = notebook.n_pages() + 1;
    let label = gtk::Label::new(Some(&format!("Session {n}")));
    let idx = notebook.append_page(&page, Some(&label));
    notebook.set_current_page(Some(idx));
}

// ── Read-only management panel (see DESIGN.md) ───────────────────────────
//
// Thin transport: shell out to `claude plugin …` (JSON where available) or
// read config files directly; parse defensively; never execute a server or
// print a secret. v1 is browse-only — no installs, no config writes.

#[derive(Clone, Copy)]
enum ManageKind {
    PluginsInstalled,
    PluginsAvailable,
    Marketplaces,
    Mcp,
    Skills,
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}\n…(truncated)", &s[..n])
    } else {
        s.to_string()
    }
}

fn claude_json(bin: &str, args: &[&str]) -> Result<serde_json::Value, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run `claude {}`: {e}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!(
            "`claude {}` failed ({})\n{}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "could not parse JSON from `claude {}` ({e}); raw output:\n{}",
            args.join(" "),
            trunc(stdout.trim(), 4000)
        )
    })
}

fn fmt_plugins(v: &serde_json::Value) -> String {
    let arr = v
        .as_array()
        .cloned()
        .or_else(|| v.get("plugins").and_then(|p| p.as_array()).cloned());
    let arr = match arr {
        Some(a) => a,
        None => {
            return format!(
                "(unexpected JSON shape; showing raw)\n{}",
                trunc(&serde_json::to_string_pretty(v).unwrap_or_default(), 4000)
            )
        }
    };
    if arr.is_empty() {
        return "(none)".into();
    }
    let mut s = String::new();
    for it in &arr {
        let g = |k: &str| it.get(k).and_then(|x| x.as_str()).unwrap_or("");
        let name = if g("name").is_empty() {
            it.get("name").map(|x| x.to_string()).unwrap_or_default()
        } else {
            g("name").to_string()
        };
        let ver = g("version");
        let mkt = g("marketplace");
        let enabled = it
            .get("enabled")
            .and_then(|x| x.as_bool())
            .map(|b| if b { "enabled" } else { "disabled" })
            .unwrap_or("");
        let desc = g("description");
        s.push_str(&format!("• {name}"));
        if !ver.is_empty() {
            s.push_str(&format!("  v{ver}"));
        }
        if !mkt.is_empty() {
            s.push_str(&format!("  @{mkt}"));
        }
        if !enabled.is_empty() {
            s.push_str(&format!("  [{enabled}]"));
        }
        s.push('\n');
        if !desc.is_empty() {
            s.push_str(&format!("    {desc}\n"));
        }
    }
    s
}

/// Walk ~/.claude.json for every `mcpServers` map (top-level = user scope,
/// `projects.<path>.mcpServers` = that project). Secrets are never printed.
fn report_mcp() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.claude.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => return format!("cannot read {path}: {e}"),
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return format!("{path} is not valid JSON: {e}"),
    };
    let mut out = String::new();
    let mut emit = |scope: &str, servers: &serde_json::Value| {
        if let Some(map) = servers.as_object() {
            if map.is_empty() {
                return;
            }
            out.push_str(&format!("[{scope}]\n"));
            for (name, cfg) in map {
                let t = cfg
                    .get("type")
                    .or_else(|| cfg.get("transport"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("stdio");
                let loc = cfg
                    .get("url")
                    .and_then(|x| x.as_str())
                    .map(|u| {
                        // host only — no query/token in path
                        u.split('/').take(3).collect::<Vec<_>>().join("/")
                    })
                    .or_else(|| {
                        cfg.get("command").and_then(|x| x.as_str()).map(String::from)
                    })
                    .unwrap_or_default();
                let nenv = cfg
                    .get("env")
                    .and_then(|x| x.as_object())
                    .map(|m| m.len())
                    .unwrap_or(0);
                let nhdr = cfg
                    .get("headers")
                    .and_then(|x| x.as_object())
                    .map(|m| m.len())
                    .unwrap_or(0);
                out.push_str(&format!("  • {name}  ({t})  {loc}"));
                if nenv > 0 {
                    out.push_str(&format!("  (env: {nenv} hidden)"));
                }
                if nhdr > 0 {
                    out.push_str(&format!("  (headers: {nhdr} hidden)"));
                }
                out.push('\n');
            }
        }
    };
    if let Some(s) = v.get("mcpServers") {
        emit("user", s);
    }
    if let Some(projs) = v.get("projects").and_then(|p| p.as_object()) {
        for (proj, pv) in projs {
            if let Some(s) = pv.get("mcpServers") {
                emit(proj, s);
            }
        }
    }
    if out.is_empty() {
        "(no mcpServers configured in ~/.claude.json)".into()
    } else {
        out.push_str("\nSecrets (env/headers values) are intentionally hidden.");
        out
    }
}

fn scan_skills(dir: &Path, label: &str, out: &mut String) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut any = false;
    for ent in rd.flatten() {
        if !ent.path().is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        let sk = ent.path().join("SKILL.md");
        let desc = std::fs::read_to_string(&sk)
            .ok()
            .and_then(|c| {
                c.lines()
                    .map(|l| l.trim())
                    .find(|l| {
                        !l.is_empty() && !l.starts_with("---") && !l.starts_with('#')
                    })
                    .map(|l| l.chars().take(100).collect::<String>())
            })
            .unwrap_or_default();
        if !any {
            out.push_str(&format!("[{label}] {}\n", dir.display()));
            any = true;
        }
        out.push_str(&format!("  • {name}"));
        if !desc.is_empty() {
            out.push_str(&format!(" — {desc}"));
        }
        out.push('\n');
    }
}

fn report_skills() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = String::new();
    scan_skills(Path::new(&format!("{home}/.claude/skills")), "user", &mut out);
    if let Ok(cwd) = std::env::current_dir() {
        scan_skills(&cwd.join(".claude/skills"), "project (cwd)", &mut out);
    }
    if out.is_empty() {
        out.push_str("(no filesystem skills in ~/.claude/skills or ./.claude/skills)\n");
    }
    out.push_str(
        "\nPlugin-bundled skills are not listed here — see the Plugins tabs \
         (a skill ships inside its plugin).",
    );
    out
}

fn manage_fetch(kind: ManageKind, bin: &str) -> String {
    match kind {
        ManageKind::PluginsInstalled => {
            match claude_json(bin, &["plugin", "list", "--json"]) {
                Ok(v) => fmt_plugins(&v),
                Err(e) => e,
            }
        }
        ManageKind::PluginsAvailable => {
            match claude_json(bin, &["plugin", "list", "--available", "--json"]) {
                Ok(v) => fmt_plugins(&v),
                Err(e) => e,
            }
        }
        ManageKind::Marketplaces => {
            match Command::new(bin)
                .args(["plugin", "marketplace", "list"])
                .output()
            {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    let e = String::from_utf8_lossy(&o.stderr);
                    let body = if s.trim().is_empty() { e } else { s };
                    let body = body.trim();
                    if body.is_empty() {
                        "(no marketplaces configured)".into()
                    } else {
                        trunc(body, 8000)
                    }
                }
                Err(e) => format!("failed to run `claude plugin marketplace list`: {e}"),
            }
        }
        ManageKind::Mcp => report_mcp(),
        ManageKind::Skills => report_skills(),
    }
}

// v2: confirmed mutating action. Shows the exact `claude …` command, runs
// it off-thread only on confirm, prints stdout+stderr, then refreshes.
// Secrets are never placed in argv by the UI itself (MCP add is deferred).
// `pretty` is the secret-free display string (shown in the dialog AND echoed
// in the output). `argv` is what actually runs. For commands with no secrets
// callers pass `format!("claude {}", argv.join(" "))`; MCP add passes a
// redacted form so env/header values never reach the UI, dialog, or log.
fn confirm_and_run(
    win: &gtk::Window,
    bin: Rc<String>,
    argv: Vec<String>,
    pretty: String,
    view: gtk::TextView,
    status: gtk::Label,
    reload: Rc<dyn Fn()>,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Run this command?")
        .detail(format!(
            "{pretty}\n\nThis runs the Claude Code CLI and may execute \
             third-party code on your machine."
        ))
        .buttons(["Cancel", "Run"].as_slice())
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.choose(Some(win), gtk::gio::Cancellable::NONE, move |res| {
        if !matches!(res, Ok(1)) {
            return; // Cancel / Esc
        }
        status.set_text("running…");
        let (tx, rx) = async_channel::bounded::<String>(1);
        let bin2 = (*bin).clone();
        let argv2 = argv.clone();
        let shown = pretty.clone();
        std::thread::spawn(move || {
            let msg = match Command::new(&bin2).args(&argv2).output() {
                Ok(o) => format!(
                    "$ {}\n[exit {}]\n\n{}{}",
                    shown,
                    o.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                ),
                Err(e) => format!("$ {shown}\n\nfailed to launch: {e}"),
            };
            let _ = tx.send_blocking(msg);
        });
        let view = view.clone();
        let status = status.clone();
        let reload = reload.clone();
        glib::spawn_future_local(async move {
            if let Ok(text) = rx.recv().await {
                view.buffer().set_text(&text);
                status.set_text("");
                reload(); // re-list so the change is visible
            }
        });
    });
}

fn manage_page(win: &gtk::Window, bin: Rc<String>, kind: ManageKind) -> gtk::Widget {
    let v = gtk::Box::new(gtk::Orientation::Vertical, 6);
    v.set_margin_top(6);
    v.set_margin_bottom(6);
    v.set_margin_start(6);
    v.set_margin_end(6);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let refresh = gtk::Button::with_label("↻ Refresh");
    let status = gtk::Label::new(Some("loading…"));
    bar.append(&refresh);
    bar.append(&status);

    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&view));

    v.append(&bar);

    let reload: Rc<dyn Fn()> = {
        let view = view.clone();
        let status = status.clone();
        let bin = bin.clone();
        Rc::new(move || {
            status.set_text("loading…");
            view.buffer().set_text("");
            let (tx, rx) = async_channel::bounded::<String>(1);
            let bin2 = (*bin).clone();
            std::thread::spawn(move || {
                let _ = tx.send_blocking(manage_fetch(kind, &bin2));
            });
            let view = view.clone();
            let status = status.clone();
            glib::spawn_future_local(async move {
                if let Ok(text) = rx.recv().await {
                    view.buffer().set_text(&text);
                    status.set_text("");
                }
            });
        })
    };
    refresh.connect_clicked({
        let reload = reload.clone();
        move |_| reload()
    });

    // v2 action bar (confirmed mutations). Read-only listing stays above.
    let mut spec_buttons: Vec<(&str, &str, bool)> = Vec::new(); // label, subcmd, allow-empty
    let (placeholder, prefix): (&str, Vec<&str>) = match kind {
        ManageKind::PluginsInstalled | ManageKind::PluginsAvailable => {
            spec_buttons = vec![
                ("Install", "install", false),
                ("Enable", "enable", false),
                ("Disable", "disable", false),
                ("Uninstall", "uninstall", false),
                ("Update", "update", false),
            ];
            ("plugin  or  plugin@marketplace", vec!["plugin"])
        }
        ManageKind::Marketplaces => {
            spec_buttons = vec![
                ("Add", "add", false),
                ("Remove", "remove", false),
                ("Update", "update", true), // blank = update all
            ];
            (
                "<url | github-owner/repo | path>  (Update: blank = all)",
                vec!["plugin", "marketplace"],
            )
        }
        ManageKind::Mcp => {
            spec_buttons = vec![("Remove", "remove", false)];
            ("MCP server name to remove", vec!["mcp"])
        }
        ManageKind::Skills => ("", vec![]),
    };

    if !spec_buttons.is_empty() {
        let abar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let ent = gtk::Entry::new();
        ent.set_hexpand(true);
        ent.set_placeholder_text(Some(placeholder));
        abar.append(&ent);
        for (label, sub, allow_empty) in spec_buttons {
            let b = gtk::Button::with_label(label);
            let ent = ent.clone();
            let win = win.clone();
            let bin = bin.clone();
            let view = view.clone();
            let status = status.clone();
            let reload = reload.clone();
            let prefix: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
            let sub = sub.to_string();
            b.connect_clicked(move |_| {
                let spec = ent.text().trim().to_string();
                if spec.is_empty() && !allow_empty {
                    status.set_text("enter a value first");
                    return;
                }
                let mut argv = prefix.clone();
                argv.push(sub.clone());
                if !spec.is_empty() {
                    argv.push(spec.clone());
                }
                let pretty = format!("claude {}", argv.join(" "));
                confirm_and_run(
                    &win,
                    bin.clone(),
                    argv,
                    pretty,
                    view.clone(),
                    status.clone(),
                    reload.clone(),
                );
            });
            abar.append(&b);
        }
        v.append(&abar);
    }

    // v2.1: MCP "Add server" form. env/header VALUES are passed to the CLI
    // but never shown in the dialog, the echoed command, or the output.
    if matches!(kind, ManageKind::Mcp) {
        let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let row1 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Entry::new();
        name.set_placeholder_text(Some("server name"));
        name.set_hexpand(true);
        let scope = gtk::DropDown::from_strings(&["user", "project", "local"]);
        let transport = gtk::DropDown::from_strings(&["stdio", "http", "sse"]);
        row1.append(&gtk::Label::new(Some("name")));
        row1.append(&name);
        row1.append(&gtk::Label::new(Some("scope")));
        row1.append(&scope);
        row1.append(&gtk::Label::new(Some("transport")));
        row1.append(&transport);

        let target = gtk::Entry::new();
        target.set_hexpand(true);
        target.set_placeholder_text(Some(
            "stdio: command line (e.g. npx my-mcp-server)   |   http/sse: URL",
        ));
        let env = gtk::Entry::new();
        env.set_hexpand(true);
        env.set_placeholder_text(Some(
            "stdio env (optional): KEY=VAL KEY2=VAL2  — values hidden after Add",
        ));
        let headers = gtk::Entry::new();
        headers.set_hexpand(true);
        headers.set_placeholder_text(Some(
            "http/sse headers (optional): Name: value ;; Name2: value2  — values hidden",
        ));

        let addrow = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let addbtn = gtk::Button::with_label("Add server");
        let hint = gtk::Label::new(Some("env/header values are never displayed or logged"));
        hint.set_xalign(0.0);
        addrow.append(&addbtn);
        addrow.append(&hint);

        form.append(&row1);
        form.append(&target);
        form.append(&env);
        form.append(&headers);
        form.append(&addrow);

        {
            let win = win.clone();
            let bin = bin.clone();
            let view = view.clone();
            let status = status.clone();
            let reload = reload.clone();
            let (name, scope, transport, target, env, headers) = (
                name.clone(),
                scope.clone(),
                transport.clone(),
                target.clone(),
                env.clone(),
                headers.clone(),
            );
            addbtn.connect_clicked(move |_| {
                let nm = name.text().trim().to_string();
                let tg = target.text().trim().to_string();
                if nm.is_empty() || tg.is_empty() {
                    status.set_text("name and command/URL are required");
                    return;
                }
                let sc = ["user", "project", "local"]
                    .get(scope.selected() as usize)
                    .copied()
                    .unwrap_or("user");
                let tr = ["stdio", "http", "sse"]
                    .get(transport.selected() as usize)
                    .copied()
                    .unwrap_or("stdio");

                let mut argv: Vec<String> = vec![
                    "mcp".into(),
                    "add".into(),
                    "-s".into(),
                    sc.into(),
                    "--transport".into(),
                    tr.into(),
                ];
                // Redacted echo built in lockstep; secret values become ***.
                let mut shown = format!("claude mcp add -s {sc} --transport {tr}");

                if tr == "stdio" {
                    for tok in env.text().split_whitespace() {
                        if let Some((k, _)) = tok.split_once('=') {
                            argv.push("-e".into());
                            argv.push(tok.to_string());
                            shown.push_str(&format!(" -e {k}=***"));
                        }
                    }
                } else {
                    for h in headers.text().split(";;") {
                        let h = h.trim();
                        if h.is_empty() {
                            continue;
                        }
                        let hn = h.split(':').next().unwrap_or(h).trim();
                        argv.push("--header".into());
                        argv.push(h.to_string());
                        shown.push_str(&format!(" --header \"{hn}: ***\""));
                    }
                }

                argv.push(nm.clone());
                shown.push_str(&format!(" {nm}"));
                if tr == "stdio" {
                    argv.push("--".into());
                    argv.extend(tg.split_whitespace().map(String::from));
                    shown.push_str(&format!(" -- {tg}"));
                } else {
                    argv.push(tg.clone());
                    shown.push_str(&format!(" {tg}"));
                }

                confirm_and_run(
                    &win,
                    bin.clone(),
                    argv,
                    shown,
                    view.clone(),
                    status.clone(),
                    reload.clone(),
                );
            });
        }
        v.append(&form);
    }

    let note = match kind {
        ManageKind::Skills => Some(
            "No first-party skill install. Install a plugin that bundles \
             skills via the Plugins tabs.",
        ),
        _ => None,
    };
    if let Some(n) = note {
        let l = gtk::Label::new(Some(n));
        l.set_wrap(true);
        l.set_xalign(0.0);
        v.append(&l);
    }

    v.append(&scroll);
    reload();
    v.upcast()
}

fn open_manage_window(parent: &ApplicationWindow, bin: Rc<String>) {
    let win = gtk::Window::builder()
        .title("Manage — Plugins / MCP / Skills")
        .transient_for(parent)
        .default_width(840)
        .default_height(640)
        .build();
    let nb = gtk::Notebook::new();
    nb.set_scrollable(true);
    for (title, kind) in [
        ("Plugins (installed)", ManageKind::PluginsInstalled),
        ("Plugins (available)", ManageKind::PluginsAvailable),
        ("Marketplaces", ManageKind::Marketplaces),
        ("MCP servers", ManageKind::Mcp),
        ("Skills", ManageKind::Skills),
    ] {
        let page = manage_page(&win, bin.clone(), kind);
        nb.append_page(&page, Some(&gtk::Label::new(Some(title))));
    }
    win.set_child(Some(&nb));
    win.present();
}

// ④ Scan ~/.claude/projects/<proj>/*.jsonl for resumable sessions. The
// on-disk shape is parsed defensively: session id = file stem, cwd from any
// line carrying a "cwd" string, label = first user message. Files without a
// cwd are skipped (cannot resume without one). Returns newest first.
fn scan_sessions() -> Vec<(std::time::SystemTime, String, PathBuf, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let root = PathBuf::from(format!("{home}/.claude/projects"));
    let mut out: Vec<(std::time::SystemTime, String, PathBuf, String)> = Vec::new();
    let projs = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for proj in projs.flatten() {
        let files = match std::fs::read_dir(proj.path()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let sid = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            let file = match std::fs::File::open(&path) {
                Ok(fl) => fl,
                Err(_) => continue,
            };
            let mut cwd: Option<String> = None;
            let mut label: Option<String> = None;
            for line in BufReader::new(file).lines().map_while(Result::ok).take(80) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if cwd.is_none() {
                    if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                        cwd = Some(c.to_string());
                    }
                }
                if label.is_none() {
                    let is_user = v.get("type").and_then(|t| t.as_str()) == Some("user")
                        || v.get("message")
                            .and_then(|m| m.get("role"))
                            .and_then(|r| r.as_str())
                            == Some("user");
                    if is_user {
                        let content = v
                            .get("message")
                            .and_then(|m| m.get("content"));
                        let txt = content
                            .and_then(|c| c.as_str().map(String::from))
                            .or_else(|| {
                                content
                                    .and_then(|c| c.as_array())
                                    .and_then(|a| a.first())
                                    .and_then(|b| b.get("text"))
                                    .and_then(|t| t.as_str())
                                    .map(String::from)
                            });
                        if let Some(t) = txt {
                            let t = t.trim().replace('\n', " ");
                            if !t.is_empty() {
                                label = Some(t.chars().take(80).collect());
                            }
                        }
                    }
                }
                if cwd.is_some() && label.is_some() {
                    break;
                }
            }
            if let Some(c) = cwd {
                out.push((
                    mtime,
                    sid,
                    PathBuf::from(c),
                    label.unwrap_or_else(|| "(no preview)".into()),
                ));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(200);
    out
}

fn open_resume_window(
    notebook: &gtk::Notebook,
    parent: &ApplicationWindow,
    bin: Rc<String>,
) {
    let win = gtk::Window::builder()
        .title("Resume a session")
        .transient_for(parent)
        .default_width(760)
        .default_height(560)
        .build();
    let v = gtk::Box::new(gtk::Orientation::Vertical, 6);
    v.set_margin_top(6);
    v.set_margin_bottom(6);
    v.set_margin_start(6);
    v.set_margin_end(6);
    let status = gtk::Label::new(Some("scanning ~/.claude/projects…"));
    status.set_xalign(0.0);
    let list = gtk::ListBox::new();
    list.set_activate_on_single_click(false); // double-click / Enter to resume
    let sc = gtk::ScrolledWindow::new();
    sc.set_vexpand(true);
    sc.set_child(Some(&list));
    v.append(&status);
    v.append(&sc);
    win.set_child(Some(&v));

    let picks: Rc<RefCell<Vec<(PathBuf, String)>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let (tx, rx) =
            async_channel::bounded::<Vec<(std::time::SystemTime, String, PathBuf, String)>>(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(scan_sessions());
        });
        let list = list.clone();
        let status = status.clone();
        let picks = picks.clone();
        glib::spawn_future_local(async move {
            if let Ok(rows) = rx.recv().await {
                if rows.is_empty() {
                    status.set_text("No resumable sessions found in ~/.claude/projects.");
                    return;
                }
                status.set_text(&format!("{} session(s) — double-click to resume", rows.len()));
                for (_, sid, cwd, label) in rows {
                    let short = sid.chars().take(8).collect::<String>();
                    let lbl = gtk::Label::new(Some(&format!(
                        "{}\n{}   ·   {}",
                        label,
                        cwd.to_string_lossy(),
                        short
                    )));
                    lbl.set_xalign(0.0);
                    list.append(&lbl);
                    picks.borrow_mut().push((cwd, sid));
                }
            }
        });
    }
    {
        let notebook = notebook.clone();
        let parent = parent.clone();
        let bin = bin.clone();
        let picks = picks.clone();
        let win = win.clone();
        list.connect_row_activated(move |_, row| {
            let i = row.index();
            if i < 0 {
                return;
            }
            if let Some((cwd, sid)) = picks.borrow().get(i as usize).cloned() {
                add_tab(&notebook, &parent, bin.clone(), Some((cwd, sid)));
                win.close();
            }
        });
    }
    win.present();
}

fn build_ui(app: &Application) {
    let bin = Rc::new(resolve_claude());

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toolbar.set_margin_top(6);
    toolbar.set_margin_start(6);
    let new_btn = gtk::Button::with_label("➕ New session");
    toolbar.append(&new_btn);
    let manage_btn = gtk::Button::with_label("🧩 Manage");
    manage_btn.set_tooltip_text(Some(
        "Browse installed/available plugins, MCP servers, skills (read-only)",
    ));
    toolbar.append(&manage_btn);
    let resume_btn = gtk::Button::with_label("⟲ Resume…");
    resume_btn.set_tooltip_text(Some(
        "Resume a previous session from ~/.claude/projects",
    ));
    toolbar.append(&resume_btn);

    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    notebook.set_scrollable(true);

    vbox.append(&toolbar);
    vbox.append(&notebook);

    // Make the bundled SVG resolvable by name. NOTE: a reliable dock/taskbar
    // icon on Linux comes from the installed .desktop + themed icon
    // (packaging); this runtime path is best-effort and compositor-dependent.
    if let Some(disp) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&disp)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"));
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Claude Code — Linux GUI (v0.5.2)")
        .default_width(1100)
        .default_height(780)
        .child(&vbox)
        .build();
    window.set_icon_name(Some(APP_ID));

    {
        let notebook = notebook.clone();
        let window = window.clone();
        let bin = bin.clone();
        new_btn.connect_clicked(move |_| add_tab(&notebook, &window, bin.clone(), None));
    }
    {
        let window = window.clone();
        let bin = bin.clone();
        manage_btn.connect_clicked(move |_| open_manage_window(&window, bin.clone()));
    }
    {
        let notebook = notebook.clone();
        let window = window.clone();
        let bin = bin.clone();
        resume_btn.connect_clicked(move |_| {
            open_resume_window(&notebook, &window, bin.clone())
        });
    }

    add_tab(&notebook, &window, bin.clone(), None);
    window.present();
}

fn main() -> glib::ExitCode {
    // Align the X11 WM_CLASS with APP_ID so GNOME associates the window with
    // the installed .desktop (StartupWMClass=APP_ID) and shows its icon.
    // Without this, `cargo run` yields WM_CLASS from the binary name and the
    // dock falls back to a generic icon.
    glib::set_prgname(Some(APP_ID));
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
