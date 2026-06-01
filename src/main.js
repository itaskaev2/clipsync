const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// --- State ---
let peers = [];

// --- DOM refs ---
const statusBadge = document.getElementById('status-badge');
const pairingCode = document.getElementById('pairing-code');
const pairingMsg = document.getElementById('pairing-msg');
const peerList = document.getElementById('peer-list');
const payloadLimit = document.getElementById('payload-limit');
const debounceMs = document.getElementById('debounce-ms');
const clipboardPriority = document.getElementById('clipboard-priority');
const syncPaused = document.getElementById('sync-paused');
const settingsMsg = document.getElementById('settings-msg');

// --- Pairing ---
document.getElementById('btn-pair').addEventListener('click', async () => {
  const code = pairingCode.value.trim();
  if (code.length !== 6 || !/^\d{6}$/.test(code)) {
    pairingMsg.textContent = 'Enter a 6-digit code';
    pairingMsg.className = 'msg error';
    return;
  }

  try {
    await invoke('pair_with_code', { code });
    pairingMsg.textContent = 'Pairing request sent.';
    pairingMsg.className = 'msg success';
    pairingCode.value = '';
    await loadPeers();
  } catch (err) {
    pairingMsg.textContent = `Pairing failed: ${err}`;
    pairingMsg.className = 'msg error';
  }
});

// --- Settings ---
document.getElementById('btn-save-settings').addEventListener('click', async () => {
  try {
    await invoke('update_config', {
      config: {
        payload_limit_mb: parseInt(payloadLimit.value) || 25,
        debounce_ms: parseInt(debounceMs.value) || 200,
        clipboard_priority: clipboardPriority.value,
        sync_paused: syncPaused.checked,
      },
    });
    settingsMsg.textContent = 'Settings saved.';
    settingsMsg.className = 'msg success';
  } catch (err) {
    settingsMsg.textContent = `Save failed: ${err}`;
    settingsMsg.className = 'msg error';
  }
});

// --- Peer list ---
async function loadPeers() {
  try {
    peers = await invoke('get_peers');
    renderPeers();
  } catch (err) {
    console.error('Failed to load peers:', err);
  }
}

function renderPeers() {
  peerList.innerHTML = '';
  if (peers.length === 0) {
    peerList.innerHTML = '<li class="empty">No paired peers</li>';
    return;
  }
  for (const p of peers) {
    const li = document.createElement('li');
    li.innerHTML = `
      <span>${escapeHtml(p.name || p.id)}</span>
      <span class="peer-status ${p.connected ? 'online' : 'offline'}">
        ${p.connected ? 'online' : 'offline'}
      </span>
    `;
    peerList.appendChild(li);
  }
}

// --- Load initial config ---
async function loadConfig() {
  try {
    const config = await invoke('get_config');
    payloadLimit.value = config.payload_limit_mb;
    debounceMs.value = config.debounce_ms;
    clipboardPriority.value = config.clipboard_priority;
    syncPaused.checked = config.sync_paused;
    statusBadge.textContent = config.sync_paused ? 'Paused' : 'Connected';
    statusBadge.className = config.sync_paused
      ? 'badge disconnected'
      : 'badge connected';
  } catch (err) {
    console.error('Failed to load config:', err);
  }
}

// --- Event listeners from backend ---
listen('clipsync:peer-joined', (event) => {
  console.log('Peer joined:', event.payload);
  loadPeers();
});

listen('clipsync:peer-left', (event) => {
  console.log('Peer left:', event.payload);
  loadPeers();
});

listen('clipsync:status-changed', (event) => {
  const { connected, paused } = event.payload;
  if (paused) {
    statusBadge.textContent = 'Paused';
    statusBadge.className = 'badge disconnected';
  } else if (connected) {
    statusBadge.textContent = 'Connected';
    statusBadge.className = 'badge connected';
  } else {
    statusBadge.textContent = 'Disconnected';
    statusBadge.className = 'badge disconnected';
  }
});

listen('clipsync:notification', (event) => {
  const { title, body } = event.payload;
  console.log(`[${title}] ${body}`);
});

// --- Init ---
loadConfig();
loadPeers();
setInterval(loadPeers, 5000);

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
