// Claude Code — Linux GUI (v0.1.0) — native GTK4, no Electron.
//
// UNVERIFIED BUILD: written without a Rust toolchain / GTK4 dev libs to compile
// or type-check. Iterate against real `cargo build` output. Risk spots marked
// `FRAGILE:`.
//
// Permission model — grounded in empirical probes of the installed `claude`:
//   * per-turn `--session-id` / `--resume`, `--output-format json`        (verified)
//   * default perm -> structured `permission_denials` (tool + file_path)  (verified)
//   * Read INSIDE workdir: allowed by default                             (verified)
//   * Read OUTSIDE workdir: denied; fixed by `--add-dir <dir>`            (verified)
//   * Write: denied by default; fixed by `--permission-mode acceptEdits`  (verified)
//   * resume + `--add-dir` reads previously-denied outside file           (verified)
//   * combining `acceptEdits` + multiple `--add-dir` in one resumed call: NOT
//     jointly tested (independent flags; low risk) — FRAGILE.
//
// On approve: re-run resumed with `--permission-mode acceptEdits` AND
// `--add-dir` for every approved directory. Approved dirs accumulate and are
// passed on EVERY subsequent turn (each turn is a fresh process; access does
// not persist across invocations otherwise).
//
// v0.1 scope: Write,Edit,Read only. No Bash / command execution (unverified,
// riskier).
//
// Not affiliated with, endorsed by, or sponsored by Anthropic.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow};

const APP_ID: &str = "dev.local.claude_code_linux_gui";
const TOOLS: &str = "Write,Edit,Read"; // FRAGILE: multi-tool arg form unverified

#[derive(Default)]
struct State {
    workdir: Option<PathBuf>,
    session_id: Option<String>,
    total_cost: f64,
    pending_approval: bool,
    pending_dirs: Vec<String>, // dirs from the last denial, awaiting approve
    allowed_dirs: Vec<String>, // approved dirs, re-sent every turn
}

fn resolve_claude() -> String {
    if let Ok(v) = std::env::var("CLAUDE_BIN") {
        if !v.is_empty() {
            return v;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/claude"),
        "/usr/local/bin/claude".to_string(),
        "/usr/bin/claude".to_string(),
        format!("{home}/.npm-global/bin/claude"),
    ];
    for c in candidates {
        if Path::new(&c).exists() {
            return c;
        }
    }
    "claude".to_string()
}

struct TurnOutcome {
    result: String,
    session_id: String,
    cost: f64,
    denials: Vec<String>,     // "Tool -> /path" for display
    denied_dirs: Vec<String>, // parent dirs of denied file paths
}

fn run_claude_turn(
    bin: &str,
    workdir: &PathBuf,
    session_id: &Option<String>,
    message: &str,
    accept_edits: bool,
    allowed_dirs: &[String],
) -> Result<TurnOutcome, String> {
    let new_sid = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(bin);
    cmd.arg("-p")
        .arg(message)
        .arg("--output-format")
        .arg("json")
        .arg("--tools")
        .arg(TOOLS)
        .current_dir(workdir);
    match session_id {
        Some(sid) => {
            cmd.arg("--resume").arg(sid);
        }
        None => {
            cmd.arg("--session-id").arg(&new_sid);
        }
    }
    if accept_edits {
        cmd.arg("--permission-mode").arg("acceptEdits");
    }
    for d in allowed_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Failed to launch claude ({bin}): {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(if err.trim().is_empty() {
            format!("claude exited with status {}", out.status)
        } else {
            err.trim().to_string()
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
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
        .map(|s| s.to_string())
        .unwrap_or(new_sid);
    let cost = v.get("total_cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut denials = Vec::new();
    let mut denied_dirs = Vec::new();
    if let Some(arr) = v.get("permission_denials").and_then(|x| x.as_array()) {
        for d in arr {
            let tool = d.get("tool_name").and_then(|x| x.as_str()).unwrap_or("?");
            let fp = d
                .get("tool_input")
                .and_then(|i| i.get("file_path"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if fp.is_empty() {
                denials.push(tool.to_string());
            } else {
                denials.push(format!("{tool} -> {fp}"));
                if let Some(parent) = Path::new(fp).parent() {
                    let p = parent.to_string_lossy().to_string();
                    if !p.is_empty() && !denied_dirs.contains(&p) {
                        denied_dirs.push(p);
                    }
                }
            }
        }
    }
    Ok(TurnOutcome { result, session_id: sid, cost, denials, denied_dirs })
}

fn append(buffer: &gtk::TextBuffer, who: &str, text: &str) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("\n{who}:\n{text}\n"));
}

struct Ui {
    state: Rc<RefCell<State>>,
    bin: Rc<String>,
    buffer: gtk::TextBuffer,
    entry: gtk::Entry,
    send: gtk::Button,
    approve: gtk::Button,
    auto_edit: gtk::CheckButton,
}

impl Clone for Ui {
    fn clone(&self) -> Self {
        Ui {
            state: self.state.clone(),
            bin: self.bin.clone(),
            buffer: self.buffer.clone(),
            entry: self.entry.clone(),
            send: self.send.clone(),
            approve: self.approve.clone(),
            auto_edit: self.auto_edit.clone(),
        }
    }
}

fn start_turn(ui: &Ui, message: String, force_accept_edits: bool) {
    let (workdir, session_id, allowed_dirs) = {
        let s = ui.state.borrow();
        match &s.workdir {
            Some(w) => (w.clone(), s.session_id.clone(), s.allowed_dirs.clone()),
            None => return,
        }
    };
    let accept_edits = force_accept_edits || ui.auto_edit.is_active();

    ui.entry.set_sensitive(false);
    ui.send.set_sensitive(false);
    ui.approve.set_sensitive(false);

    // FRAGILE: thread -> UI handoff (async-channel + spawn_future_local).
    let (tx, rx) = async_channel::bounded(1);
    let bin = (*ui.bin).clone();
    std::thread::spawn(move || {
        let res = run_claude_turn(
            &bin,
            &workdir,
            &session_id,
            &message,
            accept_edits,
            &allowed_dirs,
        );
        let _ = tx.send_blocking(res);
    });

    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(o)) => {
                let total = {
                    let mut s = ui.state.borrow_mut();
                    s.session_id = Some(o.session_id);
                    s.total_cost += o.cost;
                    s.total_cost
                };
                append(&ui.buffer, "Claude", &o.result);
                append(
                    &ui.buffer,
                    "System",
                    &format!("cost: this turn ${:.4}, session ${:.4}", o.cost, total),
                );
                if !o.denials.is_empty() {
                    {
                        let mut s = ui.state.borrow_mut();
                        s.pending_approval = true;
                        s.pending_dirs = o.denied_dirs.clone();
                    }
                    append(
                        &ui.buffer,
                        "System",
                        &format!(
                            "Claude needs permission for: {}\n\
                             >>> Click the [Approve] button below. Typing does NOT grant permission. <<<",
                            o.denials.join(", ")
                        ),
                    );
                    ui.approve.set_sensitive(true);
                } else {
                    ui.state.borrow_mut().pending_approval = false;
                }
            }
            Ok(Err(e)) => append(&ui.buffer, "System", &format!("Error: {e}")),
            Err(_) => append(&ui.buffer, "System", "Error: worker channel closed"),
        }
        ui.entry.set_sensitive(true);
        ui.send.set_sensitive(true);
    });
}

fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(State::default()));
    let bin = Rc::new(resolve_claude());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pick = gtk::Button::with_label("Choose folder…");
    let dir_label = gtk::Label::new(Some("No folder selected"));
    let auto_edit = gtk::CheckButton::with_label("Auto-approve edits");
    top.append(&pick);
    top.append(&dir_label);
    top.append(&auto_edit);

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

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Claude Code — Linux GUI (v0.1.0)")
        .default_width(1000)
        .default_height(720)
        .child(&root)
        .build();

    let ui = Ui {
        state: state.clone(),
        bin: bin.clone(),
        buffer: buffer.clone(),
        entry: entry.clone(),
        send: send.clone(),
        approve: approve.clone(),
        auto_edit: auto_edit.clone(),
    };

    // ---- Folder picker (modern gtk::FileDialog, GTK 4.10+) ----
    {
        let state = state.clone();
        let ui_fp = ui.clone();
        let dir_label = dir_label.clone();
        let buffer = buffer.clone();
        let window = window.clone();
        pick.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose folder").build();
            let state = state.clone();
            let ui_fp = ui_fp.clone();
            let dir_label = dir_label.clone();
            let buffer = buffer.clone();
            dialog.select_folder(Some(&window), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        {
                            let mut s = state.borrow_mut();
                            s.workdir = Some(path.clone());
                            s.session_id = None;
                            s.total_cost = 0.0;
                            s.pending_approval = false;
                            s.pending_dirs.clear();
                            s.allowed_dirs.clear();
                        }
                        dir_label.set_text(&path.to_string_lossy());
                        ui_fp.entry.set_sensitive(true);
                        ui_fp.send.set_sensitive(true);
                        ui_fp.approve.set_sensitive(false);
                        append(
                            &buffer,
                            "System",
                            "Folder set. New session. File-editing tools enabled (no Bash in v0.1).",
                        );
                    }
                }
            });
        });
    }

    // ---- Send ----
    {
        let ui_s = ui.clone();
        send.connect_clicked(move |_| {
            let msg = ui_s.entry.text().to_string();
            if msg.trim().is_empty() {
                return;
            }
            ui_s.entry.set_text("");
            append(&ui_s.buffer, "You", &msg);
            start_turn(&ui_s, msg, false);
        });
    }
    {
        let send = send.clone();
        entry.connect_activate(move |_| {
            send.emit_clicked();
        });
    }

    // ---- Approve: promote pending dirs to allowed, resume with acceptEdits ----
    {
        let ui_a = ui.clone();
        approve.connect_clicked(move |_| {
            let had_pending = {
                let mut s = ui_a.state.borrow_mut();
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
            if !had_pending {
                return;
            }
            append(&ui_a.buffer, "You", "[Approved — granting access and continuing]");
            start_turn(
                &ui_a,
                "Approved. Proceed with the action you described.".to_string(),
                true,
            );
        });
    }

    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
