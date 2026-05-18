// Claude Code — Linux GUI (v0.2.0) — native GTK4, no Electron.
//
// UNVERIFIED BUILD: written without a Rust toolchain / GTK4 dev libs to compile
// or type-check. Large rewrite (multi-session + per-session mode + full tools).
// Expect several `cargo build` iterations. Risk spots marked `FRAGILE:`.
//
// Grounded in empirical probes of the installed `claude`:
//   * per-turn `--session-id` / `--resume`, `--output-format json`        (verified)
//   * default perm -> structured `permission_denials`                     (verified)
//     - file ops carry tool_input.file_path; Bash carries tool_input.command
//   * Read inside workdir auto-allowed; outside denied, fixed `--add-dir`  (verified)
//   * Write denied by default; resume + acceptEdits runs it               (verified)
//   * Bash denied by default; resume + acceptEdits RUNS it                (verified)
//   * approved dirs must be re-passed every turn (fresh process each turn) (verified)
// UNVERIFIED: exact behaviour of `--permission-mode plan` and `auto` under
// `-p`; the multi-tool default toolset is left unrestricted (no `--tools`).
//
// Tools: unrestricted (Bash/Write/Edit/Read/...) — governed by the per-session
// permission mode. This is a real agent harness on your own machine; the mode
// dropdown is the safety control.
//
// Not affiliated with, endorsed by, or sponsored by Anthropic.

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow};
use wait_timeout::ChildExt;

const APP_ID: &str = "dev.local.claude_code_linux_gui";

// Dropdown index -> permission-mode flag value (None = CLI default / "ask").
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

#[derive(Default)]
struct Session {
    workdir: Option<PathBuf>,
    session_id: Option<String>,
    total_cost: f64,
    pending_approval: bool,
    pending_dirs: Vec<String>,
    allowed_dirs: Vec<String>,
}

struct TurnOutcome {
    result: String,
    session_id: String,
    cost: f64,
    denials: Vec<String>,
    denied_dirs: Vec<String>,
}

fn run_claude_turn(
    bin: &str,
    workdir: &PathBuf,
    session_id: &Option<String>,
    message: &str,
    permission_mode: Option<&str>,
    allowed_dirs: &[String],
) -> Result<TurnOutcome, String> {
    let new_sid = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(bin);
    cmd.arg("-p")
        .arg(message)
        .arg("--output-format")
        .arg("json")
        .current_dir(workdir);
    match session_id {
        Some(sid) => {
            cmd.arg("--resume").arg(sid);
        }
        None => {
            cmd.arg("--session-id").arg(&new_sid);
        }
    }
    if let Some(m) = permission_mode {
        cmd.arg("--permission-mode").arg(m);
    }
    for d in allowed_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    // FRAGILE: spawn + drain pipes on threads + wait_timeout. Highest-risk
    // block to iterate against the compiler.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch claude ({bin}): {e}"))?;
    let mut co = child.stdout.take().expect("piped stdout");
    let mut ce = child.stderr.take().expect("piped stderr");
    let h_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = co.read_to_end(&mut b);
        b
    });
    let h_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = ce.read_to_end(&mut b);
        b
    });
    // Generous: only a safety net for a TRUE hang, not a cap on legit long
    // turns (real-repo turns can take minutes).
    let status = match child
        .wait_timeout(Duration::from_secs(600))
        .map_err(|e| format!("wait error: {e}"))?
    {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "Timed out after 600s (process killed). The turn hung or ran too long.".to_string(),
            );
        }
    };
    let out_bytes = h_out.join().unwrap_or_default();
    let err_bytes = h_err.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err_bytes);
        return Err(if err.trim().is_empty() {
            format!("claude exited with status {status}")
        } else {
            err.trim().to_string()
        });
    }
    let stdout = String::from_utf8_lossy(&out_bytes);
    let v: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
        format!(
            "Could not parse claude JSON: {e}\n{}",
            stdout.chars().take(500).collect::<String>()
        )
    })?;
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .unwrap_or("(empty response)")
        .to_string();
    let sid = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .unwrap_or(new_sid);
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
    Ok(TurnOutcome { result, session_id: sid, cost, denials, denied_dirs })
}

fn append(buffer: &gtk::TextBuffer, who: &str, text: &str) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("\n{who}:\n{text}\n"));
}

#[derive(Clone)]
struct Tab {
    sess: Rc<RefCell<Session>>,
    bin: Rc<String>,
    buffer: gtk::TextBuffer,
    entry: gtk::Entry,
    send: gtk::Button,
    approve: gtk::Button,
    mode: gtk::DropDown,
    status: gtk::Label,
}

fn start_turn(tab: &Tab, message: String, force_accept_edits: bool) {
    let (workdir, session_id, allowed_dirs) = {
        let s = tab.sess.borrow();
        match &s.workdir {
            Some(w) => (w.clone(), s.session_id.clone(), s.allowed_dirs.clone()),
            None => return,
        }
    };
    let permission_mode = if force_accept_edits {
        Some("acceptEdits")
    } else {
        mode_flag(tab.mode.selected())
    };

    tab.entry.set_sensitive(false);
    tab.send.set_sensitive(false);
    tab.approve.set_sensitive(false);
    tab.status.set_text("⏳ working…");

    // FRAGILE: thread -> UI handoff (async-channel + spawn_future_local).
    let (tx, rx) = async_channel::bounded(1);
    let bin = (*tab.bin).clone();
    let pm = permission_mode.map(str::to_string);
    std::thread::spawn(move || {
        let res = run_claude_turn(
            &bin,
            &workdir,
            &session_id,
            &message,
            pm.as_deref(),
            &allowed_dirs,
        );
        let _ = tx.send_blocking(res);
    });

    let tab = tab.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(o)) => {
                let total = {
                    let mut s = tab.sess.borrow_mut();
                    s.session_id = Some(o.session_id);
                    s.total_cost += o.cost;
                    s.total_cost
                };
                append(&tab.buffer, "Claude", &o.result);
                append(
                    &tab.buffer,
                    "System",
                    &format!("cost: this turn ${:.4}, session ${:.4}", o.cost, total),
                );
                if !o.denials.is_empty() {
                    {
                        let mut s = tab.sess.borrow_mut();
                        s.pending_approval = true;
                        s.pending_dirs = o.denied_dirs.clone();
                    }
                    append(
                        &tab.buffer,
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
            }
            Ok(Err(e)) => append(&tab.buffer, "System", &format!("Error: {e}")),
            Err(_) => append(&tab.buffer, "System", "Error: worker channel closed"),
        }
        tab.status.set_text("");
        tab.entry.set_sensitive(true);
        tab.send.set_sensitive(true);
    });
}

fn build_session_tab(window: &ApplicationWindow, bin: Rc<String>) -> (gtk::Widget, String) {
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

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    scroll.set_child(Some(&view));
    let buffer = view.buffer();

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
    root.append(&scroll);
    root.append(&bottom);

    let tab = Tab {
        sess: sess.clone(),
        bin,
        buffer: buffer.clone(),
        entry: entry.clone(),
        send: send.clone(),
        approve: approve.clone(),
        mode: mode.clone(),
        status: status.clone(),
    };

    // Folder picker
    {
        let sess = sess.clone();
        let tab_fp = tab.clone();
        let dir_label = dir_label.clone();
        let buffer = buffer.clone();
        let window = window.clone();
        pick.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose folder").build();
            let sess = sess.clone();
            let tab_fp = tab_fp.clone();
            let dir_label = dir_label.clone();
            let buffer = buffer.clone();
            dialog.select_folder(Some(&window), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        {
                            let mut s = sess.borrow_mut();
                            *s = Session {
                                workdir: Some(path.clone()),
                                ..Session::default()
                            };
                        }
                        dir_label.set_text(&path.to_string_lossy());
                        tab_fp.entry.set_sensitive(true);
                        tab_fp.send.set_sensitive(true);
                        tab_fp.approve.set_sensitive(false);
                        append(
                            &buffer,
                            "System",
                            "Folder set. New session. All tools enabled; permission governed by the mode dropdown.",
                        );
                    }
                }
            });
        });
    }

    // Send
    {
        let tab_s = tab.clone();
        send.connect_clicked(move |_| {
            let msg = tab_s.entry.text().to_string();
            if msg.trim().is_empty() {
                return;
            }
            tab_s.entry.set_text("");
            append(&tab_s.buffer, "You", &msg);
            start_turn(&tab_s, msg, false);
        });
    }
    {
        let send = send.clone();
        entry.connect_activate(move |_| send.emit_clicked());
    }

    // Approve
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
            append(&tab_a.buffer, "You", "[Approved — granting access and continuing]");
            start_turn(
                &tab_a,
                "Approved. Proceed with the action you described.".to_string(),
                true,
            );
        });
    }

    (root.upcast(), "session".to_string())
}

fn add_tab(notebook: &gtk::Notebook, window: &ApplicationWindow, bin: Rc<String>) {
    let (page, _) = build_session_tab(window, bin);
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
    toolbar.set_margin_bottom(0);
    toolbar.set_margin_start(6);
    let new_btn = gtk::Button::with_label("➕ New session");
    toolbar.append(&new_btn);

    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    notebook.set_scrollable(true);

    vbox.append(&toolbar);
    vbox.append(&notebook);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Claude Code — Linux GUI (v0.2.0)")
        .default_width(1100)
        .default_height(780)
        .child(&vbox)
        .build();

    {
        let notebook = notebook.clone();
        let window = window.clone();
        let bin = bin.clone();
        new_btn.connect_clicked(move |_| {
            add_tab(&notebook, &window, bin.clone());
        });
    }

    add_tab(&notebook, &window, bin.clone()); // first session
    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
