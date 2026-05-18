'use strict';

const pickBtn = document.getElementById('pick-dir');
const workdirEl = document.getElementById('workdir');
const costEl = document.getElementById('cost');
const transcript = document.getElementById('transcript');
const input = document.getElementById('input');
const sendBtn = document.getElementById('send');

let workdir = null;
let sessionId = null;
let totalCost = 0;

function addMessage(role, text) {
  const el = document.createElement('div');
  el.className = 'msg ' + role;
  const who = document.createElement('div');
  who.className = 'who';
  who.textContent = role === 'user' ? 'You' : role === 'claude' ? 'Claude' : 'System';
  const body = document.createElement('pre');
  body.className = 'body';
  body.textContent = text;
  el.appendChild(who);
  el.appendChild(body);
  transcript.appendChild(el);
  transcript.scrollTop = transcript.scrollHeight;
  return body;
}

function setBusy(busy) {
  input.disabled = busy || !workdir;
  sendBtn.disabled = busy || !workdir;
  sendBtn.textContent = busy ? 'Working…' : 'Send';
}

pickBtn.addEventListener('click', async () => {
  const dir = await window.api.pickDir();
  if (!dir) return;
  workdir = dir;
  sessionId = null; // new folder = new session
  totalCost = 0;
  costEl.textContent = '';
  workdirEl.textContent = dir;
  workdirEl.classList.remove('muted');
  transcript.innerHTML = '';
  addMessage('system', 'Folder set. New session. Chat-only in v0.0.1 (tools disabled).');
  setBusy(false);
  input.focus();
});

async function send() {
  const message = input.value.trim();
  if (!message || !workdir) return;
  input.value = '';
  addMessage('user', message);
  setBusy(true);
  const res = await window.api.sendMessage({ workdir, sessionId, message });
  if (res.ok) {
    sessionId = res.sessionId || sessionId;
    if (typeof res.cost === 'number') {
      totalCost += res.cost;
      costEl.textContent = 'session cost ≈ $' + totalCost.toFixed(4);
    }
    addMessage('claude', res.result != null ? String(res.result) : '(empty response)');
  } else {
    addMessage('system', 'Error: ' + res.error);
  }
  setBusy(false);
  input.focus();
}

sendBtn.addEventListener('click', send);
input.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    send();
  }
});
