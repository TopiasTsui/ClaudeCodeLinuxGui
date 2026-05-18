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
use std::time::Duration;

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
    cost: f64,
    denials: Vec<String>,
    denied_dirs: Vec<String>,
}

enum Ev {
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
    total_cost: f64,
    pending_approval: bool,
    pending_dirs: Vec<String>,
    allowed_dirs: Vec<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
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
    send: gtk::Button,
    approve: gtk::Button,
    mode: gtk::DropDown,
    status: gtk::Label,
}

fn push_msg(tab: &Tab, who: &str, text: &str) {
    tab.msgs.borrow_mut().push((who.to_string(), text.to_string()));
    render(tab);
}

fn parse_result(v: &serde_json::Value) -> TurnResult {
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .unwrap_or("(empty response)")
        .to_string();
    let session_id = v.get("session_id").and_then(|x| x.as_str()).map(str::to_string);
    let cost = v.get("total_cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0);
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
    TurnResult { result, session_id, cost, denials, denied_dirs }
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

    let pm = if force_accept_edits {
        Some("acceptEdits")
    } else {
        mode_flag(tab.mode.selected())
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
    if let Some(m) = pm {
        cmd.arg("--permission-mode").arg(m);
    }
    for d in &allowed_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            push_msg(tab, "System", &format!("Failed to launch claude: {e}"));
            return;
        }
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();

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
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            if tab.gen.get() != my_gen {
                break; // a newer process superseded this one — stop silently
            }
            match ev {
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
                    let total = {
                        let mut s = tab.sess.borrow_mut();
                        if let Some(sid) = o.session_id {
                            s.session_id = Some(sid);
                        }
                        s.total_cost += o.cost;
                        s.total_cost
                    };
                    push_msg(&tab, "Claude", &o.result);
                    push_msg(
                        &tab,
                        "System",
                        &format!("cost: this turn ${:.4}, session ${:.4}", o.cost, total),
                    );
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
                    tab.entry.set_sensitive(true);
                    tab.send.set_sensitive(true);
                }
                Ev::Ended(why) => {
                    tab.stream.borrow_mut().clear();
                    push_msg(&tab, "System", &format!("(session process ended: {why})"));
                    tab.status.set_text("");
                    tab.entry.set_sensitive(false);
                    tab.send.set_sensitive(false);
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
        tab.entry.set_sensitive(false);
        tab.send.set_sensitive(false);
        tab.approve.set_sensitive(false);
    } else {
        push_msg(tab, "System", "Error: session process not running. Re-choose the folder.");
    }
}

fn build_session_tab(window: &ApplicationWindow, bin: Rc<String>) -> gtk::Widget {
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
    entry.set_placeholder_text(Some("Message Claude Code…  (Enter to send)"));
    entry.set_sensitive(false);
    let approve = gtk::Button::with_label("Approve");
    approve.set_sensitive(false);
    let send = gtk::Button::with_label("Send");
    send.set_sensitive(false);
    bottom.append(&entry);
    bottom.append(&approve);
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
        send: send.clone(),
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
                        tab_fp.entry.set_sensitive(true);
                        tab_fp.send.set_sensitive(true);
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

    {
        let tab_m = tab.clone();
        mode.connect_selected_notify(move |_| {
            if tab_m.sess.borrow().workdir.is_some() {
                push_msg(&tab_m, "System", "Mode changed — restarting session (context kept).");
                spawn_proc(&tab_m, false);
            }
        });
    }

    root.upcast()
}

fn add_tab(notebook: &gtk::Notebook, window: &ApplicationWindow, bin: Rc<String>) {
    let page = build_session_tab(window, bin);
    let n = notebook.n_pages() + 1;
    let label = gtk::Label::new(Some(&format!("Session {n}")));
    let idx = notebook.append_page(&page, Some(&label));
    notebook.set_current_page(Some(idx));
}

fn build_ui(app: &Application) {
    let bin = Rc::new(resolve_claude());

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toolbar.set_margin_top(6);
    toolbar.set_margin_start(6);
    let new_btn = gtk::Button::with_label("➕ New session");
    toolbar.append(&new_btn);

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
        new_btn.connect_clicked(move |_| add_tab(&notebook, &window, bin.clone()));
    }

    add_tab(&notebook, &window, bin.clone());
    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
