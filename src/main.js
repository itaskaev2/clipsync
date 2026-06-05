const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// --- State ---
let peers = [];

// --- DOM refs ---
const statusBadge = document.getElementById('status-badge');
const yourCode = document.getElementById('your-code');
const pairingCode = document.getElementById('pairing-code');
const pairingMsg = document.getElementById('pairing-msg');
const peerList = document.getElementById('peer-list');
const payloadLimit = document.getElementById('payload-limit');
const debounceMs = document.getElementById('debounce-ms');
const clipboardPriority = document.getElementById('clipboard-priority');
const syncPaused = document.getElementById('sync-paused');
const settingsMsg = document.getElementById('settings-msg');
const appVersion = document.getElementById('app-version');
const footerVersion = document.getElementById('footer-version');
const instanceId = document.getElementById('instance-id');

// --- Generate a code on this machine ---
document.getElementById('btn-generate').addEventListener('click', async () => {
  try {
    const code = await invoke('generate_code');
    yourCode.textContent = code;
    pairingMsg.textContent = 'Code generated — enter it on the other machine.';
    pairingMsg.className = 'msg success';
  } catch (err) {
    pairingMsg.textContent = `Generate failed: ${err}`;
    pairingMsg.className = 'msg error';
  }
});

// --- Pair with the code from the other machine ---
document.getElementById('btn-pair').addEventListener('click', async () => {
  const code = pairingCode.value.trim();
  if (!/^\d{6}$/.test(code)) {
    pairingMsg.textContent = 'Enter a 6-digit code';
    pairingMsg.className = 'msg error';
    return;
  }
  try {
    await invoke('pair_with_code', { code });
    yourCode.textContent = code;
    pairingMsg.textContent = 'Paired. Looking for the peer on this code…';
    pairingMsg.className = 'msg success';
    pairingCode.value = '';
    await loadPeers();
  } catch (err) {
    pairingMsg.textContent = `Pairing failed: ${err}`;
    pairingMsg.className = 'msg error';
  }
});

// --- Save settings ---
document.getElementById('btn-save-settings').addEventListener('click', async () => {
  try {
    // NB: the Rust command parameter is named `update`.
    await invoke('update_config', {
      update: {
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
    const name = document.createElement('span');
    name.textContent = p.name || p.id;
    const status = document.createElement('span');
    status.className = `peer-status ${p.connected ? 'online' : 'offline'}`;
    status.textContent = p.connected ? 'online' : 'offline';
    li.append(name, status);
    peerList.appendChild(li);
  }
}

function setBadge(paused, peerCount) {
  if (paused) {
    statusBadge.textContent = 'Paused';
    statusBadge.className = 'badge disconnected';
  } else if (peerCount > 0) {
    statusBadge.textContent = `Connected (${peerCount})`;
    statusBadge.className = 'badge connected';
  } else {
    statusBadge.textContent = 'Waiting for peer';
    statusBadge.className = 'badge disconnected';
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
    if (config.pairing_code) yourCode.textContent = config.pairing_code;
    const connectedCount = peers.filter((p) => p.connected).length;
    setBadge(config.sync_paused, connectedCount);
  } catch (err) {
    console.error('Failed to load config:', err);
  }
}

// --- Build version + device identity (to verify both machines match) ---
async function loadVersion() {
  try {
    const v = await invoke('get_version');
    appVersion.textContent = `v${v}`;
    footerVersion.textContent = `v${v}`;
  } catch (err) {
    console.error('Failed to load version:', err);
  }
}

async function loadIdentity() {
  try {
    const status = await invoke('get_status');
    if (status && status.instance_id) {
      instanceId.textContent = String(status.instance_id).slice(0, 8);
    }
  } catch (err) {
    console.error('Failed to load status:', err);
  }
}

// --- Backend events ---
listen('clipsync:peer-joined', () => loadPeers());
listen('clipsync:peer-left', () => loadPeers());

listen('clipsync:status-changed', (event) => {
  const { peers: peerCount = 0, paused = false } = event.payload || {};
  setBadge(paused, peerCount);
});

listen('clipsync:notification', (event) => {
  const { title, body } = event.payload || {};
  pairingMsg.textContent = `${title}: ${body}`;
  pairingMsg.className = 'msg';
});

// --- Init ---
loadVersion();
loadIdentity();
loadConfig();
loadPeers();
setInterval(loadPeers, 5000);
