// ============================================================================
// Apex Supervision — Moteur Applicatif Frontend (RBAC, Sécurité & Performance)
// ============================================================================
//
// Règles de sécurité de ce fichier :
// - Toute donnée dynamique insérée dans du HTML passe par esc().
// - Toute donnée passée en argument d'un gestionnaire onclick/onchange passe par jsArg().
// - Tout SVG importé passe par sanitizeSvg() avant rendu.

const WIDGET_TYPES = [
  'Gauge', 'Pump', 'Valve', 'MotorValve2Way', 'Fan', 'Damper', 'Boiler', 'Chiller',
  'Compressor', 'HeatExchanger', 'CoolingTower', 'HeaterBattery', 'FilterUnit',
  'CircuitBreaker', 'Transformer', 'ElectricMotor', 'FlowMeter', 'PressureGauge',
  'Thermometer', 'TrendChart', 'SetpointButton', 'SwitchButton', 'PushButton',
  'TankLevel', 'IndicatorLed', 'NumericDisplay', 'PipeSegment', 'CustomSvgSymbol'
];

// Protocoles sans implémentation réelle (valeurs fictives) : signalés comme tels dans l'IHM
const SIMULATED_PROTOCOLS = ['Simulator', 'OpcUa', 'BacnetIp', 'SiemensS7'];
const WEEK_DAYS = [[1, 'Lun'], [2, 'Mar'], [3, 'Mer'], [4, 'Jeu'], [5, 'Ven'], [6, 'Sam'], [7, 'Dim']];

const DEFAULT_SVG = '<svg viewBox="0 0 100 100"><circle cx="50" cy="50" r="35" stroke="currentColor" stroke-width="3" fill="none"/></svg>';

const state = {
  mode: 'studio',
  activeTab: 'synoptic',
  authToken: readStoredToken(),
  currentUser: null,
  usersList: [],
  symbols: [],
  tags: {},
  history: {},
  dashboards: [],
  currentDashboardId: null,
  flows: [],
  currentFlowId: null,
  alarms: [],
  drivers: [],
  schedules: [],
  systemStatus: null,
  selectedWidgetId: null,
  selectedFlowNodeId: null,
  wiringSource: null,
  nodeLiveValues: {},
  covCount: 0,
  shuttingDown: false,
  ws: null,
  wsRetryTimer: null,
  pendingTagIds: new Set(),
  renderScheduled: false
};

// ============================================================================
// SÉCURITÉ : ÉCHAPPEMENT HTML, ARGUMENTS JS & ASSAINISSEMENT SVG
// ============================================================================

const ESC_MAP = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;', '`': '&#96;' };

/** Échappe une valeur pour une insertion sûre dans du HTML (texte ou attribut). */
function esc(v) {
  if (v === null || v === undefined) return '';
  return String(v).replace(/[&<>"'`]/g, c => ESC_MAP[c]);
}

/** Encode une valeur comme argument JavaScript dans un attribut HTML (onclick="f(${jsArg(x)})"). */
function jsArg(v) {
  return esc(JSON.stringify(v === undefined ? null : v));
}

const SVG_FORBIDDEN = new Set(['script', 'style', 'foreignobject', 'iframe', 'object', 'embed', 'handler', 'listener', 'link', 'meta', 'base']);
const svgCache = new Map();

/** Filtre côté client des SVG (défense en profondeur, en plus du filtrage serveur). */
function sanitizeSvg(svgText) {
  const raw = String(svgText || '');
  if (svgCache.has(raw)) return svgCache.get(raw);
  let clean = DEFAULT_SVG;
  try {
    const doc = new DOMParser().parseFromString(raw, 'image/svg+xml');
    const root = doc.documentElement;
    if (root && root.localName.toLowerCase() === 'svg' && !doc.getElementsByTagName('parsererror').length) {
      const cleanAttrs = (el) => {
        [...el.attributes].forEach(a => {
          const name = a.name.toLowerCase();
          const val = a.value.replace(/\s+/g, '').toLowerCase();
          const isUrl = name === 'href' || name.endsWith(':href') || name === 'src';
          if (name.startsWith('on') || /(java|vb)script:/.test(val)) {
            el.removeAttribute(a.name);
          } else if (isUrl && !(val.startsWith('#') || /^data:image\/(png|jpeg|gif)/.test(val))) {
            el.removeAttribute(a.name);
          }
        });
      };
      const walk = (el) => {
        [...el.children].forEach(child => {
          const name = child.localName.toLowerCase();
          const animatesHref = (name === 'animate' || name === 'set') && /href/i.test(child.getAttribute('attributeName') || '');
          if (SVG_FORBIDDEN.has(name) || animatesHref) {
            child.remove();
            return;
          }
          cleanAttrs(child);
          walk(child);
        });
      };
      cleanAttrs(root);
      walk(root);
      clean = new XMLSerializer().serializeToString(root);
    }
  } catch (_) {
    clean = DEFAULT_SVG;
  }
  if (svgCache.size > 300) svgCache.clear();
  svgCache.set(raw, clean);
  return clean;
}

function readStoredToken() {
  try { return localStorage.getItem('scada_auth_token') || ''; } catch (_) { return ''; }
}

function storeToken(token) {
  try {
    if (token) localStorage.setItem('scada_auth_token', token);
    else localStorage.removeItem('scada_auth_token');
  } catch (_) {}
}

function canWrite() {
  return !!(state.currentUser && state.currentUser.permissions && state.currentUser.permissions.can_write_tags);
}

function isAdmin() {
  return !!(state.currentUser && state.currentUser.role === 'Admin');
}

// Injecte le jeton de session dans tous les appels API et gère l'expiration de session (401)
const _nativeFetch = window.fetch.bind(window);
window.fetch = async (url, options = {}) => {
  const headers = Object.assign({}, options.headers || {});
  if (state.authToken) headers['X-Scada-Token'] = state.authToken;
  const res = await _nativeFetch(url, Object.assign({}, options, { headers }));
  if (res.status === 401 && !String(url).startsWith('/api/auth/login')) {
    handleSessionExpired();
  }
  return res;
};

/** GET JSON tolérant : renvoie `fallback` si la requête échoue ou est refusée. */
async function getJson(url, fallback) {
  try {
    const res = await fetch(url);
    if (!res.ok) return fallback;
    return await res.json();
  } catch (_) {
    return fallback;
  }
}

/** Lit le message d'erreur JSON d'une réponse API. */
async function apiError(res) {
  try {
    const body = await res.json();
    return body.message || `Erreur HTTP ${res.status}`;
  } catch (_) {
    return `Erreur HTTP ${res.status}`;
  }
}

window.addEventListener('DOMContentLoaded', async () => {
  startSystemClock();
  renderScheduleDaysPicker();
  if (!state.authToken) {
    showLoginModal();
    return;
  }
  await startSession();
});

async function startSession() {
  const ok = await bootstrapData();
  if (ok) connectRealtimeSocket();
}

function startSystemClock() {
  const el = document.getElementById('sysClock');
  setInterval(() => {
    el.textContent = new Date().toLocaleTimeString('fr-FR');
  }, 1000);
}

async function bootstrapData() {
  const meRes = await fetch('/api/auth/me').catch(() => null);
  if (!meRes || !meRes.ok) {
    if (meRes && meRes.status !== 401) showLoginModal('Serveur indisponible : réessayez dans un instant.');
    return false;
  }
  state.currentUser = await meRes.json();
  const viewer = state.currentUser.role === 'Viewer';

  const [symbols, tags, dashboards, alarms, status, flows, drivers, schedules] = await Promise.all([
    getJson('/api/symbols', []),
    getJson('/api/tags', []),
    getJson('/api/dashboards', []),
    getJson('/api/alarms', []),
    getJson('/api/system/status', null),
    viewer ? [] : getJson('/api/flows', []),
    viewer ? [] : getJson('/api/drivers', []),
    viewer ? [] : getJson('/api/schedules', [])
  ]);

  state.symbols = symbols || [];

  state.tags = {};
  tags.forEach(t => {
    state.tags[t.id] = t;
    if (t.state && typeof t.state.value === 'number' && !state.history[t.id]) {
      state.history[t.id] = [t.state.value];
    }
  });

  state.dashboards = dashboards;
  if (!state.currentDashboardId || !dashboards.find(d => d.id === state.currentDashboardId)) {
    state.currentDashboardId = dashboards[0] ? dashboards[0].id : null;
  }

  state.flows = flows;
  if (!state.currentFlowId || !flows.find(f => f.id === state.currentFlowId)) {
    state.currentFlowId = flows[0] ? flows[0].id : null;
  }

  state.alarms = alarms;
  state.drivers = drivers;
  state.schedules = schedules;
  state.systemStatus = status;

  applyUserPermissionsToUi();
  updatePauseButtonUi();
  updateSysInfoPill();
  populateDashboardSelect();
  renderSettingsForm();
  populateFlowSelect();
  populateHistorianSelect();
  populateScheduleTagSelect();
  renderCustomSymbolsPalette();
  renderTagTree();
  renderDashboard();
  renderFlowEditor();
  renderAlarmBanner();
  renderAlarmsTable();
  if (!viewer && state.activeTab === 'tagdb') renderDriversAndTagDb();
  renderSchedulesTable();
  return true;
}

function updateSysInfoPill() {
  const el = document.getElementById('sysInfoPill');
  const st = state.systemStatus;
  if (el && st) el.textContent = `v${st.version} · PLC Modbus 127.0.0.1:${st.modbus_slave_port}`;
}

function getActiveDashboard() {
  return state.dashboards.find(d => d.id === state.currentDashboardId) || state.dashboards[0];
}

function getActiveFlow() {
  return state.flows.find(f => f.id === state.currentFlowId) || state.flows[0];
}

// ============================================================================
// CONTRÔLE D'ALIMENTATION & SYSTÈME (PAUSE / REDÉMARRER / ÉTEINDRE)
// ============================================================================

function updatePauseButtonUi() {
  const btn = document.getElementById('btnPauseAcq');
  if (!btn || !state.systemStatus) return;
  if (state.systemStatus.acquisition_paused) {
    btn.textContent = '▶️ Reprendre Acq.';
    btn.style.borderColor = '#10b981';
    btn.style.color = '#34d399';
  } else {
    btn.textContent = '⏸️ Pause Acq.';
    btn.style.borderColor = '';
    btn.style.color = '';
  }
}

async function toggleAcquisitionPause() {
  const nextPaused = !(state.systemStatus && state.systemStatus.acquisition_paused);
  const res = await fetch('/api/system/pause', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ paused: nextPaused })
  });
  if (res.ok) {
    state.systemStatus = await res.json();
    updatePauseButtonUi();
  } else {
    alert(await apiError(res));
  }
}

async function confirmServerRestart() {
  if (!confirm('Confirmer le REDÉMARRAGE du moteur Apex Supervision ?\nLes drivers actifs seront réinitialisés à chaud (les drivers arrêtés volontairement restent arrêtés).')) {
    return;
  }
  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').textContent = '🔄';
  document.getElementById('powerOverlayTitle').style.color = '#38bdf8';
  document.getElementById('powerOverlayTitle').textContent = 'Redémarrage de Apex Supervision en cours...';
  document.getElementById('powerOverlayMsg').textContent = 'Réinitialisation des drivers industriels actifs et rechargement des moteurs...';
  overlay.style.display = 'flex';

  try {
    const res = await fetch('/api/system/restart', { method: 'POST' });
    if (res.ok) state.systemStatus = await res.json();
  } catch (_) {}

  setTimeout(async () => {
    state.shuttingDown = false;
    overlay.style.display = 'none';
    await bootstrapData();
    if (state.activeTab === 'historian') await loadSystemAndHistorianTab();
  }, 900);
}

async function confirmServerShutdown() {
  if (!confirm('Confirmer la MISE HORS TENSION de l\'application Apex Supervision ?\nTous les drivers industriels seront déconnectés proprement.')) {
    return;
  }
  try {
    const res = await fetch('/api/system/standby', { method: 'POST' });
    if (res.ok) state.systemStatus = await res.json();
  } catch (_) {}

  showPowerOffOverlay();
}

function showPowerOffOverlay() {
  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').textContent = '⏻';
  document.getElementById('powerOverlayTitle').style.color = '#ef4444';
  document.getElementById('powerOverlayTitle').textContent = 'Application Apex Supervision Arrêtée (Hors Tension)';
  document.getElementById('powerOverlayMsg').innerHTML = `
    Tous les drivers industriels (Automate Modbus TCP, Simulateur GTB, MQTT) ont été arrêtés proprement.<br>
    L'acquisition temps réel est hors tension.<br><br>
    <div style="display:flex;gap:12px;justify-content:center;margin-top:16px;flex-wrap:wrap;">
      <button class="btn btn-success" style="padding:10px 20px;font-size:14px;" onclick="powerOnFromStandby()">⚡ Rallumer / Redémarrer l'Application</button>
      <button class="btn btn-danger" style="padding:10px 16px;font-size:12px;" onclick="terminateOsProcess()">🛑 Quitter définitivement le serveur Windows (.exe)</button>
    </div>
  `;
  overlay.style.display = 'flex';
}

async function powerOnFromStandby() {
  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').textContent = '⚡';
  document.getElementById('powerOverlayTitle').style.color = '#10b981';
  document.getElementById('powerOverlayTitle').textContent = 'Démarrage de Apex Supervision en cours...';
  document.getElementById('powerOverlayMsg').textContent = 'Remise sous tension des drivers industriels et synchronisation de la Tag Database...';

  try {
    const res = await fetch('/api/system/restart', { method: 'POST' });
    if (res.ok) state.systemStatus = await res.json();
  } catch (_) {}

  setTimeout(async () => {
    state.shuttingDown = false;
    overlay.style.display = 'none';
    await bootstrapData();
    if (state.activeTab === 'historian') await loadSystemAndHistorianTab();
  }, 700);
}

async function terminateOsProcess() {
  if (!confirm('Voulez-vous fermer définitivement le processus serveur Windows (rust-scada-flow.exe) ?')) {
    return;
  }
  state.shuttingDown = true;
  document.getElementById('powerOverlayMsg').innerHTML = `
    Le processus serveur Windows <code>rust-scada-flow.exe</code> a été arrêté.<br><br>
    Pour le relancer ultérieurement, double-cliquez sur <code>Demarrer-SCADA.bat</code> ou exécutez <code>cargo run</code>.
  `;
  try {
    await fetch('/api/system/shutdown', { method: 'POST' });
  } catch (_) {}
}

// ============================================================================
// WEBSOCKET TEMPS RÉEL (/ws/realtime?token=...) & RENDU GROUPÉ
// ============================================================================

function connectRealtimeSocket() {
  if (state.shuttingDown || !state.authToken) return;
  clearTimeout(state.wsRetryTimer);
  if (state.ws) {
    state.ws.onclose = null;
    try { state.ws.close(); } catch (_) {}
    state.ws = null;
  }

  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/realtime?token=${encodeURIComponent(state.authToken)}`);
  state.ws = ws;
  const pill = document.getElementById('wsStatus');
  const txt = document.getElementById('wsText');

  ws.onopen = () => {
    pill.classList.remove('offline');
    txt.textContent = 'LIVE COV ACTIF';
  };

  ws.onmessage = (evt) => {
    let msg;
    try { msg = JSON.parse(evt.data); } catch (_) { return; }

    if (msg.event === 'InitialSnapshot') {
      msg.data.tags.forEach(t => { state.tags[t.id] = t; });
      state.alarms = msg.data.alarms || [];
      if (state.currentUser && state.currentUser.role !== 'Viewer') {
        state.drivers = msg.data.drivers || [];
        state.schedules = msg.data.schedules || [];
      }
      state.systemStatus = msg.data.status || state.systemStatus;
      updatePauseButtonUi();
      updateSysInfoPill();
      renderTagTree();
      renderDashboard();
      renderAlarmBanner();
      renderAlarmsTable();
      if (state.activeTab === 'tagdb') renderTagCatalogTable();
    } else if (msg.event === 'TagUpdateBatch') {
      msg.data.forEach(applyTagState);
      scheduleRender();
    } else if (msg.event === 'TagUpdate') {
      applyTagState(msg.data);
      scheduleRender();
    } else if (msg.event === 'AlarmUpdate') {
      state.alarms = msg.data;
      renderAlarmBanner();
      renderAlarmsTable();
    } else if (msg.event === 'DriverListUpdate') {
      state.drivers = msg.data;
      if (state.activeTab === 'tagdb') renderDriversCardsOnly();
    } else if (msg.event === 'SettingsUpdate') {
      state.systemStatus = Object.assign({}, state.systemStatus || {}, { settings: msg.data });
      renderSettingsForm();
      renderAlarmsTable();
      renderAlarmBanner();
    } else if (msg.event === 'FlowTrace') {
      const tr = msg.data;
      state.nodeLiveValues[tr.from_node_id] = tr.value;
      state.nodeLiveValues[tr.to_node_id] = tr.value;
      if (state.activeTab === 'flow') animateFlowEdge(tr.from_node_id, tr.to_node_id);
    }
  };

  ws.onclose = () => {
    if (state.ws !== ws) return;
    state.ws = null;
    if (state.shuttingDown || !state.authToken) return;
    pill.classList.add('offline');
    txt.textContent = 'RECONNEXION...';
    state.wsRetryTimer = setTimeout(async () => {
      // Une session expirée est détectée par le wrapper fetch (401 → écran de connexion)
      const res = await fetch('/api/auth/me').catch(() => null);
      if (res && res.status === 401) return;
      connectRealtimeSocket();
    }, 2000);
  };
}

/** Intègre une mise à jour COV dans l'état local (le rendu est différé, voir scheduleRender). */
function applyTagState(st) {
  state.covCount++;
  if (state.tags[st.tag_id]) {
    state.tags[st.tag_id].state = st;
  }
  if (typeof st.value === 'number') {
    if (!state.history[st.tag_id]) state.history[st.tag_id] = [];
    state.history[st.tag_id].push(st.value);
    if (state.history[st.tag_id].length > 55) state.history[st.tag_id].shift();
  }
  state.pendingTagIds.add(st.tag_id);
}

/** Regroupe les mises à jour COV : au plus un rendu DOM par image affichée. */
function scheduleRender() {
  if (state.renderScheduled) return;
  state.renderScheduled = true;
  requestAnimationFrame(flushPendingRender);
}

function flushPendingRender() {
  state.renderScheduled = false;
  const ids = [...state.pendingTagIds];
  state.pendingTagIds.clear();
  const txt = document.getElementById('wsText');
  if (txt) txt.textContent = `COV LIVE (${state.covCount})`;
  const onTagDb = state.activeTab === 'tagdb';
  ids.forEach(id => {
    updateTagRowInPlace(id);
    updateBoundWidgetsInPlace(id);
    if (onTagDb) updateCatalogRowInPlace(id);
  });
}

// ============================================================================
// PANNEAU GAUCHE : ARBORESCENCE DES VARIABLES
// ============================================================================

function renderTagTree() {
  const container = document.getElementById('tagTree');
  const filter = (document.getElementById('tagSearch').value || '').toLowerCase();
  const all = Object.values(state.tags);
  document.getElementById('tagCountBadge').textContent = all.length;

  const groups = {};
  all.forEach(t => {
    if (filter && !t.id.toLowerCase().includes(filter) && !String(t.label).toLowerCase().includes(filter)) return;
    const prefix = t.id.split('.').slice(0, 2).join(' / ') || t.module_source;
    if (!groups[prefix]) groups[prefix] = [];
    groups[prefix].push(t);
  });

  const frag = document.createDocumentFragment();
  Object.entries(groups).forEach(([grpName, list]) => {
    const grp = document.createElement('div');
    grp.className = 'tree-group';
    grp.innerHTML = `<div class="tree-group-title">📂 ${esc(grpName)}</div>`;

    list.forEach(tag => {
      const item = document.createElement('div');
      item.className = 'tag-item';
      item.draggable = true;
      item.id = `tree-tag-${cssSafe(tag.id)}`;
      item.ondragstart = (e) => {
        e.dataTransfer.setData('text/plain', JSON.stringify({ kind: 'tag', tag_id: tag.id }));
      };
      item.onclick = () => inspectTagDirect(tag.id);
      item.innerHTML = buildTagItemHtml(tag);
      grp.appendChild(item);
    });

    frag.appendChild(grp);
  });
  container.innerHTML = '';
  container.appendChild(frag);
}

function buildTagItemHtml(tag) {
  const qual = tag.state ? tag.state.quality : 'Uncertain';
  return `
    <div class="tag-top">
      <span class="tag-id">${esc(tag.id)}</span>
      <span class="badge-qual q-${esc(qual)}">${esc(qual)}</span>
    </div>
    <div style="font-size:11px;color:#94a3b8;margin-bottom:4px;">${esc(tag.label)}</div>
    <div class="tag-meta">
      <span>${esc(tag.module_source)} ${tag.writable ? '<b class="badge-rw">RW</b>' : 'RO'}</span>
      <span class="tag-live-val">${formatTagValue(tag)}</span>
    </div>
  `;
}

function updateTagRowInPlace(tagId) {
  const el = document.getElementById(`tree-tag-${cssSafe(tagId)}`);
  if (el && state.tags[tagId]) {
    el.innerHTML = buildTagItemHtml(state.tags[tagId]);
  }
}

/** Valeur formatée et déjà échappée, prête à être insérée dans du HTML. */
function formatTagValue(tag) {
  if (!tag || !tag.state || tag.state.value === undefined) return '--';
  const v = tag.state.value;
  const u = tag.unit ? ` ${tag.unit}` : '';
  if (typeof v === 'boolean') return v ? 'MARCHE (1)' : 'ARRÊT (0)';
  if (typeof v === 'number') return esc(`${v.toFixed(2).replace(/\.00$/, '')}${u}`);
  return esc(`${v}${u}`);
}

function inspectTagDirect(tagId) {
  const tag = state.tags[tagId];
  if (!tag) return;
  const box = document.getElementById('inspectorContent');
  const val = tag.state ? tag.state.value : '';
  const writeAllowed = tag.writable && canWrite();
  box.innerHTML = `
    <div style="border-bottom:1px solid var(--border);padding-bottom:8px;">
      <div style="color:var(--accent);font-weight:700;font-family:Consolas;">${esc(tag.id)}</div>
      <div style="font-size:11px;color:#94a3b8;">${esc(tag.label)}</div>
    </div>
    <div class="form-group">
      <label>Valeur Temps Réel</label>
      <div class="big-num" style="font-size:20px;">${formatTagValue(tag)}</div>
    </div>
    <div class="form-group">
      <label>Source / Adresse Protocole</label>
      <input readonly value="${esc(tag.module_source)} — ${esc(tag.protocol_address)}" />
    </div>
    ${writeAllowed ? `
      <div class="form-group">
        <label>Forcer / Envoyer une Consigne</label>
        <div style="display:flex;gap:6px;">
          <input id="directWriteVal" value="${esc(val)}" />
          <button class="btn btn-primary" onclick="sendWriteTag(${jsArg(tag.id)}, parseInputForTag(${jsArg(tag.data_type)}, document.getElementById('directWriteVal').value))">Envoyer</button>
        </div>
      </div>
    ` : `<p style="color:#64748b;font-size:11px;">${tag.writable ? 'Envoi de consignes non autorisé pour votre profil.' : 'Variable en lecture seule (Capteur / Mesure).'}</p>`}
  `;
}

function parseInputForTag(dataType, rawStr) {
  if (dataType === 'Bool') {
    const s = String(rawStr).toLowerCase().trim();
    return s === 'true' || s === '1' || s === 'on' || s === 'marche';
  }
  if (dataType === 'Float' || dataType === 'Int') {
    return parseFloat(rawStr);
  }
  return rawStr;
}

// ============================================================================
// ATELIER 1 : SYNOPTIQUES MULTI-VUES & WIDGETS INDUSTRIELS
// ============================================================================

function populateDashboardSelect() {
  const sel = document.getElementById('dashboardSelect');
  sel.innerHTML = state.dashboards.map(d =>
    `<option value="${esc(d.id)}" ${d.id === state.currentDashboardId ? 'selected' : ''}>${esc(d.name)}</option>`
  ).join('');
}

function selectDashboard(id) {
  state.currentDashboardId = id;
  state.selectedWidgetId = null;
  renderDashboard();
}

async function promptNewDashboard() {
  const name = prompt('Nom du nouveau synoptique :', '04. Sous-Station CVC Zone Sud');
  if (!name) return;
  const newDash = {
    id: 'syn-' + Date.now(),
    name,
    category: 'Personnalisé',
    description: 'Synoptique créé dans le Studio',
    grid_size: 20,
    widgets: [],
    updated_at: new Date().toISOString()
  };
  const res = await fetch('/api/dashboards', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(newDash)
  });
  if (res.ok) {
    const saved = await res.json();
    state.dashboards.push(saved);
    state.currentDashboardId = saved.id;
    populateDashboardSelect();
    renderDashboard();
  } else {
    alert(await apiError(res));
  }
}

function duplicateSelectedWidget() {
  const dash = getActiveDashboard();
  if (!dash || !state.selectedWidgetId) return;
  const orig = dash.widgets.find(w => w.id === state.selectedWidgetId);
  if (!orig) return;
  const copy = JSON.parse(JSON.stringify(orig));
  copy.id = 'w-' + Date.now();
  copy.x += 30;
  copy.y += 30;
  dash.widgets.push(copy);
  state.selectedWidgetId = copy.id;
  renderDashboard();
  renderWidgetInspector(copy);
}

async function deleteCurrentDashboard() {
  if (state.dashboards.length <= 1) {
    alert('Impossible de supprimer le dernier synoptique.');
    return;
  }
  const dash = getActiveDashboard();
  if (!dash || !confirm(`Supprimer le synoptique "${dash.name}" ?`)) return;
  const res = await fetch(`/api/dashboards/${encodeURIComponent(dash.id)}`, { method: 'DELETE' });
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  state.dashboards = state.dashboards.filter(d => d.id !== dash.id);
  state.currentDashboardId = state.dashboards[0].id;
  populateDashboardSelect();
  renderDashboard();
}

async function saveCurrentDashboard() {
  const dash = getActiveDashboard();
  if (!dash) return;
  const res = await fetch('/api/dashboards', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(dash)
  });
  if (res.ok) {
    alert(`Synoptique "${dash.name}" enregistré dans SQLite.`);
  } else {
    alert(`Enregistrement refusé : ${await apiError(res)}`);
  }
}

function renderDashboard() {
  const container = document.getElementById('widgetsContainer');
  const dash = getActiveDashboard();
  const frag = document.createDocumentFragment();
  if (dash) {
    dash.widgets.forEach(w => frag.appendChild(buildWidgetCard(w)));
  }
  container.innerHTML = '';
  container.appendChild(frag);
}

function buildWidgetCard(w) {
  const card = document.createElement('div');
  card.className = 'scada-widget' + (state.selectedWidgetId === w.id ? ' selected' : '');
  card.id = `widget-${cssSafe(w.id)}`;
  card.style.left = `${w.x}px`;
  card.style.top = `${w.y}px`;
  card.style.width = `${w.width}px`;
  card.style.height = `${w.height}px`;

  card.onclick = (e) => {
    e.stopPropagation();
    state.selectedWidgetId = w.id;
    if (state.mode === 'studio') {
      renderDashboard();
      renderWidgetInspector(w);
    } else if (w.bound_tag_id) {
      inspectTagDirect(w.bound_tag_id);
    }
  };

  card.ondragover = (e) => {
    e.preventDefault();
    e.stopPropagation();
    card.classList.add('drop-hover');
  };
  card.ondragleave = () => card.classList.remove('drop-hover');
  card.ondrop = (e) => {
    e.preventDefault();
    e.stopPropagation();
    card.classList.remove('drop-hover');
    if (state.mode !== 'studio') return;
    const raw = e.dataTransfer.getData('text/plain');
    if (!raw) return;
    let data;
    try { data = JSON.parse(raw); } catch (_) { return; }
    if (data.kind === 'tag') {
      w.bound_tag_id = data.tag_id;
      if (state.tags[data.tag_id]) w.title = state.tags[data.tag_id].label;
      state.selectedWidgetId = w.id;
      renderDashboard();
      renderWidgetInspector(w);
    }
  };

  const header = document.createElement('div');
  header.className = 'widget-header';
  header.innerHTML = `
    <span>${esc(w.title)}</span>
    <span style="font-size:10px;color:#64748b;">${esc(w.widget_type)}</span>
  `;
  header.onmousedown = (e) => startWidgetDrag(e, w, card);

  const body = document.createElement('div');
  body.className = 'widget-body';
  body.id = `widget-body-${cssSafe(w.id)}`;
  body.innerHTML = buildWidgetBodyHtml(w);

  const resizeHandle = document.createElement('div');
  resizeHandle.className = 'resize-handle';
  resizeHandle.onmousedown = (e) => startWidgetResize(e, w, card);

  card.appendChild(header);
  card.appendChild(body);
  card.appendChild(resizeHandle);
  return card;
}

function buildWidgetBodyHtml(w) {
  const tag = w.bound_tag_id ? state.tags[w.bound_tag_id] : null;
  const val = tag && tag.state ? tag.state.value : null;
  const qual = esc(tag && tag.state ? tag.state.quality : 'Uncertain');
  const unit = tag && tag.unit ? esc(tag.unit) : '';
  const numVal = typeof val === 'number' ? val : 0;
  const boolVal = Boolean(val);

  if (w.widget_type === 'Gauge') {
    const min = Number((w.properties && w.properties.min) ?? 0);
    const max = Number((w.properties && w.properties.max) ?? 100);
    const pct = Math.max(0, Math.min(1, (numVal - min) / ((max - min) || 1)));
    const dash = (pct * 126).toFixed(1);
    const strokeCol = pct > 0.85 ? '#ef4444' : pct > 0.7 ? '#f59e0b' : '#38bdf8';
    return `
      <svg width="150" height="90" viewBox="0 0 120 72">
        <path d="M 15 60 A 45 45 0 0 1 105 60" fill="none" stroke="#1e293b" stroke-width="10" stroke-linecap="round"/>
        <path d="M 15 60 A 45 45 0 0 1 105 60" fill="none" stroke="${strokeCol}" stroke-width="10"
              stroke-dasharray="${dash} 150" stroke-linecap="round"/>
        <text x="60" y="53" text-anchor="middle" fill="#f8fafc" font-size="15.5" font-weight="bold" font-family="Consolas">
          ${numVal.toFixed(1)}${unit}
        </text>
      </svg>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'} (${qual})</div>
    `;
  }

  if (w.widget_type === 'Pump') {
    const color = boolVal ? '#10b981' : '#475569';
    return `
      <svg width="76" height="76" viewBox="0 0 100 100">
        <rect x="48" y="10" width="38" height="16" fill="#1e293b" stroke="${color}" stroke-width="3"/>
        <circle cx="50" cy="54" r="34" fill="#0b101b" stroke="${color}" stroke-width="4"/>
        <g class="${boolVal ? 'spin-active' : ''}" style="transform-origin:50px 54px;">
          <line x1="50" y1="26" x2="50" y2="82" stroke="${color}" stroke-width="5"/>
          <line x1="22" y1="54" x2="78" y2="54" stroke="${color}" stroke-width="5"/>
          <circle cx="50" cy="54" r="8" fill="${color}"/>
        </g>
      </svg>
      <button class="btn" style="margin-top:6px;" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!boolVal})">
        ${boolVal ? '🟢 MARCHE (Arrêter)' : '⚪ ARRÊT (Démarrer)'}
      </button>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Fan') {
    const color = boolVal ? '#38bdf8' : '#475569';
    return `
      <svg width="78" height="78" viewBox="0 0 100 100">
        <rect x="10" y="10" width="80" height="80" rx="12" fill="#0b101b" stroke="${color}" stroke-width="3"/>
        <g class="${boolVal ? 'spin-active' : ''}">
          <ellipse cx="50" cy="28" rx="9" ry="18" fill="${color}"/>
          <ellipse cx="50" cy="72" rx="9" ry="18" fill="${color}"/>
          <ellipse cx="28" cy="50" rx="18" ry="9" fill="${color}"/>
          <ellipse cx="72" cy="50" rx="18" ry="9" fill="${color}"/>
          <circle cx="50" cy="50" r="8" fill="#fff"/>
        </g>
      </svg>
      <button class="btn" style="margin-top:6px;" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!boolVal})">
        ${boolVal ? '🌀 SOUFFLAGE ACTIF' : '⚪ VENTILATEUR ARRÊTÉ'}
      </button>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Damper') {
    const pct = Math.max(0, Math.min(100, numVal));
    const angle = -45 + (pct / 100) * 45;
    return `
      <svg width="110" height="62" viewBox="0 0 120 66">
        <rect x="15" y="8" width="90" height="50" fill="#0b101b" stroke="#475569" stroke-width="3"/>
        <line x1="35" y1="12" x2="35" y2="54" stroke="#38bdf8" stroke-width="4" transform="rotate(${angle} 35 33)"/>
        <line x1="60" y1="12" x2="60" y2="54" stroke="#38bdf8" stroke-width="4" transform="rotate(${angle} 60 33)"/>
        <line x1="85" y1="12" x2="85" y2="54" stroke="#38bdf8" stroke-width="4" transform="rotate(${angle} 85 33)"/>
      </svg>
      <div style="font-weight:800;color:#38bdf8;font-size:14px;">Ouverture : ${pct.toFixed(0)} %</div>
      <input type="range" min="0" max="100" value="${pct}" onchange="sendWriteTag(${jsArg(w.bound_tag_id)}, parseFloat(this.value))" />
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Boiler') {
    return `
      <svg width="90" height="84" viewBox="0 0 100 95">
        <rect x="20" y="12" width="60" height="72" rx="10" fill="#131c31" stroke="#f59e0b" stroke-width="3"/>
        <path d="M 50 32 C 36 50, 40 68, 50 72 C 60 68, 64 50, 50 32 Z" fill="#f97316"/>
        <path d="M 50 45 C 43 55, 45 66, 50 70 C 55 66, 57 55, 50 45 Z" fill="#fde047"/>
      </svg>
      <div style="font-weight:800;font-size:17px;color:#fbbf24;font-family:Consolas;">${numVal.toFixed(1)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Chiller') {
    return `
      <svg width="105" height="78" viewBox="0 0 120 88">
        <rect x="12" y="12" width="96" height="64" rx="8" fill="#0c192c" stroke="#38bdf8" stroke-width="3"/>
        <circle cx="40" cy="44" r="18" fill="#070b14" stroke="#38bdf8" stroke-width="2.5"/>
        <circle cx="80" cy="44" r="18" fill="#070b14" stroke="#38bdf8" stroke-width="2.5"/>
        <text x="40" y="49" text-anchor="middle" fill="#38bdf8" font-size="16">❄</text>
        <text x="80" y="49" text-anchor="middle" fill="#38bdf8" font-size="16">❄</text>
      </svg>
      <div style="font-weight:800;font-size:17px;color:#38bdf8;font-family:Consolas;">Eau Glacée : ${numVal.toFixed(2)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'} (${qual})</div>
    `;
  }

  if (w.widget_type === 'PipeSegment') {
    const active = numVal > 0.1 || boolVal;
    const strokeCol = active ? '#38bdf8' : '#334155';
    return `
      <svg width="100%" height="44" viewBox="0 0 200 44">
        <rect x="4" y="14" width="192" height="16" rx="4" fill="#0f172a" stroke="${strokeCol}" stroke-width="3"/>
        ${active ? `<line x1="12" y1="22" x2="188" y2="22" stroke="#38bdf8" stroke-width="4" stroke-dasharray="10 8"/>` : ''}
      </svg>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Tuyauterie'}</div>
    `;
  }

  if (w.widget_type === 'Valve') {
    const pct = Math.max(0, Math.min(100, numVal));
    return `
      <svg width="110" height="58" viewBox="0 0 120 62">
        <polygon points="20,15 60,38 20,58" fill="#1e293b" stroke="#38bdf8" stroke-width="2.5"/>
        <polygon points="100,15 60,38 100,58" fill="#1e293b" stroke="#38bdf8" stroke-width="2.5"/>
        <line x1="60" y1="38" x2="60" y2="10" stroke="#f59e0b" stroke-width="4"/>
        <rect x="44" y="3" width="32" height="10" rx="3" fill="#f59e0b"/>
      </svg>
      <div style="font-weight:800;font-size:15px;color:#38bdf8;">${pct.toFixed(1)} %</div>
      <input type="range" min="0" max="100" value="${pct}" onchange="sendWriteTag(${jsArg(w.bound_tag_id)}, parseFloat(this.value))" />
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'SetpointButton') {
    const step = Number(w.properties && w.properties.step) || 0.5;
    return `
      <div class="big-num" style="color:#38bdf8;">${numVal.toFixed(1)} ${unit}</div>
      <div class="setpoint-controls">
        <button class="btn" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${(numVal - step).toFixed(2)})">-${step}</button>
        <input type="number" step="${step}" value="${numVal}" id="sp-in-${cssSafe(w.id)}" />
        <button class="btn" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${(numVal + step).toFixed(2)})">+${step}</button>
        <button class="btn btn-primary" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, parseFloat(document.getElementById(${jsArg('sp-in-' + cssSafe(w.id))}).value))">OK</button>
      </div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'SwitchButton') {
    return `
      <div style="margin:8px 0;">
        <button class="btn ${boolVal ? 'btn-primary' : ''}" style="padding:10px 18px;font-size:13px;"
                onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!boolVal})">
          ${boolVal ? '⚡ COMMUTATEUR : ON (1)' : '⚪ COMMUTATEUR : OFF (0)'}
        </button>
      </div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'TrendChart') {
    const pts = (w.bound_tag_id && state.history[w.bound_tag_id]) ? state.history[w.bound_tag_id] : [numVal];
    const min = Math.min(...pts, numVal - 0.5);
    const max = Math.max(...pts, numVal + 0.5);
    const wSvg = Math.max(200, w.width - 24);
    const hSvg = Math.max(85, w.height - 68);
    const poly = pts.map((v, idx) => {
      const x = (idx / Math.max(1, pts.length - 1)) * (wSvg - 12) + 6;
      const y = hSvg - 8 - ((v - min) / ((max - min) || 1)) * (hSvg - 16);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    }).join(' ');
    return `
      <svg width="${wSvg}" height="${hSvg}" style="background:#050911;border-radius:6px;border:1px solid #1e293b;">
        <polyline fill="none" stroke="#38bdf8" stroke-width="2.5" points="${poly}" />
      </svg>
      <div style="display:flex;justify-content:space-between;width:100%;margin-top:5px;font-size:11px;">
        <span class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</span>
        <b style="color:#38bdf8;font-family:Consolas;">${numVal.toFixed(2)} ${unit}</b>
      </div>
    `;
  }

  if (w.widget_type === 'TankLevel') {
    const pct = Math.max(0, Math.min(100, numVal));
    const hFill = (pct / 100) * 90;
    return `
      <svg width="92" height="114" viewBox="0 0 92 114">
        <rect x="16" y="10" width="60" height="92" rx="8" fill="#070b14" stroke="#475569" stroke-width="3"/>
        <rect x="19" y="${100 - hFill}" width="54" height="${hFill}" rx="5" fill="rgba(56, 189, 248, 0.65)"/>
        <text x="46" y="60" text-anchor="middle" fill="#fff" font-weight="bold" font-size="14">${pct.toFixed(0)}%</text>
      </svg>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'IndicatorLed') {
    const ledColor = boolVal ? '#ef4444' : '#10b981';
    const allowWrite = canWrite();
    return `
      <div style="display:flex;align-items:center;gap:10px;">
        <div style="width:22px;height:22px;border-radius:50%;background:${ledColor};box-shadow:0 0 14px ${ledColor};"></div>
        <div>
          <div style="font-weight:700;font-size:12.5px;color:${ledColor};">
            ${boolVal ? 'DÉFAUT / ALARME' : 'ÉTAT NORMAL'}
          </div>
          <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
        </div>
        ${allowWrite ? `<button class="btn" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!boolVal})">Tester</button>` : ''}
      </div>
    `;
  }

  // --- NOUVEAUX ÉLÉMENTS INDUSTRIELS ENRICHIS (P&ID ISO 10628 / CVC / IEC 60617) ---

  if (w.widget_type === 'MotorValve2Way') {
    const open = boolVal || numVal > 5;
    const col = open ? '#10b981' : '#ef4444';
    return `
      <svg width="105" height="62" viewBox="0 0 120 68">
        <polygon points="18,18 60,42 18,62" fill="${open ? '#064e3b' : '#450a0a'}" stroke="${col}" stroke-width="3"/>
        <polygon points="102,18 60,42 102,62" fill="${open ? '#064e3b' : '#450a0a'}" stroke="${col}" stroke-width="3"/>
        <line x1="60" y1="42" x2="60" y2="14" stroke="#f59e0b" stroke-width="3.5"/>
        <circle cx="60" cy="11" r="9" fill="#0b1120" stroke="#f59e0b" stroke-width="2.5"/>
        <text x="60" y="15" text-anchor="middle" fill="#f59e0b" font-size="10" font-weight="bold">M</text>
      </svg>
      <button class="btn" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!open})">
        ${open ? '🟢 VANNE OUVERTE' : '🔴 VANNE FERMÉE'}
      </button>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Compressor') {
    const active = boolVal || numVal > 0.5;
    const col = active ? '#10b981' : '#64748b';
    return `
      <svg width="88" height="82" viewBox="0 0 100 94">
        <circle cx="50" cy="48" r="35" fill="#0b1120" stroke="${col}" stroke-width="3.5"/>
        <polygon points="24,26 80,36 80,60 24,70" fill="none" stroke="${col}" stroke-width="2.5"/>
        <g class="${active ? 'spin-active' : ''}" style="transform-origin:50px 48px;">
          <circle cx="50" cy="48" r="8" fill="${col}"/>
          <line x1="36" y1="48" x2="64" y2="48" stroke="#fff" stroke-width="3"/>
        </g>
      </svg>
      <div style="font-weight:700;font-size:12px;color:${col};">${active ? 'COMPRESSEUR EN CHARGE' : 'ARRÊT'}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'HeatExchanger') {
    return `
      <svg width="110" height="74" viewBox="0 0 120 80">
        <rect x="16" y="10" width="88" height="60" rx="6" fill="#0b1120" stroke="#f97316" stroke-width="3"/>
        <polyline points="26,20 42,60 58,20 74,60 90,20" fill="none" stroke="#38bdf8" stroke-width="3"/>
        <line x1="6" y1="25" x2="16" y2="25" stroke="#ef4444" stroke-width="4"/>
        <line x1="104" y1="55" x2="114" y2="55" stroke="#38bdf8" stroke-width="4"/>
      </svg>
      <div style="font-weight:800;font-size:15px;color:#fb923c;font-family:Consolas;">ΔT / Échangeur : ${numVal.toFixed(1)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'CoolingTower') {
    const active = boolVal || numVal > 0;
    return `
      <svg width="92" height="86" viewBox="0 0 100 94">
        <polygon points="28,18 72,18 84,82 16,82" fill="#0b1120" stroke="#38bdf8" stroke-width="3"/>
        <ellipse cx="50" cy="18" rx="22" ry="6" fill="#0f172a" stroke="#38bdf8" stroke-width="2.5"/>
        <path d="M 30 58 Q 50 48 70 58" fill="none" stroke="#38bdf8" stroke-width="2.5" stroke-dasharray="4 3"/>
        <path d="M 24 72 Q 50 62 76 72" fill="none" stroke="#38bdf8" stroke-width="2.5"/>
      </svg>
      <div style="font-weight:800;font-size:14px;color:#38bdf8;">${numVal.toFixed(1)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || (active ? 'Tour Active' : 'Tour')}</div>
    `;
  }

  if (w.widget_type === 'HeaterBattery') {
    const col = numVal > 20 ? '#ef4444' : '#38bdf8';
    return `
      <svg width="96" height="76" viewBox="0 0 100 80">
        <rect x="18" y="10" width="64" height="60" rx="5" fill="#0b1120" stroke="${col}" stroke-width="3"/>
        <polyline points="28,22 72,22 28,38 72,38 28,54 72,54" fill="none" stroke="${col}" stroke-width="3"/>
      </svg>
      <div style="font-weight:800;font-size:15px;color:${col};font-family:Consolas;">${numVal.toFixed(1)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Batterie Thermique'}</div>
    `;
  }

  if (w.widget_type === 'FilterUnit') {
    const clogged = boolVal || numVal > 200;
    const col = clogged ? '#ef4444' : '#10b981';
    return `
      <svg width="92" height="74" viewBox="0 0 100 80">
        <rect x="18" y="10" width="64" height="60" fill="#0b1120" stroke="${col}" stroke-width="3"/>
        <polyline points="30,12 70,26 30,40 70,54 30,68" fill="none" stroke="${col}" stroke-width="2.5" stroke-dasharray="4 2"/>
      </svg>
      <div style="font-weight:700;font-size:12px;color:${col};">${clogged ? '⚠️ COLMATAGE FILTRE ΔP' : '✅ FILTRE PROPRE'}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Filtre F7'}</div>
    `;
  }

  if (w.widget_type === 'CircuitBreaker') {
    const closed = boolVal || numVal > 0;
    const col = closed ? '#ef4444' : '#10b981';
    return `
      <svg width="86" height="82" viewBox="0 0 100 94">
        <line x1="50" y1="6" x2="50" y2="30" stroke="#94a3b8" stroke-width="4"/>
        <circle cx="50" cy="30" r="4" fill="#fff"/>
        <line x1="50" y1="66" x2="50" y2="90" stroke="#94a3b8" stroke-width="4"/>
        <circle cx="50" cy="66" r="4" fill="#fff"/>
        <line x1="50" y1="66" x2="${closed ? 50 : 28}" y2="${closed ? 30 : 34}" stroke="${col}" stroke-width="5"/>
      </svg>
      <button class="btn" onclick="sendWriteTag(${jsArg(w.bound_tag_id)}, ${!closed})">
        ${closed ? '⚡ ENCLENCHÉ (FERMÉ)' : '🟢 OUVERT (ISOLÉ)'}
      </button>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Disjoncteur'}</div>
    `;
  }

  if (w.widget_type === 'Transformer') {
    return `
      <svg width="96" height="80" viewBox="0 0 100 84">
        <circle cx="50" cy="30" r="20" fill="none" stroke="#f59e0b" stroke-width="3.5"/>
        <circle cx="50" cy="54" r="20" fill="none" stroke="#38bdf8" stroke-width="3.5"/>
      </svg>
      <div style="font-weight:800;font-size:15px;color:#fbbf24;font-family:Consolas;">${numVal.toFixed(1)} ${unit}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Transfo HTA/BT'}</div>
    `;
  }

  if (w.widget_type === 'ElectricMotor') {
    const run = boolVal || numVal > 0;
    const col = run ? '#10b981' : '#64748b';
    return `
      <svg width="86" height="82" viewBox="0 0 100 94">
        <circle cx="50" cy="48" r="34" fill="#0b1120" stroke="${col}" stroke-width="3.5"/>
        <text x="50" y="46" text-anchor="middle" fill="${col}" font-size="18" font-weight="bold">M</text>
        <text x="50" y="64" text-anchor="middle" fill="#94a3b8" font-size="13">3~</text>
      </svg>
      <div style="font-weight:700;font-size:12px;color:${col};">${run ? '🟢 MOTEUR 3~ ACTIF' : '⚪ ARRÊTÉ'}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Moteur Asynchrone'}</div>
    `;
  }

  if (w.widget_type === 'FlowMeter') {
    return `
      <svg width="96" height="76" viewBox="0 0 100 80">
        <line x1="6" y1="45" x2="94" y2="45" stroke="#38bdf8" stroke-width="4"/>
        <circle cx="50" cy="45" r="24" fill="#0b1120" stroke="#38bdf8" stroke-width="3.5"/>
        <text x="50" y="42" text-anchor="middle" fill="#38bdf8" font-size="12" font-weight="bold">FIT</text>
        <text x="50" y="56" text-anchor="middle" fill="#f8fafc" font-size="10">${numVal.toFixed(1)}</text>
      </svg>
      <div style="font-weight:800;font-size:15px;color:#38bdf8;font-family:Consolas;">${numVal.toFixed(2)} ${unit || 'm³/h'}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Débitmètre ISA'}</div>
    `;
  }

  if (w.widget_type === 'PressureGauge') {
    const min = Number((w.properties && w.properties.min) ?? 0);
    const max = Number((w.properties && w.properties.max) ?? 10);
    const pct = Math.max(0, Math.min(1, (numVal - min) / ((max - min) || 1)));
    const deg = -120 + pct * 240;
    return `
      <svg width="90" height="86" viewBox="0 0 100 94">
        <circle cx="50" cy="48" r="36" fill="#0b1120" stroke="#38bdf8" stroke-width="3.5"/>
        <line x1="50" y1="48" x2="50" y2="20" stroke="#ef4444" stroke-width="3" transform="rotate(${deg} 50 48)"/>
        <circle cx="50" cy="48" r="4" fill="#fff"/>
        <text x="50" y="72" text-anchor="middle" fill="#38bdf8" font-size="11" font-weight="bold">${numVal.toFixed(2)} ${unit || 'bar'}</text>
      </svg>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Manomètre'}</div>
    `;
  }

  if (w.widget_type === 'Thermometer') {
    const pct = Math.max(5, Math.min(100, ((numVal + 10) / 110) * 100));
    const hCol = (pct / 100) * 56;
    const col = numVal > 60 ? '#ef4444' : numVal > 30 ? '#f59e0b' : '#38bdf8';
    return `
      <svg width="70" height="96" viewBox="0 0 70 100">
        <rect x="27" y="10" width="16" height="64" rx="8" fill="#0b1120" stroke="#475569" stroke-width="2.5"/>
        <rect x="30" y="${70 - hCol}" width="10" height="${hCol}" fill="${col}"/>
        <circle cx="35" cy="78" r="13" fill="${col}" stroke="#475569" stroke-width="2.5"/>
      </svg>
      <div style="font-weight:800;font-size:15px;color:${col};font-family:Consolas;">${numVal.toFixed(1)} ${unit || '°C'}</div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Thermomètre'}</div>
    `;
  }

  if (w.widget_type === 'PushButton') {
    return `
      <button class="btn btn-primary" style="padding:12px 20px;border-radius:30px;font-weight:800;"
              onmousedown="sendWriteTag(${jsArg(w.bound_tag_id)}, true)"
              onmouseup="sendWriteTag(${jsArg(w.bound_tag_id)}, false)"
              onmouseleave="if (event.buttons & 1) sendWriteTag(${jsArg(w.bound_tag_id)}, false)"
              ontouchstart="event.preventDefault(); sendWriteTag(${jsArg(w.bound_tag_id)}, true)"
              ontouchend="event.preventDefault(); sendWriteTag(${jsArg(w.bound_tag_id)}, false)">
        👆 IMPULSION MARCHE
      </button>
      <div class="widget-tag-sub" style="margin-top:6px;">${esc(w.bound_tag_id) || 'Bouton Impulsion'}</div>
    `;
  }

  // --- RENDU DYNAMIQUE DES SYMBOLES GÉNÉRIQUES IMPORTÉS (CustomSvgSymbol) ---
  if (w.widget_type === 'CustomSvgSymbol') {
    const symId = w.properties && w.properties.symbol_id;
    const sym = state.symbols.find(s => s.id === symId);
    const isActive = boolVal || numVal > 0.1;
    const dynColor = qual !== 'Good' ? '#f59e0b' : (isActive ? '#10b981' : '#38bdf8');
    let svgHtml = sanitizeSvg(sym ? sym.svg_content : (w.properties && w.properties.svg_content) || DEFAULT_SVG);
    if (isActive && ((sym && sym.animate_rotation) || (w.properties && w.properties.animate_rotation))) {
      svgHtml = svgHtml.replace('sym-rotatable', 'sym-rotatable spin-active');
    }
    return `
      <div class="custom-svg-wrapper" style="color:${dynColor};">
        ${svgHtml}
      </div>
      <div style="font-weight:800;font-size:13.5px;color:${dynColor};font-family:Consolas;margin-top:4px;">
        ${typeof val === 'boolean' ? (val ? 'MARCHE (1)' : 'ARRÊT (0)') : `${numVal.toFixed(1)} ${unit}`}
      </div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || (sym ? esc(sym.standard) : 'Symbole SVG')}</div>
    `;
  }

  return `
    <div class="big-num">${numVal.toFixed(2)} <span style="font-size:14px;color:#38bdf8;">${unit}</span></div>
    <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'} (${qual})</div>
  `;
}

function updateBoundWidgetsInPlace(tagId) {
  const dash = getActiveDashboard();
  if (!dash || state.activeTab !== 'synoptic') return;
  dash.widgets.forEach(w => {
    if (w.bound_tag_id === tagId) {
      // Ne pas écraser une saisie de consigne en cours
      if (w.widget_type === 'SetpointButton' && document.activeElement && document.activeElement.id === `sp-in-${cssSafe(w.id)}`) {
        return;
      }
      const el = document.getElementById(`widget-body-${cssSafe(w.id)}`);
      if (el) el.innerHTML = buildWidgetBodyHtml(w);
    }
  });
}

function onPaletteDrag(e, widgetType) {
  e.dataTransfer.setData('text/plain', JSON.stringify({ kind: 'palette', widget_type: widgetType }));
}

function onCustomSymbolDrag(e, symbolId) {
  e.dataTransfer.setData('text/plain', JSON.stringify({ kind: 'custom_symbol', symbol_id: symbolId }));
}

function handleCanvasDrop(e) {
  e.preventDefault();
  if (state.mode !== 'studio') return;
  const dash = getActiveDashboard();
  if (!dash) return;
  const raw = e.dataTransfer.getData('text/plain');
  if (!raw) return;
  let payload;
  try { payload = JSON.parse(raw); } catch (_) { return; }
  const rect = document.getElementById('synopticSurface').getBoundingClientRect();
  const x = Math.max(10, Math.round((e.clientX - rect.left - 90) / 20) * 20);
  const y = Math.max(10, Math.round((e.clientY - rect.top - 40) / 20) * 20);

  let newW = null;
  if (payload.kind === 'tag') {
    const tag = state.tags[payload.tag_id];
    if (!tag) return;
    let wType = 'NumericDisplay';
    if (tag.data_type === 'Bool') wType = tag.writable ? 'Pump' : 'IndicatorLed';
    else if (tag.writable) wType = 'SetpointButton';
    else wType = 'Gauge';

    newW = {
      id: 'w-' + Date.now(),
      x, y,
      width: 230,
      height: 195,
      widget_type: wType,
      bound_tag_id: tag.id,
      title: tag.label,
      properties: { min: 0, max: 100, step: 0.5 }
    };
  } else if (payload.kind === 'palette' && WIDGET_TYPES.includes(payload.widget_type)) {
    newW = {
      id: 'w-' + Date.now(),
      x, y,
      width: payload.widget_type === 'TrendChart' ? 440 : 230,
      height: payload.widget_type === 'PipeSegment' ? 95 : 195,
      widget_type: payload.widget_type,
      bound_tag_id: Object.keys(state.tags)[0] || null,
      title: `Composant ${payload.widget_type}`,
      properties: { min: 0, max: 100, step: 0.5 }
    };
  } else if (payload.kind === 'custom_symbol') {
    const sym = state.symbols.find(s => s.id === payload.symbol_id);
    if (!sym) return;
    newW = {
      id: 'w-' + Date.now(),
      x, y,
      width: sym.default_width || 210,
      height: sym.default_height || 190,
      widget_type: 'CustomSvgSymbol',
      bound_tag_id: Object.keys(state.tags)[0] || null,
      title: sym.name,
      properties: {
        symbol_id: sym.id,
        animate_rotation: sym.animate_rotation,
        svg_content: sym.svg_content
      }
    };
  }
  if (!newW) return;
  dash.widgets.push(newW);
  state.selectedWidgetId = newW.id;
  renderDashboard();
  renderWidgetInspector(newW);
}

function startWidgetDrag(e, widget, cardEl) {
  if (state.mode !== 'studio') return;
  e.preventDefault();
  state.selectedWidgetId = widget.id;
  renderWidgetInspector(widget);

  const sx = e.clientX, sy = e.clientY;
  const ox = widget.x, oy = widget.y;

  function onMove(ev) {
    widget.x = Math.max(0, Math.round((ox + (ev.clientX - sx)) / 10) * 10);
    widget.y = Math.max(0, Math.round((oy + (ev.clientY - sy)) / 10) * 10);
    cardEl.style.left = `${widget.x}px`;
    cardEl.style.top = `${widget.y}px`;
  }
  function onUp() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function startWidgetResize(e, widget, cardEl) {
  if (state.mode !== 'studio') return;
  e.preventDefault();
  e.stopPropagation();
  const sx = e.clientX, sy = e.clientY;
  const ow = widget.width, oh = widget.height;

  function onMove(ev) {
    widget.width = Math.max(150, Math.round((ow + (ev.clientX - sx)) / 10) * 10);
    widget.height = Math.max(85, Math.round((oh + (ev.clientY - sy)) / 10) * 10);
    cardEl.style.width = `${widget.width}px`;
    cardEl.style.height = `${widget.height}px`;
  }
  function onUp() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    renderDashboard();
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function renderWidgetInspector(w) {
  const box = document.getElementById('inspectorContent');
  const tagOpts = Object.values(state.tags).map(t =>
    `<option value="${esc(t.id)}" ${w.bound_tag_id === t.id ? 'selected' : ''}>${esc(t.id)} — ${esc(t.label)}</option>`
  ).join('');

  const props = w.properties || {};
  const minVal = Number(props.min ?? 0);
  const maxVal = Number(props.max ?? 100);
  const stepVal = Number(props.step ?? 0.5);

  box.innerHTML = `
    <div class="form-group">
      <label>Titre du Widget</label>
      <input value="${esc(w.title)}" oninput="updateWidgetField('title', this.value)" />
    </div>
    <div class="form-group">
      <label>Type de Composant Graphique</label>
      <select onchange="updateWidgetField('widget_type', this.value)">
        ${WIDGET_TYPES.map(t => `<option value="${t}" ${w.widget_type === t ? 'selected' : ''}>${t}</option>`).join('')}
      </select>
    </div>
    <div class="form-group">
      <label>Variable Liée (Data-Binding)</label>
      <select onchange="updateWidgetField('bound_tag_id', this.value || null)">
        <option value="">-- Non lié --</option>
        ${tagOpts}
      </select>
    </div>
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:8px;">
      <div class="form-group">
        <label>Échelle Min</label>
        <input type="number" value="${minVal}" onchange="updateWidgetProp('min', parseFloat(this.value))" />
      </div>
      <div class="form-group">
        <label>Échelle Max</label>
        <input type="number" value="${maxVal}" onchange="updateWidgetProp('max', parseFloat(this.value))" />
      </div>
      <div class="form-group">
        <label>Pas Consigne</label>
        <input type="number" step="0.1" min="0.01" value="${stepVal}" onchange="updateWidgetProp('step', parseFloat(this.value))" />
      </div>
    </div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:8px;">
      <div class="form-group">
        <label>Largeur (px)</label>
        <input type="number" value="${Number(w.width)}" onchange="updateWidgetField('width', parseFloat(this.value))" />
      </div>
      <div class="form-group">
        <label>Hauteur (px)</label>
        <input type="number" value="${Number(w.height)}" onchange="updateWidgetField('height', parseFloat(this.value))" />
      </div>
    </div>
    <button class="btn btn-danger" onclick="deleteWidget(${jsArg(w.id)})">🗑️ Supprimer le Widget</button>
  `;
}

function updateWidgetField(field, val) {
  const dash = getActiveDashboard();
  if (!dash) return;
  const w = dash.widgets.find(x => x.id === state.selectedWidgetId);
  if (w) { w[field] = val; renderDashboard(); }
}

function updateWidgetProp(prop, val) {
  const dash = getActiveDashboard();
  if (!dash) return;
  const w = dash.widgets.find(x => x.id === state.selectedWidgetId);
  if (w) {
    if (!w.properties) w.properties = {};
    w.properties[prop] = val;
    renderDashboard();
  }
}

function deleteWidget(id) {
  const dash = getActiveDashboard();
  if (!dash) return;
  dash.widgets = dash.widgets.filter(w => w.id !== id);
  state.selectedWidgetId = null;
  renderDashboard();
}

// ============================================================================
// ATELIER 2 : ÉDITEUR VISUEL DE FLUX NODE-RED (9 TYPES DE BLOCS)
// ============================================================================

function populateFlowSelect() {
  const sel = document.getElementById('flowSelect');
  sel.innerHTML = state.flows.map(f =>
    `<option value="${esc(f.id)}" ${f.id === state.currentFlowId ? 'selected' : ''}>${esc(f.name)} ${f.enabled ? '🟢' : '⚪'}</option>`
  ).join('');
  updateFlowToggleBtn();
}

function updateFlowToggleBtn() {
  const flow = getActiveFlow();
  const btn = document.getElementById('btnToggleFlow');
  if (!flow || !btn) return;
  btn.textContent = flow.enabled ? '🟢 Flux Actif' : '⚪ Flux Inactif';
}

function selectFlow(id) {
  state.currentFlowId = id;
  state.selectedFlowNodeId = null;
  state.wiringSource = null;
  updateFlowToggleBtn();
  renderFlowEditor();
}

async function toggleCurrentFlowEnabled() {
  const flow = getActiveFlow();
  if (!flow) return;
  flow.enabled = !flow.enabled;
  await saveCurrentFlow(true);
  populateFlowSelect();
}

async function deleteCurrentFlow() {
  if (state.flows.length <= 1) {
    alert('Impossible de supprimer le dernier flux.');
    return;
  }
  const flow = getActiveFlow();
  if (!flow || !confirm(`Supprimer le flux "${flow.name}" ?`)) return;
  const res = await fetch(`/api/flows/${encodeURIComponent(flow.id)}`, { method: 'DELETE' });
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  state.flows = state.flows.filter(f => f.id !== flow.id);
  state.currentFlowId = state.flows[0].id;
  populateFlowSelect();
  renderFlowEditor();
}

async function triggerCurrentFlowNow() {
  const flow = getActiveFlow();
  if (!flow) return;
  const res = await fetch(`/api/flows/${encodeURIComponent(flow.id)}/trigger`, { method: 'POST' });
  if (!res.ok) alert(await apiError(res));
}

async function promptNewFlow() {
  const name = prompt('Nom du nouveau graphe d\'automatisme :', '03. Délestage Électrique TGBT');
  if (!name) return;
  const newFlow = {
    id: 'flow-' + Date.now(),
    name,
    description: 'Graphe logique personnalisé',
    enabled: true,
    nodes: [],
    edges: []
  };
  const res = await fetch('/api/flows', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(newFlow)
  });
  if (res.ok) {
    const saved = await res.json();
    state.flows.push(saved);
    state.currentFlowId = saved.id;
    populateFlowSelect();
    renderFlowEditor();
  } else {
    alert(await apiError(res));
  }
}

function addFlowNode(nodeType) {
  const flow = getActiveFlow();
  if (!flow) return;
  const defaultTag = Object.keys(state.tags)[0] || 'GTB.CTA01.Temp_Soufflage';

  let kind = { node_type: 'TagInput', tag_id: defaultTag };
  let label = `Nœud ${nodeType}`;

  if (nodeType === 'TagInput') {
    label = `TagInput : ${defaultTag}`;
    kind = { node_type: 'TagInput', tag_id: defaultTag };
  } else if (nodeType === 'PidController') {
    label = 'Régulateur PID';
    kind = {
      node_type: 'PidController',
      setpoint_tag_id: 'GTB.CTA01.Consigne_Temp',
      kp: 12.0, ki: 1.5, kd: 0.2,
      min_out: 0.0, max_out: 100.0,
      reverse_acting: false
    };
  } else if (nodeType === 'Threshold') {
    label = 'Seuil Comparateur';
    kind = { node_type: 'Threshold', operator: 'Gt', threshold: 25.0, hysteresis: 0.3 };
  } else if (nodeType === 'Hysteresis') {
    label = 'Thermostat Hystérésis';
    kind = { node_type: 'Hysteresis', low_threshold: 18.0, high_threshold: 22.0, invert: false };
  } else if (nodeType === 'LogicGate') {
    label = 'Porte Logique AND';
    kind = { node_type: 'LogicGate', gate: 'And' };
  } else if (nodeType === 'DelayTimer') {
    label = 'Tempo TON (2000 ms)';
    kind = { node_type: 'DelayTimer', delay_ms: 2000 };
  } else if (nodeType === 'MathFormula') {
    label = 'Formule Linéaire';
    kind = { node_type: 'MathFormula', factor: 1.0, offset: 0.0 };
  } else if (nodeType === 'RhaiScript') {
    label = 'Calcul Script Rhai';
    kind = { node_type: 'RhaiScript', script: '// Formule personnalisée\nclamp(val * 1.1, 0.0, 100.0)' };
  } else if (nodeType === 'TagOutput') {
    label = 'TagOutput : Vanne_Chaud_Cmd';
    kind = { node_type: 'TagOutput', target_tag_id: 'GTB.CTA01.Vanne_Chaud_Cmd' };
  }

  const newNode = {
    id: 'n-' + Date.now(),
    label,
    x: 100 + (flow.nodes.length * 45) % 500,
    y: 100 + (flow.nodes.length * 35) % 300,
    kind
  };

  flow.nodes.push(newNode);
  state.selectedFlowNodeId = newNode.id;
  renderFlowEditor();
  renderFlowNodeInspector(newNode);
}

/** Dessine uniquement les câbles (utilisé seul pendant le déplacement d'un nœud). */
function renderFlowEdges(flow) {
  const svg = document.getElementById('flowSvg');
  svg.innerHTML = '';
  if (!flow) return;
  const nodesById = new Map(flow.nodes.map(n => [n.id, n]));

  flow.edges.forEach(edge => {
    const fromN = nodesById.get(edge.from_node);
    const toN = nodesById.get(edge.to_node);
    if (!fromN || !toN) return;

    const x1 = fromN.x + 235;
    const y1 = fromN.y + (edge.from_port === 'false' ? 75 : 52);
    const x2 = toN.x;
    const y2 = toN.y + 52;
    const cx1 = x1 + 70;
    const cx2 = x2 - 70;

    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', `M ${x1} ${y1} C ${cx1} ${y1}, ${cx2} ${y2}, ${x2} ${y2}`);
    path.setAttribute('class', 'edge-path');
    path.setAttribute('id', `edge-${cssSafe(edge.from_node)}-${cssSafe(edge.to_node)}`);
    path.onclick = () => {
      if (confirm(`Supprimer le câble reliant "${fromN.label}" à "${toN.label}" ?`)) {
        flow.edges = flow.edges.filter(e => e.id !== edge.id);
        renderFlowEditor();
      }
    };
    svg.appendChild(path);
  });
}

function renderFlowEditor() {
  const flow = getActiveFlow();
  const container = document.getElementById('flowNodesContainer');
  container.innerHTML = '';
  renderFlowEdges(flow);
  if (!flow) return;

  const frag = document.createDocumentFragment();
  flow.nodes.forEach(node => {
    const el = document.createElement('div');
    el.className = 'flow-node' + (state.selectedFlowNodeId === node.id ? ' selected' : '');
    el.style.left = `${node.x}px`;
    el.style.top = `${node.y}px`;

    let headerColor = '#0284c7';
    let bodySummary = '';
    let outPorts = ['out'];

    const k = node.kind;
    if (k.node_type === 'TagInput') {
      headerColor = '#0ea5e9';
      bodySummary = `Entrée : ${k.tag_id}`;
    } else if (k.node_type === 'PidController') {
      headerColor = '#2563eb';
      bodySummary = `SP: ${k.setpoint_tag_id}\nKp=${k.kp} Ki=${k.ki} Kd=${k.kd}`;
    } else if (k.node_type === 'Threshold') {
      headerColor = '#9333ea';
      bodySummary = `Si val ${k.operator} ${k.threshold} (hyst ${k.hysteresis})`;
      outPorts = ['true', 'false'];
    } else if (k.node_type === 'Hysteresis') {
      headerColor = '#7c3aed';
      bodySummary = `ON <= ${k.low_threshold} | OFF >= ${k.high_threshold}`;
      outPorts = ['true', 'false'];
    } else if (k.node_type === 'LogicGate') {
      headerColor = '#6366f1';
      bodySummary = `Porte combinatoire : ${k.gate}`;
    } else if (k.node_type === 'DelayTimer') {
      headerColor = '#0891b2';
      bodySummary = `Temporisation TON : ${k.delay_ms} ms`;
    } else if (k.node_type === 'MathFormula') {
      headerColor = '#4f46e5';
      bodySummary = `Sortie = val × ${k.factor} + ${k.offset}`;
    } else if (k.node_type === 'RhaiScript') {
      headerColor = '#d97706';
      bodySummary = String(k.script || '').split('\n').slice(0, 3).join('\n');
    } else if (k.node_type === 'TagOutput') {
      headerColor = '#059669';
      bodySummary = `Cible -> ${k.target_tag_id}`;
      outPorts = [];
    }

    const liveVal = state.nodeLiveValues[node.id];
    const liveBadge = liveVal !== undefined
      ? `<span style="background:rgba(0,0,0,0.35);padding:1px 6px;border-radius:4px;font-family:Consolas;">${esc(typeof liveVal === 'number' ? liveVal.toFixed(2) : liveVal)}</span>`
      : `<span>${esc(k.node_type)}</span>`;

    const inPortHtml = k.node_type !== 'TagInput'
      ? `<span class="port-pill" onclick="handlePortClick(event, ${jsArg(node.id)}, 'in', true)">⬅ IN</span>`
      : `<span></span>`;

    const outPortsHtml = outPorts.map(p => {
      const isSource = state.wiringSource && state.wiringSource.nodeId === node.id && state.wiringSource.port === p;
      return `<span class="port-pill ${isSource ? 'wiring-source' : ''}" onclick="handlePortClick(event, ${jsArg(node.id)}, ${jsArg(p)}, false)">OUT:${esc(p)} ➡</span>`;
    }).join(' ');

    el.innerHTML = `
      <div class="flow-node-header" style="background:${headerColor};color:#fff;">
        <span>${esc(node.label)}</span>
        ${liveBadge}
      </div>
      <div class="flow-node-body" style="white-space:pre-line;">${esc(bodySummary)}</div>
      <div class="flow-ports-row">
        <div>${inPortHtml}</div>
        <div style="display:flex;gap:4px;">${outPortsHtml}</div>
      </div>
    `;

    el.onmousedown = (e) => startFlowNodeDrag(e, node, el);
    el.onclick = (e) => {
      e.stopPropagation();
      state.selectedFlowNodeId = node.id;
      renderFlowEditor();
      renderFlowNodeInspector(node);
    };

    frag.appendChild(el);
  });
  container.appendChild(frag);
}

function handlePortClick(e, nodeId, port, isInput) {
  e.stopPropagation();
  const flow = getActiveFlow();
  if (!flow) return;

  if (!isInput) {
    state.wiringSource = { nodeId, port };
    document.getElementById('wiringStatus').textContent = `Câblage actif depuis ${nodeId} (${port}) : cliquez maintenant sur le bouton "⬅ IN" du nœud destination.`;
    renderFlowEditor();
  } else if (state.wiringSource) {
    if (state.wiringSource.nodeId !== nodeId) {
      flow.edges.push({
        id: 'e-' + Date.now(),
        from_node: state.wiringSource.nodeId,
        from_port: state.wiringSource.port,
        to_node: nodeId,
        to_port: 'in'
      });
    }
    state.wiringSource = null;
    document.getElementById('wiringStatus').textContent = 'Câble créé ! Cliquez sur "⚡ Déployer à Chaud" pour l\'activer.';
    renderFlowEditor();
  }
}

function startFlowNodeDrag(e, node, el) {
  if (e.target.classList.contains('port-pill')) return;
  e.preventDefault();
  const sx = e.clientX, sy = e.clientY;
  const ox = node.x, oy = node.y;
  const flow = getActiveFlow();
  let frame = 0;

  // Pendant le déplacement : seuls le nœud et les câbles sont mis à jour (au plus une fois par image)
  function onMove(ev) {
    node.x = Math.max(10, Math.round((ox + (ev.clientX - sx)) / 10) * 10);
    node.y = Math.max(10, Math.round((oy + (ev.clientY - sy)) / 10) * 10);
    el.style.left = `${node.x}px`;
    el.style.top = `${node.y}px`;
    if (!frame) {
      frame = requestAnimationFrame(() => {
        frame = 0;
        renderFlowEdges(flow);
      });
    }
  }
  function onUp() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    renderFlowEdges(flow);
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function animateFlowEdge(fromId, toId) {
  const path = document.getElementById(`edge-${cssSafe(fromId)}-${cssSafe(toId)}`);
  if (!path) return;
  path.classList.add('active-pulse');
  setTimeout(() => path.classList.remove('active-pulse'), 320);
}

function renderFlowNodeInspector(node) {
  const box = document.getElementById('inspectorContent');
  const tagOpts = (selected) => Object.values(state.tags).map(t =>
    `<option value="${esc(t.id)}" ${selected === t.id ? 'selected' : ''}>${esc(t.id)}</option>`
  ).join('');

  const k = node.kind;
  const nid = jsArg(node.id);
  let specificHtml = '';

  if (k.node_type === 'TagInput') {
    specificHtml = `
      <div class="form-group">
        <label>Variable Source (TagInput)</label>
        <select onchange="updateFlowNodeKind(${nid}, 'tag_id', this.value)">${tagOpts(k.tag_id)}</select>
      </div>
    `;
  } else if (k.node_type === 'TagOutput') {
    specificHtml = `
      <div class="form-group">
        <label>Variable Cible (TagOutput)</label>
        <select onchange="updateFlowNodeKind(${nid}, 'target_tag_id', this.value)">${tagOpts(k.target_tag_id)}</select>
      </div>
    `;
  } else if (k.node_type === 'PidController') {
    specificHtml = `
      <div class="form-group">
        <label>Tag de Consigne (Setpoint)</label>
        <select onchange="updateFlowNodeKind(${nid}, 'setpoint_tag_id', this.value)">${tagOpts(k.setpoint_tag_id)}</select>
      </div>
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:6px;">
        <div class="form-group"><label>Gain Kp</label><input type="number" step="0.5" value="${Number(k.kp)}" onchange="updateFlowNodeKind(${nid}, 'kp', parseFloat(this.value))"/></div>
        <div class="form-group"><label>Intégral Ki</label><input type="number" step="0.1" value="${Number(k.ki)}" onchange="updateFlowNodeKind(${nid}, 'ki', parseFloat(this.value))"/></div>
        <div class="form-group"><label>Dérivé Kd</label><input type="number" step="0.1" value="${Number(k.kd)}" onchange="updateFlowNodeKind(${nid}, 'kd', parseFloat(this.value))"/></div>
      </div>
      <div style="display:grid;grid-template-columns:1fr 1fr;gap:6px;">
        <div class="form-group"><label>Sortie Min</label><input type="number" value="${Number(k.min_out)}" onchange="updateFlowNodeKind(${nid}, 'min_out', parseFloat(this.value))"/></div>
        <div class="form-group"><label>Sortie Max</label><input type="number" value="${Number(k.max_out)}" onchange="updateFlowNodeKind(${nid}, 'max_out', parseFloat(this.value))"/></div>
      </div>
      <label style="font-size:12px;color:#94a3b8;display:flex;gap:6px;align-items:center;">
        <input type="checkbox" ${k.reverse_acting ? 'checked' : ''} onchange="updateFlowNodeKind(${nid}, 'reverse_acting', this.checked)" /> Action inverse (refroidissement)
      </label>
    `;
  } else if (k.node_type === 'Threshold') {
    specificHtml = `
      <div class="form-group">
        <label>Opérateur</label>
        <select onchange="updateFlowNodeKind(${nid}, 'operator', this.value)">
          ${['Lt', 'Lte', 'Gt', 'Gte', 'Eq', 'Neq'].map(op => `<option value="${op}" ${k.operator === op ? 'selected' : ''}>${op}</option>`).join('')}
        </select>
      </div>
      <div class="form-group"><label>Seuil</label><input type="number" step="0.5" value="${Number(k.threshold)}" onchange="updateFlowNodeKind(${nid}, 'threshold', parseFloat(this.value))" /></div>
      <div class="form-group"><label>Hystérésis</label><input type="number" step="0.1" value="${Number(k.hysteresis)}" onchange="updateFlowNodeKind(${nid}, 'hysteresis', parseFloat(this.value))" /></div>
    `;
  } else if (k.node_type === 'Hysteresis') {
    specificHtml = `
      <div class="form-group"><label>Seuil Bas (ON)</label><input type="number" step="0.5" value="${Number(k.low_threshold)}" onchange="updateFlowNodeKind(${nid}, 'low_threshold', parseFloat(this.value))"/></div>
      <div class="form-group"><label>Seuil Haut (OFF)</label><input type="number" step="0.5" value="${Number(k.high_threshold)}" onchange="updateFlowNodeKind(${nid}, 'high_threshold', parseFloat(this.value))"/></div>
      <label style="font-size:12px;color:#94a3b8;display:flex;gap:6px;align-items:center;">
        <input type="checkbox" ${k.invert ? 'checked' : ''} onchange="updateFlowNodeKind(${nid}, 'invert', this.checked)" /> Inverser la sortie
      </label>
    `;
  } else if (k.node_type === 'LogicGate') {
    specificHtml = `
      <div class="form-group">
        <label>Porte Logique</label>
        <select onchange="updateFlowNodeKind(${nid}, 'gate', this.value)">
          ${['And', 'Or', 'Xor', 'Not', 'Nand', 'Nor'].map(g => `<option value="${g}" ${k.gate === g ? 'selected' : ''}>${g}</option>`).join('')}
        </select>
      </div>
    `;
  } else if (k.node_type === 'DelayTimer') {
    specificHtml = `
      <div class="form-group"><label>Temporisation TON (ms)</label><input type="number" step="100" min="0" value="${Number(k.delay_ms)}" onchange="updateFlowNodeKind(${nid}, 'delay_ms', Math.max(0, parseInt(this.value, 10) || 0))"/></div>
    `;
  } else if (k.node_type === 'MathFormula') {
    specificHtml = `
      <div class="form-group"><label>Facteur (×)</label><input type="number" step="0.1" value="${Number(k.factor)}" onchange="updateFlowNodeKind(${nid}, 'factor', parseFloat(this.value))"/></div>
      <div class="form-group"><label>Offset (+)</label><input type="number" step="0.5" value="${Number(k.offset)}" onchange="updateFlowNodeKind(${nid}, 'offset', parseFloat(this.value))"/></div>
    `;
  } else if (k.node_type === 'RhaiScript') {
    specificHtml = `
      <div class="form-group">
        <label>Code Rhai (<code>val</code>, <code>tag("ID")</code>, <code>tag_bool("ID")</code>, <code>tag_text("ID")</code>, <code>clamp(x,min,max)</code>)</label>
        <textarea rows="7" oninput="updateFlowNodeKind(${nid}, 'script', this.value)">${esc(k.script)}</textarea>
      </div>
    `;
  }

  box.innerHTML = `
    <div class="form-group">
      <label>Libellé du Nœud</label>
      <input value="${esc(node.label)}" oninput="updateFlowNodeLabel(${nid}, this.value)" />
    </div>
    ${specificHtml}
    <button class="btn btn-primary" onclick="saveCurrentFlow()">⚡ Déployer à Chaud</button>
    <button class="btn btn-danger" onclick="deleteFlowNode(${nid})">🗑️ Supprimer Nœud</button>
  `;
}

function updateFlowNodeLabel(nodeId, val) {
  const flow = getActiveFlow();
  const n = flow && flow.nodes.find(x => x.id === nodeId);
  if (n) { n.label = val; renderFlowEditor(); }
}

function updateFlowNodeKind(nodeId, field, val) {
  const flow = getActiveFlow();
  const n = flow && flow.nodes.find(x => x.id === nodeId);
  if (n) { n.kind[field] = val; renderFlowEditor(); }
}

function deleteFlowNode(nodeId) {
  const flow = getActiveFlow();
  if (!flow) return;
  flow.nodes = flow.nodes.filter(n => n.id !== nodeId);
  flow.edges = flow.edges.filter(e => e.from_node !== nodeId && e.to_node !== nodeId);
  state.selectedFlowNodeId = null;
  renderFlowEditor();
}

async function saveCurrentFlow(silent) {
  const flow = getActiveFlow();
  if (!flow) return;
  const res = await fetch('/api/flows', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(flow)
  });
  if (!res.ok) {
    alert(`Déploiement refusé : ${await apiError(res)}`);
  } else if (!silent) {
    alert(`Flux "${flow.name}" déployé à chaud !`);
  }
}

// ============================================================================
// ATELIER 3 : CONSOLE D'ALARMES ISA-18.2 & JOURNAL SQLITE
// ============================================================================

/** Réglages de mise en sommeil (diffusés par le serveur dans le statut système). */
function shelveSettings() {
  const s = state.systemStatus && state.systemStatus.settings;
  return {
    alarm_shelving_enabled: s ? s.alarm_shelving_enabled !== false : true,
    alarm_shelve_max_minutes: s && s.alarm_shelve_max_minutes ? s.alarm_shelve_max_minutes : 480
  };
}

/** Mise en sommeil possible : fonction activée et non interdite pour la variable de l'alarme. */
function canShelve(a) {
  if (!shelveSettings().alarm_shelving_enabled) return false;
  const tag = state.tags[a.tag_id];
  return !(tag && tag.alarm_config && tag.alarm_config.shelving_disabled);
}

/** Formulaire des réglages système (carte Administrateur). */
function renderSettingsForm() {
  const chk = document.getElementById('setShelvingEnabled');
  const max = document.getElementById('setShelveMax');
  if (!chk || !max) return;
  const s = shelveSettings();
  chk.checked = s.alarm_shelving_enabled;
  if (document.activeElement !== max) max.value = s.alarm_shelve_max_minutes;
}

async function saveSystemSettings() {
  const enabled = document.getElementById('setShelvingEnabled').checked;
  const max = parseInt(document.getElementById('setShelveMax').value, 10);
  if (!enabled && !confirm('Désactiver la mise en sommeil ? Toutes les alarmes en sommeil seront réveillées immédiatement.')) {
    renderSettingsForm();
    return;
  }
  const res = await fetch('/api/system/settings', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ alarm_shelving_enabled: enabled, alarm_shelve_max_minutes: max })
  });
  if (!res.ok) {
    alert(`Réglages refusés : ${await apiError(res)}`);
    renderSettingsForm();
    return;
  }
  state.systemStatus = Object.assign({}, state.systemStatus || {}, { settings: await res.json() });
  renderSettingsForm();
  renderAlarmsTable();
}

/** Une alarme en sommeil reste suivie mais est exclue du bandeau et des compteurs. */
function isShelved(a) {
  return !!(a.shelved_until && new Date(a.shelved_until) > new Date());
}

function renderAlarmBanner() {
  const visible = state.alarms.filter(a => !isShelved(a));
  const shelvedCount = state.alarms.length - visible.length;
  const unack = visible.filter(a => a.state === 'ActiveUnack' || a.state === 'ClearedUnack');
  const active = visible.filter(a => a.state === 'ActiveUnack' || a.state === 'ActiveAck');
  document.getElementById('navAlarmCount').textContent = active.length + (shelvedCount ? ` · 💤${shelvedCount}` : '');

  const bar = document.getElementById('topAlarmBanner');
  const text = document.getElementById('topAlarmText');

  if (unack.length > 0) {
    bar.classList.add('has-unack');
    const latest = unack[0];
    text.innerHTML = `🚨 <b>${unack.length} ALARME(S) NON ACQUITTÉE(S)</b> — [${esc(latest.severity)}] <b>${esc(latest.tag_id)}</b> : ${esc(latest.message)}`;
  } else if (active.length > 0) {
    bar.classList.remove('has-unack');
    text.innerHTML = `⚠️ <b>${active.length} alarme(s) active(s) acquittée(s)</b> — Surveillance en cours`;
  } else {
    bar.classList.remove('has-unack');
    text.innerHTML = `✅ Aucune alarme active — Système nominal`;
  }
}

function renderAlarmsTable() {
  const tbody = document.getElementById('alarmsTableBody');
  if (!tbody) return;
  if (state.alarms.length === 0) {
    tbody.innerHTML = `<tr><td colspan="8" style="text-align:center;color:#64748b;padding:20px;">Aucune alarme active. Cliquez sur "Tester" sur le pressostat filtre du synoptique pour déclencher une alarme ISA-18.2.</td></tr>`;
    return;
  }
  const allowAck = state.currentUser && state.currentUser.permissions && state.currentUser.permissions.can_ack_alarms;

  tbody.innerHTML = state.alarms.map(a => {
    const shelved = isShelved(a);
    const needsAck = a.state === 'ActiveUnack' || a.state === 'ClearedUnack';
    const ackCell = needsAck
      ? (allowAck ? `<button class="btn btn-primary" onclick="ackSingleAlarm(${jsArg(a.id)})">Acquitter</button>` : '<span style="color:#f59e0b;">Non acquittée</span>')
      : '<span style="color:#10b981;">✔ Acquittée</span>';
    const shelveCell = !allowAck ? '' : shelved
      ? `<button class="btn" onclick="unshelveAlarm(${jsArg(a.id)})">⏰ Réveiller</button>`
      : (a.state !== 'Normal' && canShelve(a) ? `<button class="btn" onclick="shelveAlarm(${jsArg(a.id)})" title="Masquer temporairement une alarme connue (8 h maximum)">💤 Sommeil</button>` : '');
    return `
    <tr class="alarm-row-${esc(a.state)}" style="${shelved ? 'opacity:0.55;' : ''}">
      <td style="font-family:Consolas;">${esc(new Date(a.raised_at).toLocaleTimeString('fr-FR'))}</td>
      <td><b style="color:${a.severity === 'Critical' ? '#ef4444' : '#f59e0b'};">${esc(a.severity)}</b></td>
      <td>
        <span class="badge-qual">${esc(a.state)}</span>
        ${shelved ? `<div style="font-size:10.5px;color:#a78bfa;">💤 jusqu'à ${esc(new Date(a.shelved_until).toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' }))} (${esc(a.shelved_by || '')})</div>` : ''}
      </td>
      <td style="font-family:Consolas;font-weight:700;">${esc(a.tag_id)}</td>
      <td>${esc(a.message)}</td>
      <td style="font-family:Consolas;">${esc(JSON.stringify(a.trigger_value))}</td>
      <td>${esc(a.acked_by || '--')}${a.ack_comment ? `<div style="font-size:10.5px;color:#94a3b8;">${esc(a.ack_comment)}</div>` : ''}</td>
      <td style="display:flex;gap:6px;flex-wrap:wrap;">${ackCell}${shelveCell}</td>
    </tr>
  `;
  }).join('');
}

async function shelveAlarm(alarmId) {
  const max = shelveSettings().alarm_shelve_max_minutes;
  const raw = prompt(`Durée de mise en sommeil (minutes, 1 à ${max}) :`, String(Math.min(60, max)));
  if (raw === null) return;
  const minutes = parseInt(raw, 10);
  if (!(minutes >= 1 && minutes <= max)) {
    alert(`Durée invalide : saisissez un nombre de minutes entre 1 et ${max}.`);
    return;
  }
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/shelve`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ minutes, comment: ackComment() })
  });
  if (!res.ok) alert(await apiError(res));
}

async function unshelveAlarm(alarmId) {
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/unshelve`, { method: 'POST' });
  if (!res.ok) alert(await apiError(res));
}

function ackComment() {
  const el = document.getElementById('ackComment');
  return el && el.value.trim() ? el.value.trim() : null;
}

async function ackSingleAlarm(alarmId) {
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/ack`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ comment: ackComment() })
  });
  if (res.ok) {
    state.alarms = await getJson('/api/alarms', state.alarms);
    renderAlarmBanner();
    renderAlarmsTable();
    loadAlarmJournal();
  } else {
    alert(await apiError(res));
  }
}

async function ackAllAlarms() {
  const res = await fetch('/api/alarms/ack-all', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ comment: ackComment() })
  });
  if (res.ok) {
    state.alarms = await res.json();
    renderAlarmBanner();
    renderAlarmsTable();
    loadAlarmJournal();
  } else {
    alert(await apiError(res));
  }
}

async function clearNormalAlarms() {
  const res = await fetch('/api/alarms/clear-normal', { method: 'POST' });
  if (res.ok) {
    state.alarms = await res.json();
    renderAlarmBanner();
    renderAlarmsTable();
  } else {
    alert(await apiError(res));
  }
}

async function loadAlarmJournal() {
  const tbody = document.getElementById('alarmJournalTableBody');
  if (!tbody) return;
  const list = await getJson('/api/alarms/journal', []);
  if (!list.length) {
    tbody.innerHTML = '<tr><td colspan="6" style="text-align:center;color:#64748b;">Aucun événement archivé dans le journal SQLite.</td></tr>';
    return;
  }
  tbody.innerHTML = list.map(e => `
    <tr>
      <td style="font-family:Consolas;">${esc(new Date(e.recorded_at).toLocaleString('fr-FR'))}</td>
      <td><b>${esc(e.event_type)}</b></td>
      <td>${esc(e.severity)}</td>
      <td style="font-family:Consolas;">${esc(e.tag_id)}</td>
      <td>${esc(e.message)}</td>
      <td>${esc(e.operator || 'Système')}</td>
    </tr>
  `).join('');
}

// ============================================================================
// ATELIER 4 : DRIVERS (START / STOP / RESTART / DELETE) & TAG DATABASE CRUD
// ============================================================================

async function renderDriversAndTagDb() {
  state.drivers = await getJson('/api/drivers', state.drivers);
  renderDriversCardsOnly();
  renderTagCatalogTable();
  refreshDriverFormMode();
}

function renderDriversCardsOnly() {
  const box = document.getElementById('driversCardsContainer');
  if (!box) return;
  box.innerHTML = state.drivers.map(d => {
    const st = d.stats || {};
    const id = jsArg(d.id);
    const statusBadge = st.running
      ? (st.connected
          ? `<span class="badge-qual q-Good">🟢 EN LIGNE (${Number(st.last_latency_ms) || 1} ms)</span>`
          : `<span class="badge-qual q-BadDisconnect">🔴 DÉFAUT COM</span>`)
      : `<span class="badge-qual q-Uncertain">⏹️ ARRÊTÉ</span>`;

    return `
      <div style="display:flex;justify-content:space-between;align-items:center;background:var(--bg-card);border:1px solid var(--border);border-radius:6px;padding:12px 14px;margin-bottom:8px;gap:10px;flex-wrap:wrap;">
        <div>
          <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;">
            <b style="color:var(--accent);font-size:14px;">${esc(d.id)}</b>
            <span>${esc(d.name)}</span>
            ${statusBadge}
            ${SIMULATED_PROTOCOLS.includes(d.protocol)
              ? `<span class="badge-qual q-Uncertain" title="Aucune communication réelle : valeurs fictives (démonstration)">SIMULÉ</span>`
              : ''}
            <span style="font-size:11px;color:#64748b;">${(d.tags || []).length} variable(s)</span>
          </div>
          <div style="font-size:11.5px;color:#94a3b8;margin-top:4px;">
            Protocole : <b>${esc(d.protocol)}</b> | Endpoint : <code>${esc(d.endpoint)}</code> | Slave : ${Number(d.unit_id)} |
            Trames OK : <b style="color:#34d399;">${Number(st.frames_ok) || 0}</b> | Erreurs : <b style="color:#f87171;">${Number(st.frames_err) || 0}</b>
            ${st.last_error ? `| <span style="color:#f87171;">${esc(st.last_error)}</span>` : ''}
          </div>
        </div>
        <div style="display:flex;gap:6px;align-items:center;">
          ${st.running
            ? `<button class="btn" onclick="stopDriver(${id})">⏹️ Arrêter</button>`
            : `<button class="btn btn-primary" onclick="startDriver(${id})">▶️ Démarrer</button>`
          }
          <button class="btn" onclick="restartDriver(${id})">🔄 Redémarrer</button>
          <button class="btn" onclick="editDriverInForm(${id})">⚙️ Paramètres</button>
          <button class="btn" onclick="browseDriverTags(${id})">🔍 Auto-Découverte</button>
          <button class="btn btn-danger" onclick="deleteDriver(${id})">🗑️</button>
        </div>
      </div>
    `;
  }).join('');
}

async function driverAction(id, action) {
  const res = await fetch(`/api/drivers/${encodeURIComponent(id)}/${action}`, { method: 'POST' });
  if (!res.ok) alert(await apiError(res));
  await renderDriversAndTagDb();
}

async function startDriver(id) { await driverAction(id, 'start'); }
async function stopDriver(id) { await driverAction(id, 'stop'); }
async function restartDriver(id) { await driverAction(id, 'restart'); }

async function deleteDriver(id) {
  const drv = state.drivers.find(d => d.id === id);
  const n = drv && drv.tags ? drv.tags.length : 0;
  if (!confirm(`Supprimer le driver "${id}" et ses ${n} variable(s) ?\nLes widgets liés à ces variables afficheront « Non lié ».`)) return;
  const res = await fetch(`/api/drivers/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  Object.keys(state.tags).forEach(tid => {
    if (state.tags[tid].module_source === id) delete state.tags[tid];
  });
  renderTagTree();
  await renderDriversAndTagDb();
}

function renderTagCatalogTable() {
  const tbody = document.getElementById('tagCatalogTableBody');
  if (!tbody) return;
  const all = Object.values(state.tags).sort((a, b) => a.id.localeCompare(b.id));
  const allowEdit = state.currentUser && state.currentUser.permissions && state.currentUser.permissions.can_manage_drivers_tags;
  tbody.innerHTML = all.map(t => {
    const qual = t.state ? t.state.quality : 'Uncertain';
    const low = t.alarm_config && t.alarm_config.low !== null && t.alarm_config.low !== undefined ? t.alarm_config.low : '-';
    const high = t.alarm_config && t.alarm_config.high !== null && t.alarm_config.high !== undefined ? t.alarm_config.high : '-';
    const sid = cssSafe(t.id);
    return `
      <tr id="cat-row-${sid}">
        <td style="font-family:Consolas;font-weight:700;color:#fff;">${esc(t.id)}</td>
        <td>${esc(t.label)}</td>
        <td>${esc(t.module_source)}</td>
        <td style="font-family:Consolas;">${esc(t.protocol_address)}</td>
        <td>${esc(t.data_type)} (${t.writable ? 'RW' : 'RO'})</td>
        <td id="cat-val-${sid}" style="font-family:Consolas;color:var(--accent);font-weight:700;">${formatTagValue(t)}</td>
        <td id="cat-q-${sid}"><span class="badge-qual q-${esc(qual)}">${esc(qual)}</span></td>
        <td style="font-family:Consolas;">L: ${esc(low)} / H: ${esc(high)}</td>
        <td>
          ${allowEdit ? `<button class="btn" onclick="editTagInForm(${jsArg(t.id)})">Éditer</button>
          <button class="btn btn-danger" onclick="deleteTagById(${jsArg(t.id)})">Suppr.</button>` : ''}
        </td>
      </tr>
    `;
  }).join('');
}

/** Met à jour uniquement la valeur et la qualité d'une ligne du catalogue (pas de rendu complet). */
function updateCatalogRowInPlace(tagId) {
  const t = state.tags[tagId];
  if (!t) return;
  const sid = cssSafe(tagId);
  const valCell = document.getElementById(`cat-val-${sid}`);
  if (!valCell) return;
  valCell.innerHTML = formatTagValue(t);
  const qual = t.state ? t.state.quality : 'Uncertain';
  const qCell = document.getElementById(`cat-q-${sid}`);
  if (qCell) qCell.innerHTML = `<span class="badge-qual q-${esc(qual)}">${esc(qual)}</span>`;
}

/** Le formulaire passe en mode « mise à jour » quand l'ID saisi est celui d'un driver existant. */
function refreshDriverFormMode() {
  const id = document.getElementById('drvId').value.trim();
  const existing = state.drivers.find(d => d.id === id);
  document.getElementById('btnDriverSubmit').textContent = existing ? '💾 Mettre à jour' : '+ Déclarer & Démarrer';
  document.getElementById('drvProto').disabled = !!existing;
}

function editDriverInForm(id) {
  const d = state.drivers.find(x => x.id === id);
  if (!d) return;
  document.getElementById('drvId').value = d.id;
  document.getElementById('drvProto').value = d.protocol;
  document.getElementById('drvEndpoint').value = d.endpoint;
  document.getElementById('drvUnitId').value = d.unit_id;
  document.getElementById('drvPoll').value = d.poll_interval_ms;
  refreshDriverFormMode();
  document.getElementById('drvEndpoint').focus();
}

async function createNewDriver() {
  const id = document.getElementById('drvId').value.trim();
  const protocol = document.getElementById('drvProto').value;
  const endpoint = document.getElementById('drvEndpoint').value.trim();
  const unit_id = parseInt(document.getElementById('drvUnitId').value || '1', 10);
  const poll_interval_ms = parseInt(document.getElementById('drvPoll').value || '1000', 10);

  if (state.drivers.find(d => d.id === id)) {
    // Driver existant : mise à jour des paramètres de communication (redémarrage à chaud si actif)
    const res = await fetch(`/api/drivers/${encodeURIComponent(id)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ endpoint, unit_id, poll_interval_ms })
    });
    if (!res.ok) alert(`Mise à jour refusée : ${await apiError(res)}`);
    await renderDriversAndTagDb();
    refreshDriverFormMode();
    return;
  }

  const payload = {
    id,
    name: `Équipement ${protocol} (${id})`,
    protocol,
    endpoint,
    unit_id,
    poll_interval_ms,
    enabled: true,
    tags: []
  };

  const res = await fetch('/api/drivers', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });

  if (res.ok) {
    await browseDriverTags(id);
  } else {
    alert(await apiError(res));
  }
  await renderDriversAndTagDb();
}

async function browseDriverTags(driverId) {
  const res = await fetch(`/api/drivers/${encodeURIComponent(driverId)}/browse`, { method: 'POST' });
  if (res.ok) {
    const discovered = await res.json();
    discovered.forEach(t => { state.tags[t.id] = t; });
    renderTagTree();
    renderTagCatalogTable();
    populateHistorianSelect();
    populateScheduleTagSelect();
  } else {
    alert(await apiError(res));
  }
}

async function saveTagFromForm() {
  const id = document.getElementById('tagFormId').value.trim();
  const label = document.getElementById('tagFormLabel').value.trim();
  const module_source = document.getElementById('tagFormSource').value.trim();
  const protocol_address = document.getElementById('tagFormAddr').value.trim();
  const data_type = document.getElementById('tagFormType').value;
  const unit = document.getElementById('tagFormUnit').value.trim() || null;
  const writable = document.getElementById('tagFormRw').value === 'true';
  const scale_factor = parseFloat(document.getElementById('tagFormScale').value || '1.0');
  const lowRaw = document.getElementById('tagFormLow').value;
  const highRaw = document.getElementById('tagFormHigh').value;

  const low = lowRaw !== '' ? parseFloat(lowRaw) : null;
  const high = highRaw !== '' ? parseFloat(highRaw) : null;

  // On conserve les paramètres avancés existants (deadband, seuils LL/HH, message...)
  const existing = state.tags[id];
  const prevAlarm = (existing && existing.alarm_config) || {};

  const tagPayload = {
    id,
    label,
    module_source,
    protocol_address,
    data_type,
    unit,
    writable,
    deadband: existing ? existing.deadband : 0.05,
    scale_factor,
    scale_offset: existing ? existing.scale_offset : 0.0,
    historize: existing ? existing.historize : true,
    alarm_config: Object.assign({}, prevAlarm, {
      enabled: (low !== null || high !== null || prevAlarm.low_low != null || prevAlarm.high_high != null || prevAlarm.trigger_on_bool != null),
      low,
      high,
      severity: prevAlarm.severity || 'High',
      shelving_disabled: document.getElementById('tagFormShelve').value === 'true'
    })
  };

  const res = await fetch('/api/tags', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(tagPayload)
  });

  if (res.ok) {
    const saved = await res.json();
    state.tags[saved.id] = saved;
    renderTagTree();
    renderTagCatalogTable();
    populateHistorianSelect();
    populateScheduleTagSelect();
  } else {
    alert(`Enregistrement refusé : ${await apiError(res)}`);
  }
}

function editTagInForm(tagId) {
  const t = state.tags[tagId];
  if (!t) return;
  document.getElementById('tagFormId').value = t.id;
  document.getElementById('tagFormLabel').value = t.label;
  document.getElementById('tagFormSource').value = t.module_source;
  document.getElementById('tagFormAddr').value = t.protocol_address;
  document.getElementById('tagFormType').value = t.data_type;
  document.getElementById('tagFormUnit').value = t.unit || '';
  document.getElementById('tagFormRw').value = String(t.writable);
  document.getElementById('tagFormScale').value = t.scale_factor || 1.0;
  document.getElementById('tagFormLow').value = (t.alarm_config && t.alarm_config.low) ?? '';
  document.getElementById('tagFormHigh').value = (t.alarm_config && t.alarm_config.high) ?? '';
  document.getElementById('tagFormShelve').value = String(!!(t.alarm_config && t.alarm_config.shelving_disabled));
}

async function deleteTagById(tagId) {
  if (!confirm(`Supprimer la variable "${tagId}" ?`)) return;
  const res = await fetch(`/api/tags/${encodeURIComponent(tagId)}`, { method: 'DELETE' });
  if (res.ok) {
    delete state.tags[tagId];
    renderTagTree();
    renderTagCatalogTable();
  } else {
    alert(await apiError(res));
  }
}

// ============================================================================
// ATELIER 5 : PLANIFICATEUR HORAIRE GTB HEBDOMADAIRE
// ============================================================================

function renderScheduleDaysPicker() {
  const box = document.getElementById('schedDaysBox');
  if (!box) return;
  box.innerHTML = WEEK_DAYS.map(([n, label]) => `
    <label class="day-chip"><input type="checkbox" value="${n}" ${n <= 5 ? 'checked' : ''} /> ${label}</label>
  `).join('');
}

function formatDays(days) {
  const list = Array.isArray(days) ? days : [1, 2, 3, 4, 5, 6, 7];
  if (list.length === 7) return 'Tous les jours';
  if (list.length === 5 && [1, 2, 3, 4, 5].every(d => list.includes(d))) return 'Lun → Ven';
  if (!list.length) return 'Aucun jour';
  return WEEK_DAYS.filter(([n]) => list.includes(n)).map(([, l]) => l).join(', ');
}

function populateScheduleTagSelect() {
  const sel = document.getElementById('schedTagSelect');
  if (!sel) return;
  const writableTags = Object.values(state.tags).filter(t => t.writable);
  sel.innerHTML = writableTags.map(t => `<option value="${esc(t.id)}">${esc(t.id)} (${esc(t.label)})</option>`).join('');
}

function renderSchedulesTable() {
  const tbody = document.getElementById('schedulesTableBody');
  if (!tbody) return;
  tbody.innerHTML = state.schedules.map(s => `
    <tr>
      <td><b>${esc(s.name)}</b></td>
      <td style="font-family:Consolas;color:var(--accent);">${esc(s.target_tag_id)}</td>
      <td>${esc(formatDays(s.days))}</td>
      <td style="font-family:Consolas;">${esc(s.start_time)} ➔ ${esc(s.end_time)}</td>
      <td style="font-family:Consolas;color:#34d399;font-weight:700;">${esc(JSON.stringify(s.active_value))}</td>
      <td style="font-family:Consolas;color:#94a3b8;">${esc(JSON.stringify(s.inactive_value))}</td>
      <td>${s.enabled ? '<span class="badge-qual q-Good">ACTIF</span>' : '<span class="badge-qual q-Uncertain">INACTIF</span>'}</td>
      <td>
        <button class="btn" onclick="sendWriteTag(${jsArg(s.target_tag_id)}, ${jsArg(s.active_value)})">Forcer Confort</button>
        <button class="btn" onclick="sendWriteTag(${jsArg(s.target_tag_id)}, ${jsArg(s.inactive_value)})">Forcer Réduit</button>
        <button class="btn" onclick="toggleScheduleEnabled(${jsArg(s.id)})">${s.enabled ? '⏸️ Désactiver' : '▶️ Activer'}</button>
        <button class="btn btn-danger" onclick="deleteScheduleRule(${jsArg(s.id)})">Suppr.</button>
      </td>
    </tr>
  `).join('');
}

async function postSchedule(payload) {
  const res = await fetch('/api/schedules', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });
  if (!res.ok) {
    alert(`Planning refusé : ${await apiError(res)}`);
    return null;
  }
  const saved = await res.json();
  state.schedules = state.schedules.filter(s => s.id !== saved.id).concat(saved).sort((a, b) => a.id.localeCompare(b.id));
  renderSchedulesTable();
  return saved;
}

async function createScheduleRule() {
  const days = [...document.querySelectorAll('#schedDaysBox input:checked')].map(i => parseInt(i.value, 10));
  if (!days.length) {
    alert('Sélectionnez au moins un jour de la semaine.');
    return;
  }
  await postSchedule({
    id: 'sched-' + Date.now(),
    name: document.getElementById('schedName').value.trim(),
    target_tag_id: document.getElementById('schedTagSelect').value,
    start_time: document.getElementById('schedStart').value,
    end_time: document.getElementById('schedEnd').value,
    active_value: parseFloat(document.getElementById('schedActiveVal').value),
    inactive_value: parseFloat(document.getElementById('schedInactiveVal').value),
    enabled: true,
    days
  });
}

async function toggleScheduleEnabled(id) {
  const rule = state.schedules.find(s => s.id === id);
  if (!rule) return;
  await postSchedule(Object.assign({}, rule, { enabled: !rule.enabled }));
}

async function deleteScheduleRule(id) {
  const res = await fetch(`/api/schedules/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  state.schedules = state.schedules.filter(s => s.id !== id);
  renderSchedulesTable();
}

// ============================================================================
// ATELIER 6 : HISTORIAN SQLITE, SYSTÈME, BACKUP JSON & AUDIT TRAIL
// ============================================================================

function populateHistorianSelect() {
  const sel = document.getElementById('histTagSelect');
  if (!sel) return;
  const current = sel.value;
  sel.innerHTML = Object.values(state.tags).map(t =>
    `<option value="${esc(t.id)}" ${t.id === current ? 'selected' : ''}>${esc(t.id)} — ${esc(t.label)}</option>`
  ).join('');
}

async function loadSystemAndHistorianTab() {
  const st = await getJson('/api/system/status', null);
  if (st) {
    state.systemStatus = st;
    renderSettingsForm();
    document.getElementById('sysKpiUptime').textContent = `PID ${st.pid} (${st.uptime_seconds}s)`;
    document.getElementById('sysKpiDrivers').textContent = `${st.running_drivers} / ${st.total_drivers}`;
    document.getElementById('sysKpiCov').textContent = st.cov_events_total;
    document.getElementById('sysKpiFlows').textContent = st.active_flows;
    const ret = document.getElementById('histRetentionInfo');
    if (ret) {
      ret.textContent = st.history_retention_days > 0
        ? `Rétention automatique : ${st.history_retention_days} jours${st.history_dropped ? ` · ${st.history_dropped} point(s) écarté(s) (file saturée)` : ''}`
        : 'Rétention automatique désactivée';
    }
  }

  await loadHistorianData();
  await loadAuditLogs();
}

/** Construit les paramètres de période (champs datetime-local → ISO 8601 UTC). */
function historianQuery() {
  const params = new URLSearchParams();
  params.set('limit', document.getElementById('histLimitSelect').value || 300);
  const from = document.getElementById('histFrom').value;
  const to = document.getElementById('histTo').value;
  if (from) params.set('from', new Date(from).toISOString());
  if (to) params.set('to', new Date(to).toISOString());
  return params;
}

function clearHistorianRange() {
  document.getElementById('histFrom').value = '';
  document.getElementById('histTo').value = '';
  loadHistorianData();
}

async function loadHistorianData() {
  const tagId = document.getElementById('histTagSelect').value;
  if (!tagId) return;

  const res = await fetch(`/api/history/${encodeURIComponent(tagId)}?${historianQuery()}`);
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  const data = await res.json();

  const unit = data.unit ? ` ${data.unit}` : '';
  document.getElementById('histCount').textContent = data.sampled
    ? `${data.count} / ${data.total_points}`
    : data.count;
  document.getElementById('histCount').title = data.sampled
    ? 'Échantillon régulier sur toute la période ; min, max et moyenne sont calculés sur tous les points.'
    : '';
  document.getElementById('histMin').textContent = data.min !== null ? `${data.min.toFixed(2)}${unit}` : '--';
  document.getElementById('histMax').textContent = data.max !== null ? `${data.max.toFixed(2)}${unit}` : '--';
  document.getElementById('histAvg').textContent = data.avg !== null
    ? `${data.avg.toFixed(2)}${unit} (±${(data.std_dev || 0).toFixed(2)})`
    : '--';

  const chartBox = document.getElementById('historianChartBox');
  const nums = data.points.map(p => typeof p.value === 'number' ? p.value : (p.value ? 1 : 0));
  if (nums.length > 1) {
    const min = Math.min(...nums) - 0.5;
    const max = Math.max(...nums) + 0.5;
    const w = 1150, h = 210;
    const poly = nums.map((v, i) => {
      const x = (i / (nums.length - 1)) * (w - 20) + 10;
      const y = h - 15 - ((v - min) / ((max - min) || 1)) * (h - 30);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    }).join(' ');
    chartBox.innerHTML = `
      <svg width="100%" height="${h}" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">
        <polyline fill="none" stroke="#38bdf8" stroke-width="2.5" points="${poly}" />
      </svg>
    `;
  } else {
    chartBox.innerHTML = '<p style="color:#64748b;padding:20px;">Aucun échantillon sur cette période.</p>';
  }

  const tbody = document.getElementById('historianTableBody');
  tbody.innerHTML = data.points.slice().reverse().slice(0, 40).map(p => `
    <tr>
      <td style="font-family:Consolas;">${esc(new Date(p.timestamp).toLocaleString('fr-FR'))}</td>
      <td style="font-family:Consolas;font-weight:700;">${esc(data.tag_id)}</td>
      <td style="font-family:Consolas;color:var(--accent);font-weight:700;">${esc(JSON.stringify(p.value) + unit)}</td>
      <td><span class="badge-qual q-${esc(p.quality)}">${esc(p.quality)}</span></td>
    </tr>
  `).join('');
}

/** Télécharge un contenu via un lien temporaire (l'API exige l'en-tête d'authentification). */
async function downloadAuthenticated(url, fallbackName) {
  const res = await fetch(url);
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  const blob = await res.blob();
  const disposition = res.headers.get('content-disposition') || '';
  const match = disposition.match(/filename="([^"]+)"/);
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = match ? match[1] : fallbackName;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
}

function downloadHistorianCsv() {
  const tagId = document.getElementById('histTagSelect').value;
  if (!tagId) return;
  const params = historianQuery();
  params.set('limit', 5000);
  downloadAuthenticated(`/api/history/${encodeURIComponent(tagId)}/csv?${params}`, 'historian_export.csv');
}

async function purgeHistorianDb() {
  if (!confirm('Purger toutes les mesures historiques de la base SQLite ?')) return;
  const res = await fetch('/api/history/purge', { method: 'POST' });
  if (!res.ok) alert(await apiError(res));
  await loadHistorianData();
  await loadAuditLogs();
}

async function loadAuditLogs() {
  const tbody = document.getElementById('auditTableBody');
  if (!tbody) return;
  state.auditLogs = await getJson('/api/audit?limit=500', []);
  renderAuditTable();
}

/** Filtre le journal d'audit (acteur, catégorie, action, cible ou détails). */
function renderAuditTable() {
  const tbody = document.getElementById('auditTableBody');
  if (!tbody) return;
  const q = (document.getElementById('auditFilter') ? document.getElementById('auditFilter').value : '').trim().toLowerCase();
  const logs = (state.auditLogs || []).filter(l => !q
    || [l.actor, l.category, l.action, l.target, l.details].some(v => String(v || '').toLowerCase().includes(q)));
  const counter = document.getElementById('auditCount');
  if (counter) counter.textContent = `${logs.length} / ${(state.auditLogs || []).length}`;
  tbody.innerHTML = logs.slice(0, 200).map(l => `
    <tr>
      <td style="font-family:Consolas;">${esc(new Date(l.timestamp).toLocaleString('fr-FR'))}</td>
      <td><b>${esc(l.actor)}</b></td>
      <td><span class="badge-rw">${esc(l.category)}</span></td>
      <td style="font-family:Consolas;">${esc(l.action)}</td>
      <td style="font-family:Consolas;color:var(--accent);">${esc(l.target)}</td>
      <td>${esc(l.details)}</td>
    </tr>
  `).join('');
}

async function exportProjectBackup() {
  const res = await fetch('/api/system/export');
  if (!res.ok) {
    alert(await apiError(res));
    return;
  }
  const bundle = await res.json();
  const blob = new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = `rust_scada_flow_backup_${Date.now()}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
}

async function importProjectBackup(event) {
  const file = event.target.files[0];
  event.target.value = '';
  if (!file) return;
  let bundle;
  try {
    bundle = JSON.parse(await file.text());
  } catch (_) {
    alert('Fichier JSON invalide.');
    return;
  }
  const res = await fetch('/api/system/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(bundle)
  });
  if (!res.ok) {
    alert(`Import refusé : ${await apiError(res)}`);
    return;
  }
  const r = await res.json();
  const errs = (r.errors || []);
  alert(`Archive restaurée : ${r.drivers} driver(s), ${r.tags} tag(s), ${r.dashboards} synoptique(s), ${r.flows} flux, ${r.schedules} planning(s).`
    + (errs.length ? `\n\n${errs.length} avertissement(s) :\n- ${errs.slice(0, 10).join('\n- ')}` : ''));
  await bootstrapData();
}

// ============================================================================
// COMMANDES COMMUNES & NAVIGATION
// ============================================================================

async function sendWriteTag(tagId, value) {
  if (!tagId) return;
  if (!canWrite()) {
    alert('Votre profil ne permet pas l\'envoi de consignes.');
    return;
  }
  const res = await fetch(`/api/tags/${encodeURIComponent(tagId)}/write`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ value })
  });
  if (!res.ok) {
    alert(`Refus d'écriture : ${await apiError(res)}`);
  }
}

function switchTab(tab, btn) {
  // Protection RBAC côté IHM (le serveur applique de toute façon les mêmes règles)
  if (state.currentUser && state.currentUser.role === 'Viewer' && tab !== 'synoptic') {
    return;
  }
  if (state.currentUser && state.currentUser.role !== 'Admin' && tab === 'users') {
    return;
  }

  state.activeTab = tab;
  document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
  if (btn) btn.classList.add('active');

  ['synoptic', 'flow', 'alarms', 'tagdb', 'schedules', 'historian', 'users'].forEach(t => {
    const el = document.getElementById(`tab-${t}`);
    if (el) el.style.display = (t === tab) ? 'block' : 'none';
  });

  if (tab === 'synoptic') renderDashboard();
  if (tab === 'flow') renderFlowEditor();
  if (tab === 'tagdb') renderDriversAndTagDb();
  if (tab === 'schedules') renderSchedulesTable();
  if (tab === 'historian') loadSystemAndHistorianTab();
  if (tab === 'users') loadUsersList();
  if (tab === 'alarms') {
    renderAlarmsTable();
    loadAlarmJournal();
  }
}

function setMode(mode) {
  if (!state.currentUser || !state.currentUser.permissions || !state.currentUser.permissions.can_edit_studio) {
    mode = 'run';
  }
  state.mode = mode;
  document.body.classList.toggle('run-mode', mode === 'run');
  document.getElementById('btnModeStudio').className = 'mode-btn' + (mode === 'studio' ? ' active-studio' : '');
  document.getElementById('btnModeRun').className = 'mode-btn' + (mode === 'run' ? ' active-run' : '');
  document.getElementById('studioHint').style.display = mode === 'studio' ? 'inline' : 'none';
}

function cssSafe(str) {
  return String(str).replace(/[^a-zA-Z0-9_-]/g, '_');
}

// ============================================================================
// ATELIER 7 : CONNEXION, COMPTE UTILISATEUR & INTERFACE ADAPTATIVE RBAC
// ============================================================================

function applyUserPermissionsToUi() {
  const user = state.currentUser;
  if (!user) return;

  const role = user.role; // 'Admin' | 'Supervisor' | 'Viewer'
  const perms = user.permissions || {};

  const iconEl = document.getElementById('userRoleIcon');
  const nameEl = document.getElementById('userDisplayName');
  const badgeEl = document.getElementById('userRoleBadge');
  const hintEl = document.getElementById('rbacHintPill');

  nameEl.textContent = user.username;

  if (role === 'Admin') {
    iconEl.textContent = '👑';
    badgeEl.textContent = 'ADMIN';
    badgeEl.style.background = 'rgba(239,68,68,0.22)';
    badgeEl.style.color = '#f87171';
    if (hintEl) {
      hintEl.textContent = '👑 Administrateur (Tous les droits)';
      hintEl.style.color = '#f87171';
    }
  } else if (role === 'Supervisor') {
    iconEl.textContent = '🛠️';
    badgeEl.textContent = 'SUPERVISEUR';
    badgeEl.style.background = 'rgba(56,189,248,0.22)';
    badgeEl.style.color = '#38bdf8';
    if (hintEl) {
      hintEl.textContent = '🛠️ Superviseur (Tout sauf Paramètres Système)';
      hintEl.style.color = '#38bdf8';
    }
  } else {
    iconEl.textContent = '👁️';
    badgeEl.textContent = 'VISUALISATION';
    badgeEl.style.background = 'rgba(16,185,129,0.22)';
    badgeEl.style.color = '#34d399';
    if (hintEl) {
      hintEl.textContent = '👁️ Visualisation (Dashboards en Lecture Seule)';
      hintEl.style.color = '#34d399';
    }
  }

  const showEl = (id, visible, displayType = 'inline-block') => {
    const el = document.getElementById(id);
    if (el) el.style.display = visible ? displayType : 'none';
  };

  showEl('navBtn-flow', perms.can_manage_flows);
  showEl('navBtn-alarms', perms.can_ack_alarms);
  showEl('navBtn-tagdb', perms.can_manage_drivers_tags);
  showEl('navBtn-schedules', perms.can_manage_schedules);
  showEl('navBtn-historian', perms.can_view_historian);
  showEl('navBtn-users', perms.can_manage_users);

  // Contrôles d'alimentation & Paramètres Système (Admin uniquement)
  showEl('adminPowerControls', perms.can_manage_system, 'flex');
  showEl('systemAdminCard', perms.can_manage_system, 'block');
  showEl('btnPurgeHistorian', perms.can_manage_system, 'inline-block');

  // Contrôles Studio / Édition (Admin & Superviseur)
  showEl('modeSwitchContainer', perms.can_edit_studio, 'flex');
  showEl('synopticStudioActions', perms.can_edit_studio, 'flex');
  showEl('synopticSaveGroup', perms.can_edit_studio, 'flex');
  showEl('btnBannerAckAll', perms.can_ack_alarms, 'inline-block');

  document.body.classList.toggle('viewer-only-mode', role === 'Viewer');
  if (!perms.can_edit_studio) setMode('run');
  else setMode(state.mode);

  // Repli sur l'onglet Synoptiques si l'onglet courant n'est plus autorisé
  const tabAllowed = {
    synoptic: true,
    flow: perms.can_manage_flows,
    alarms: perms.can_ack_alarms,
    tagdb: perms.can_manage_drivers_tags,
    schedules: perms.can_manage_schedules,
    historian: perms.can_view_historian,
    users: perms.can_manage_users
  };
  if (!tabAllowed[state.activeTab]) {
    switchTab('synoptic', document.getElementById('navBtn-synoptic'));
  }
}

/** Affiche la fenêtre de connexion. Sans session valide, elle ne peut pas être fermée. */
function showLoginModal(message) {
  const modal = document.getElementById('authModal');
  const authenticated = !!state.currentUser;
  modal.style.display = 'flex';
  modal.classList.toggle('locked', !authenticated);
  document.getElementById('authModalClose').style.display = authenticated ? 'inline-block' : 'none';
  document.getElementById('accountSection').style.display = authenticated ? 'block' : 'none';
  document.getElementById('authModalMsg').textContent = message
    || (authenticated ? 'Changez de compte ou gérez votre mot de passe.' : 'Identifiez-vous pour accéder à la supervision.');
  if (authenticated) {
    document.getElementById('accountUserLabel').textContent = `${state.currentUser.display_name} (${state.currentUser.username} — ${state.currentUser.role_label})`;
  }
  setTimeout(() => document.getElementById('loginUsername').focus(), 30);
}

function openAuthModal() {
  showLoginModal();
}

function closeAuthModal() {
  if (!state.currentUser) return;
  document.getElementById('authModal').style.display = 'none';
}

function handleSessionExpired() {
  if (!state.authToken && !state.currentUser) return;
  state.authToken = '';
  state.currentUser = null;
  storeToken('');
  if (state.ws) {
    state.ws.onclose = null;
    try { state.ws.close(); } catch (_) {}
    state.ws = null;
  }
  clearTimeout(state.wsRetryTimer);
  showLoginModal('Votre session a expiré ou a été fermée : veuillez vous reconnecter.');
}

async function submitManualLogin() {
  const username = document.getElementById('loginUsername').value.trim();
  const password = document.getElementById('loginPassword').value;
  if (!username || !password) return;
  const res = await fetch('/api/auth/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password })
  });
  if (!res.ok) {
    document.getElementById('authModalMsg').textContent = `Échec d'authentification : ${await apiError(res)}`;
    return;
  }
  const data = await res.json();
  document.getElementById('loginPassword').value = '';
  state.authToken = data.token;
  storeToken(data.token);
  state.currentUser = data.user;
  document.getElementById('authModal').style.display = 'none';
  await startSession();
  if (state.activeTab === 'users' && isAdmin()) {
    await loadUsersList();
  }
}

async function logoutCurrentUser() {
  try { await fetch('/api/auth/logout', { method: 'POST' }); } catch (_) {}
  state.currentUser = null;
  state.authToken = '';
  storeToken('');
  if (state.ws) {
    state.ws.onclose = null;
    try { state.ws.close(); } catch (_) {}
    state.ws = null;
  }
  showLoginModal('Vous êtes déconnecté.');
}

async function changeOwnPassword() {
  const current_password = document.getElementById('pwdCurrent').value;
  const new_password = document.getElementById('pwdNew').value;
  const confirmPwd = document.getElementById('pwdNew2').value;
  if (new_password !== confirmPwd) {
    alert('Les deux nouveaux mots de passe ne correspondent pas.');
    return;
  }
  const res = await fetch('/api/auth/password', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ current_password, new_password })
  });
  if (res.ok) {
    ['pwdCurrent', 'pwdNew', 'pwdNew2'].forEach(id => { document.getElementById(id).value = ''; });
    alert('Mot de passe modifié. Vos autres sessions ouvertes ont été fermées.');
  } else {
    alert(`Changement refusé : ${await apiError(res)}`);
  }
}

async function loadUsersList() {
  if (!isAdmin()) return;
  const res = await fetch('/api/users');
  if (!res.ok) return;
  state.usersList = await res.json();
  const tbody = document.getElementById('usersTableBody');
  if (!tbody) return;

  tbody.innerHTML = state.usersList.map(u => {
    const badgeColor = u.role === 'Admin'
      ? 'background:rgba(239,68,68,0.2);color:#f87171;'
      : u.role === 'Supervisor'
        ? 'background:rgba(56,189,248,0.2);color:#38bdf8;'
        : 'background:rgba(16,185,129,0.2);color:#34d399;';
    const canDelete = u.username !== 'admin' && u.username !== state.currentUser.username;
    return `
      <tr>
        <td style="font-family:Consolas;font-weight:700;color:var(--accent);">${esc(u.username)}</td>
        <td><b>${esc(u.display_name)}</b></td>
        <td><span class="badge-rw" style="${badgeColor}">${esc(u.role_label)} (${esc(u.role)})</span></td>
        <td>${esc(u.department || '--')}</td>
        <td><span class="badge-qual ${u.enabled ? 'q-Good' : 'q-BadDisconnect'}">${u.enabled ? 'ACTIF' : 'DÉSACTIVÉ'}</span></td>
        <td style="font-family:Consolas;font-size:11px;">${u.last_login_at ? esc(new Date(u.last_login_at).toLocaleString('fr-FR')) : 'Jamais'}</td>
        <td style="display:flex;gap:6px;">
          <button class="btn" onclick="fillUserForm(${jsArg(u.username)})">✏️ Éditer</button>
          ${canDelete ? `<button class="btn btn-danger" onclick="deleteUserAccount(${jsArg(u.username)})">🗑️</button>` : ''}
        </td>
      </tr>
    `;
  }).join('');
}

function fillUserForm(username) {
  const u = state.usersList.find(x => x.username === username);
  if (!u) return;
  document.getElementById('usrFormLogin').value = u.username;
  document.getElementById('usrFormName').value = u.display_name;
  document.getElementById('usrFormRole').value = u.role;
  document.getElementById('usrFormDept').value = u.department || '';
  document.getElementById('usrFormPass').value = '';
  document.getElementById('usrFormEnabled').value = String(u.enabled);
}

async function saveUserAccount() {
  const payload = {
    username: document.getElementById('usrFormLogin').value.trim(),
    display_name: document.getElementById('usrFormName').value.trim(),
    role: document.getElementById('usrFormRole').value,
    department: document.getElementById('usrFormDept').value.trim(),
    password: document.getElementById('usrFormPass').value || null,
    enabled: document.getElementById('usrFormEnabled').value === 'true'
  };
  const res = await fetch('/api/users', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });
  if (res.ok) {
    document.getElementById('usrFormPass').value = '';
    await loadUsersList();
    alert(`Compte utilisateur "${payload.username}" enregistré avec le groupe "${payload.role}".`);
  } else {
    alert(`Erreur : ${await apiError(res)}`);
  }
}

async function deleteUserAccount(username) {
  if (!confirm(`Supprimer le compte utilisateur "${username}" ?`)) return;
  const res = await fetch(`/api/users/${encodeURIComponent(username)}`, { method: 'DELETE' });
  if (res.ok) {
    await loadUsersList();
  } else {
    alert(await apiError(res));
  }
}

// ============================================================================
// BIBLIOTHÈQUES GÉNÉRIQUES DE SYMBOLES INDUSTRIELS (SVG / P&ID / DRAW.IO / JSON)
// ============================================================================

function renderCustomSymbolsPalette() {
  const container = document.getElementById('customSymbolsPalette');
  const countEl = document.getElementById('customSymCount');
  if (!container) return;
  if (countEl) countEl.textContent = `${state.symbols.length} symboles`;

  container.innerHTML = state.symbols.map(sym => `
    <div class="palette-item" draggable="true"
         ondragstart="onCustomSymbolDrag(event, ${jsArg(sym.id)})"
         title="${esc(sym.name)} (${esc(sym.standard)})"
         style="display:flex;align-items:center;justify-content:space-between;gap:4px;">
      <span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">🧩 ${esc(sym.name)}</span>
      <span style="font-size:9px;color:#38bdf8;cursor:pointer;" onclick="event.stopPropagation();deleteCustomSymbol(${jsArg(sym.id)})" title="Supprimer">✕</span>
    </div>
  `).join('');
}

function openSymbolLibraryModal() {
  document.getElementById('symbolLibraryModal').style.display = 'flex';
}

function closeSymbolLibraryModal() {
  document.getElementById('symbolLibraryModal').style.display = 'none';
}

async function installBuiltinPack(packId) {
  const res = await fetch(`/api/symbols/install-pack/${encodeURIComponent(packId)}`, {
    method: 'POST'
  });
  if (res.ok) {
    state.symbols = await getJson('/api/symbols', state.symbols);
    renderCustomSymbolsPalette();
    alert(`Pack industriel "${packId}" installé avec succès dans votre Palette Studio ! Glissez-déposez ses symboles sur vos synoptiques.`);
  } else {
    alert(`Erreur : ${await apiError(res)}`);
  }
}

async function handleSymbolFileUpload(event) {
  const file = event.target.files[0];
  if (!file) return;
  if (file.size > 1024 * 1024) {
    alert('Fichier trop volumineux (1 Mo maximum).');
    return;
  }
  const text = await file.text();
  document.getElementById('symImportRaw').value = text;
  const baseName = file.name.replace(/\.(svg|xml|json)$/i, '');
  document.getElementById('symImportName').value = baseName;
}

async function submitSymbolImport() {
  const payload = {
    library_name: document.getElementById('symImportName').value.trim(),
    category: document.getElementById('symImportCat').value.trim(),
    format: document.getElementById('symImportFormat').value,
    raw_content: document.getElementById('symImportRaw').value,
    animate_rotation: document.getElementById('symImportAnim').checked
  };
  const res = await fetch('/api/symbols/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });
  if (res.ok) {
    const imported = await res.json();
    state.symbols = await getJson('/api/symbols', state.symbols);
    renderCustomSymbolsPalette();
    closeSymbolLibraryModal();
    alert(`${imported.length} symbole(s) industriel(s) importé(s) dans la section "📦 Bibliothèques Importées" de la Palette Studio !`);
  } else {
    alert(`Erreur d'import : ${await apiError(res)}`);
  }
}

async function deleteCustomSymbol(id) {
  if (!confirm('Supprimer ce symbole de la bibliothèque ?')) return;
  const res = await fetch(`/api/symbols/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (res.ok) {
    state.symbols = state.symbols.filter(s => s.id !== id);
    renderCustomSymbolsPalette();
  } else {
    alert(await apiError(res));
  }
}
