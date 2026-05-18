'use strict';

const { app, BrowserWindow, ipcMain, dialog } = require('electron');
const { spawn } = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');

// Resolve the `claude` binary.
// NOTE: this directly addresses the single most common failure of existing
// Claude Code GUIs (PATH not inherited by a desktop-launched process, e.g.
// "env: node: No such file or directory"). We check explicit candidates
// instead of trusting the inherited PATH.
function resolveClaude() {
  const home = os.homedir();
  const candidates = [
    process.env.CLAUDE_BIN,
    path.join(home, '.local', 'bin', 'claude'),
    '/usr/local/bin/claude',
    '/usr/bin/claude',
    path.join(home, '.npm-global', 'bin', 'claude'),
  ].filter(Boolean);
  for (const c of candidates) {
    try {
      if (fs.existsSync(c)) return c;
    } catch (_) { /* ignore */ }
  }
  // Last resort: rely on PATH and let spawn surface a clear error.
  return 'claude';
}

const CLAUDE_BIN = resolveClaude();

function createWindow() {
  const win = new BrowserWindow({
    width: 1000,
    height: 720,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  win.loadFile('index.html');
}

app.whenReady().then(() => {
  createWindow();
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

ipcMain.handle('pick-dir', async () => {
  const res = await dialog.showOpenDialog({ properties: ['openDirectory'] });
  if (res.canceled || res.filePaths.length === 0) return null;
  return res.filePaths[0];
});

// One turn = one `claude -p` invocation. Context persists across turns via the
// session id (verified empirically: --session-id to start, --resume to continue).
// v0.0.1 intentionally disables tools (--tools "") so it is chat-only and never
// blocks on a permission prompt the GUI can't answer yet. This is a documented
// v0.0.1 boundary, not the end state.
ipcMain.handle('send-message', async (_evt, payload) => {
  const { workdir, sessionId, message } = payload || {};
  return new Promise((resolve) => {
    if (!workdir) {
      resolve({ ok: false, error: 'No working directory selected.' });
      return;
    }
    if (!message || !message.trim()) {
      resolve({ ok: false, error: 'Empty message.' });
      return;
    }

    const sid = sessionId || crypto.randomUUID();
    const args = ['-p', message, '--output-format', 'json', '--tools', ''];
    if (sessionId) {
      args.push('--resume', sessionId);
    } else {
      args.push('--session-id', sid);
    }

    let child;
    try {
      child = spawn(CLAUDE_BIN, args, { cwd: workdir });
    } catch (e) {
      resolve({ ok: false, error: 'Failed to launch claude (' + CLAUDE_BIN + '): ' + e.message });
      return;
    }

    let out = '';
    let err = '';
    child.stdout.on('data', (d) => { out += d.toString(); });
    child.stderr.on('data', (d) => { err += d.toString(); });
    child.on('error', (e) => {
      resolve({ ok: false, error: 'Failed to launch claude (' + CLAUDE_BIN + '): ' + e.message });
    });
    child.on('close', (code) => {
      if (code !== 0) {
        resolve({ ok: false, error: (err || ('claude exited with code ' + code)).trim() });
        return;
      }
      try {
        const parsed = JSON.parse(out);
        resolve({
          ok: true,
          result: parsed.result,
          sessionId: parsed.session_id || sid,
          cost: parsed.total_cost_usd,
        });
      } catch (e) {
        resolve({
          ok: false,
          error: 'Could not parse claude JSON output: ' + e.message + '\n--- raw (first 500 chars) ---\n' + out.slice(0, 500),
        });
      }
    });
  });
});
