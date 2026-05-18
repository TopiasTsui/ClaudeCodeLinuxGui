// Claude Code — Linux GUI (v0.0.1) — native GTK4, no Electron.
//
// UNVERIFIED: this file was written WITHOUT a Rust toolchain or GTK4 dev
// libraries available to compile or type-check it. Treat it as a starting
// point to iterate against real `cargo build` output. The most likely places
// to need version-specific fixes are marked with `FRAGILE:`.
//
// CLI integration logic below is grounded in empirical probes of the installed
// `claude` CLI (per-turn `--session-id` / `--resume`, `--output-format json`).
// v0.0.1 is chat-only (`--tools ""`) — no streaming, no tool use (deliberate).
//
// Not affiliated with, endorsed by, or sponsored by Anthropic.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow};

const APP_ID: &str = "dev.local.claude_code_linux_gui";

#[derive(Default)]
struct State {
    workdir: Option<PathBuf>,
    session_id: Option<String>,
    total_cost: f64,
}

fn resolve_claude() -> String {
    // Deliberate: do NOT trust an inherited PATH (the #1 failure of existing
    // Claude Code GUIs). Check explicit candidates. Override via CLAUDE_BIN.
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
        if std::path::Path::new(&c).exists() {
            return c;
        }
    }
    "claude".to_string() // last resort: rely on PATH, let spawn error surface
}

// Runs one turn synchronously (called on a worker thread, never the UI thread).
fn run_claude_turn(
    bin: &str,
    workdir: &PathBuf,
    session_id: &Option<String>,
    message: &str,
) -> Result<(String, String, f64), String> {
    let new_sid = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(bin);
    cmd.arg("-p")
        .arg(message)
        .arg("--output-format")
        .arg("json")
        .arg("--tools")
        .arg("")
        .current_dir(workdir);
    match session_id {
        Some(sid) => {
            cmd.arg("--resume").arg(sid);
        }
        None => {
            cmd.arg("--session-id").arg(&new_sid);
        }
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
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("Could not parse claude JSON: {e}\n{}", &stdout.chars().take(500).collect::<String>()))?;
    let result = v.get("result").and_then(|x| x.as_str()).unwrap_or("(empty response)").to_string();
    let sid = v
        .get("session_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or(new_sid);
    let cost = v.get("total_cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0);
    Ok((result, sid, cost))
}

fn append(buffer: &gtk::TextBuffer, who: &str, text: &str) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("\n{who}:\n{text}\n"));
}

fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(State::default()));
    let claude_bin = Rc::new(resolve_claude());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pick = gtk::Button::with_label("Choose folder…");
    let dir_label = gtk::Label::new(Some("No folder selected"));
    top.append(&pick);
    top.append(&dir_label);

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
    let send = gtk::Button::with_label("Send");
    send.set_sensitive(false);
    bottom.append(&entry);
    bottom.append(&send);

    root.append(&top);
    root.append(&scroll);
    root.append(&bottom);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Claude Code — Linux GUI (v0.0.1)")
        .default_width(1000)
        .default_height(720)
        .child(&root)
        .build();

    // ---- Folder picker ----
    // Uses the modern gtk::FileDialog (GTK 4.10+). The older FileChooserNative
    // triggers `GTK_IS_FILE_SYSTEM_MODEL` criticals on current GTK4.
    {
        let state = state.clone();
        let entry = entry.clone();
        let send = send.clone();
        let dir_label = dir_label.clone();
        let buffer = buffer.clone();
        let window = window.clone();
        pick.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose folder").build();
            let state = state.clone();
            let entry = entry.clone();
            let send = send.clone();
            let dir_label = dir_label.clone();
            let buffer = buffer.clone();
            dialog.select_folder(
                Some(&window),
                gtk::gio::Cancellable::NONE,
                move |res| {
                    if let Ok(file) = res {
                        if let Some(path) = file.path() {
                            {
                                let mut s = state.borrow_mut();
                                s.workdir = Some(path.clone());
                                s.session_id = None; // new folder = new session
                                s.total_cost = 0.0;
                            }
                            dir_label.set_text(&path.to_string_lossy());
                            entry.set_sensitive(true);
                            send.set_sensitive(true);
                            append(&buffer, "System", "Folder set. New session. Chat-only in v0.0.1 (tools disabled).");
                        }
                    }
                },
            );
        });
    }

    // ---- Send a turn ----
    let do_send = {
        let state = state.clone();
        let claude_bin = claude_bin.clone();
        let buffer = buffer.clone();
        let entry = entry.clone();
        let send = send.clone();
        move || {
            let message = entry.text().to_string();
            if message.trim().is_empty() {
                return;
            }
            let (workdir, session_id) = {
                let s = state.borrow();
                match &s.workdir {
                    Some(w) => (w.clone(), s.session_id.clone()),
                    None => return,
                }
            };
            entry.set_text("");
            append(&buffer, "You", &message);
            entry.set_sensitive(false);
            send.set_sensitive(false);

            // FRAGILE: thread -> UI handoff. async-channel + spawn_future_local
            // is current; older stacks used glib::MainContext::channel. Adjust
            // to whatever the resolved crate versions provide.
            let (tx, rx) = async_channel::bounded(1);
            let bin = (*claude_bin).clone();
            std::thread::spawn(move || {
                let res = run_claude_turn(&bin, &workdir, &session_id, &message);
                let _ = tx.send_blocking(res);
            });

            let state = state.clone();
            let buffer = buffer.clone();
            let entry = entry.clone();
            let send = send.clone();
            glib::spawn_future_local(async move {
                if let Ok(res) = rx.recv().await {
                    match res {
                        Ok((result, sid, cost)) => {
                            let total = {
                                let mut s = state.borrow_mut();
                                s.session_id = Some(sid);
                                s.total_cost += cost;
                                s.total_cost
                            };
                            append(&buffer, "Claude", &result);
                            append(
                                &buffer,
                                "System",
                                &format!("cost: this turn ${cost:.4}, session ${total:.4}"),
                            );
                        }
                        Err(e) => append(&buffer, "System", &format!("Error: {e}")),
                    }
                }
                entry.set_sensitive(true);
                send.set_sensitive(true);
            });
        }
    };

    // `do_send` is a move-closure (NOT Clone). Move it once into the button
    // handler; route Enter-in-entry through the button instead of cloning.
    send.connect_clicked(move |_| do_send());
    {
        let send = send.clone(); // gtk widgets are ref-counted; this Clone is fine
        entry.connect_activate(move |_| {
            send.emit_clicked();
        });
    }

    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
