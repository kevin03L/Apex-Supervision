// ============================================================================
// Apex Supervision — Moteur Applicatif Frontend (RBAC, Sécurité & Performance)
// ============================================================================
//
// Règles de sécurité de ce fichier :
// - Toute donnée dynamique insérée dans du HTML passe par esc().
// - Toute donnée passée en argument d'un gestionnaire onclick/onchange passe par jsArg().
// - Tout SVG importé passe par sanitizeSvg() (reconstruction sur liste blanche) et est inséré
//   par le DOM, jamais par innerHTML.

const WIDGET_TYPES = [
  'Gauge', 'Pump', 'Valve', 'MotorValve2Way', 'Fan', 'Damper', 'Boiler', 'Chiller',
  'Compressor', 'HeatExchanger', 'CoolingTower', 'HeaterBattery', 'FilterUnit',
  'CircuitBreaker', 'Transformer', 'ElectricMotor', 'FlowMeter', 'PressureGauge',
  'Thermometer', 'TrendChart', 'SetpointButton', 'SwitchButton', 'PushButton',
  'TankLevel', 'IndicatorLed', 'NumericDisplay', 'PipeSegment', 'CustomSvgSymbol'
];

// Protocoles sans implémentation réelle (valeurs fictives) : signalés comme tels dans l'IHM
const SIMULATED_PROTOCOLS = ['Simulator', 'OpcUa', 'BacnetIp'];
// Auto-Découverte : catalogue de démonstration des seuls protocoles simulés, simulation active.
// Jamais pour un équipement réel (Modbus TCP, MQTT, S7) : ses variables se déclarent d'après sa table d'échange.
const DISCOVERABLE_PROTOCOLS = ['OpcUa', 'BacnetIp'];
// Aide à la saisie par protocole : endpoint et exemple d'adresse de variable
const PROTOCOL_HINTS = {
  ModbusTcp: { endpoint: 'ex. 192.168.1.10:502', address: 'ex. HR:0:FLOAT32, COIL:3, IR:5' },
  Mqtt: { endpoint: 'ex. mqtt://192.168.1.20:1883', address: 'ex. batiment/ta01/temp ou sujet#clé' },
  SiemensS7: { endpoint: 'ex. 192.168.0.10?rack=0&slot=1 (S7-300 : slot=2)', address: 'ex. DB10.DBD0:REAL, DB10.DBX4.0, MW10' },
  OpcUa: { endpoint: 'ex. opc.tcp://192.168.1.30:4840', address: 'ex. ns=2;s=Chiller01.LWT' },
  BacnetIp: { endpoint: 'ex. 192.168.1.40:47808', address: 'ex. BACNET:AI:101:Present_Value' },
  Simulator: { endpoint: 'local', address: '' }
};

const PROTOCOL_LABELS = {
  ModbusTcp: 'Modbus TCP',
  Mqtt: 'MQTT',
  SiemensS7: 'Siemens S7',
  OpcUa: 'OPC-UA',
  BacnetIp: 'BACnet/IP',
  Simulator: 'Simulateur'
};

/** Exemples d'endpoint et d'adresse adaptés au protocole choisi. */
function refreshProtocolHints() {
  const proto = document.getElementById('drvProto');
  const hints = PROTOCOL_HINTS[proto && proto.value] || PROTOCOL_HINTS.ModbusTcp;
  const ep = document.getElementById('drvEndpoint');
  if (ep) ep.placeholder = hints.endpoint;
}

/** Exemple d'adresse adapté au protocole du driver saisi dans le formulaire de variable. */
function refreshTagAddressHint() {
  const src = document.getElementById('tagFormSource');
  const addr = document.getElementById('tagFormAddr');
  const hint = document.getElementById('tagFormAddrHint');
  if (!src || !addr) return;
  const drv = state.drivers.find(d => d.id === src.value.trim());
  const text = (drv && PROTOCOL_HINTS[drv.protocol] && PROTOCOL_HINTS[drv.protocol].address) || 'ex. HR:0:FLOAT32, COIL:3, IR:5';
  addr.placeholder = text;
  if (hint) hint.textContent = `Format d'adresse (${drv ? drv.protocol : 'ModbusTcp'}) : ${text}`;
}

const WEEK_DAYS = [[1, 'Lun'], [2, 'Mar'], [3, 'Mer'], [4, 'Jeu'], [5, 'Ven'], [6, 'Sam'], [7, 'Dim']];

const DEFAULT_SVG = '<svg viewBox="0 0 100 100"><circle cx="50" cy="50" r="35" stroke="currentColor" stroke-width="3" fill="none"/></svg>';

let editingScheduleId = null;

const state = {
  // Ouverture en exploitation (RUN) : le mode Studio s'active explicitement pour éditer
  mode: 'run',
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
  alarmJournal: [],
  drivers: [],
  schedules: [],
  systemStatus: null,
  // Commande de synoptique sélectionnée, en attente d'exécution (voir selectCommand)
  armedCommand: null,
  armedTimer: null,
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

// --- Assainissement des SVG : mêmes listes blanches que le serveur (crates/apex-core/src/security.rs) ---
const SVG_NS = 'http://www.w3.org/2000/svg';
const SVG_MAX_DEPTH = 64;
const SVG_ELEMENTS = new Set([
  'svg', 'g', 'defs', 'symbol', 'use', 'title', 'desc', 'path', 'rect', 'circle', 'ellipse', 'line',
  'polyline', 'polygon', 'text', 'tspan', 'textPath', 'linearGradient', 'radialGradient', 'stop',
  'pattern', 'clipPath', 'mask', 'marker', 'image', 'filter', 'feBlend', 'feColorMatrix',
  'feComponentTransfer', 'feFuncR', 'feFuncG', 'feFuncB', 'feFuncA', 'feComposite', 'feDropShadow',
  'feFlood', 'feGaussianBlur', 'feMerge', 'feMergeNode', 'feMorphology', 'feOffset', 'feTurbulence',
  'feDiffuseLighting', 'feSpecularLighting', 'feDistantLight', 'fePointLight', 'feSpotLight',
  'feDisplacementMap', 'feTile', 'feConvolveMatrix', 'switch', 'a',
  'animate', 'animateMotion', 'animateTransform', 'set'
]);
const SVG_ANIMATIONS = new Set(['animate', 'animateMotion', 'animateTransform', 'set']);
const SVG_HREF_ELEMENTS = new Set([
  'a', 'use', 'linearGradient', 'radialGradient', 'pattern', 'textPath', 'image', 'filter',
  'animate', 'animateMotion', 'animateTransform', 'set'
]);
const SVG_ATTRIBUTES = new Set([
  'id', 'class', 'style', 'transform', 'viewBox', 'preserveAspectRatio', 'version',
  'x', 'y', 'width', 'height', 'x1', 'y1', 'x2', 'y2', 'cx', 'cy', 'r', 'rx', 'ry', 'fx', 'fy', 'fr',
  'd', 'points', 'pathLength', 'dx', 'dy', 'rotate', 'textLength', 'lengthAdjust', 'startOffset',
  'method', 'spacing',
  'fill', 'fill-opacity', 'fill-rule', 'stroke', 'stroke-width', 'stroke-opacity', 'stroke-linecap',
  'stroke-linejoin', 'stroke-miterlimit', 'stroke-dasharray', 'stroke-dashoffset', 'opacity', 'color',
  'display', 'visibility', 'overflow', 'clip-path', 'clip-rule', 'mask', 'filter', 'marker-start',
  'marker-mid', 'marker-end', 'stop-color', 'stop-opacity', 'flood-color', 'flood-opacity',
  'font-family', 'font-size', 'font-weight', 'font-style', 'font-variant', 'font-stretch',
  'text-anchor', 'dominant-baseline', 'alignment-baseline', 'baseline-shift', 'letter-spacing',
  'word-spacing', 'text-decoration', 'writing-mode', 'direction', 'vector-effect', 'shape-rendering',
  'text-rendering', 'image-rendering', 'color-interpolation', 'color-interpolation-filters',
  'paint-order', 'mix-blend-mode', 'isolation', 'transform-origin',
  'offset', 'gradientUnits', 'gradientTransform', 'spreadMethod', 'patternUnits',
  'patternContentUnits', 'patternTransform', 'clipPathUnits', 'maskUnits', 'maskContentUnits',
  'markerWidth', 'markerHeight', 'markerUnits', 'refX', 'refY', 'orient',
  'filterUnits', 'primitiveUnits', 'in', 'in2', 'result', 'mode', 'operator', 'k1', 'k2', 'k3', 'k4',
  'values', 'type', 'stdDeviation', 'edgeMode', 'radius', 'tableValues', 'slope', 'intercept',
  'amplitude', 'exponent', 'baseFrequency', 'numOctaves', 'seed', 'stitchTiles',
  'surfaceScale', 'specularConstant', 'specularExponent', 'diffuseConstant', 'kernelUnitLength',
  'lighting-color', 'azimuth', 'elevation', 'z', 'pointsAtX', 'pointsAtY', 'pointsAtZ',
  'limitingConeAngle', 'scale', 'xChannelSelector', 'yChannelSelector', 'order', 'kernelMatrix',
  'divisor', 'bias', 'targetX', 'targetY', 'preserveAlpha',
  'attributeName', 'attributeType', 'from', 'to', 'by', 'begin', 'dur', 'end', 'min', 'max',
  'restart', 'repeatCount', 'repeatDur', 'calcMode', 'keyTimes', 'keySplines', 'keyPoints',
  'additive', 'accumulate', 'path'
]);
const SVG_ANIMATION_FORBIDDEN_TARGETS = new Set(['id', 'style', 'attributeName']);
const SVG_FORBIDDEN_VALUE_FRAGMENTS = ['javascript:', 'vbscript:', 'livescript:', 'expression(', '-moz-binding', 'behavior:', '@import', '\\'];
const SVG_RASTER_DATA = /^data:image\/(png|jpeg|gif|webp);base64,[a-z0-9+/=\s]*$/i;
const SVG_SAFE_ID = /^[A-Za-z0-9_.:-]{1,128}$/;
const svgCache = new Map();

/** Valeur d'attribut SVG acceptable (mêmes règles que le serveur). */
function svgAttrValueOk(element, attr, value) {
  const compact = value.replace(/[\s\u0000-\u001f\u007f-\u009f]/g, '').toLowerCase();
  if (SVG_FORBIDDEN_VALUE_FRAGMENTS.some(f => compact.includes(f))) return false;
  let rest = compact;
  for (let i = rest.indexOf('url('); i !== -1; i = rest.indexOf('url(')) {
    rest = rest.slice(i + 4);
    if (!rest.replace(/^["']+/, '').startsWith('#')) return false;
  }
  const rasterImage = attr === 'href' && element === 'image';
  if (compact.includes('data:') && !rasterImage && attr !== 'id') return false;
  const v = value.trim();
  switch (attr) {
    case 'href': return (v.startsWith('#') && SVG_SAFE_ID.test(v.slice(1))) || (rasterImage && SVG_RASTER_DATA.test(v));
    case 'style': return !compact.includes('/*') && !compact.includes('<');
    case 'id': return SVG_SAFE_ID.test(v);
    case 'attributeName': return SVG_ATTRIBUTES.has(v) && !SVG_ANIMATION_FORBIDDEN_TARGETS.has(v);
    default: return true;
  }
}

/**
 * Reconstruit un SVG dans un arbre DOM neuf (createElementNS) à partir de son analyse XML.
 */
function buildCleanSvg(raw) {
  const doc = new DOMParser().parseFromString(raw, 'image/svg+xml');
  if (doc.getElementsByTagName('parsererror').length) return null;
  const copy = (src, depth) => {
    const name = src.localName;
    if (src.prefix || (src.namespaceURI && src.namespaceURI !== SVG_NS)) return null;
    if (!SVG_ELEMENTS.has(name) || depth > SVG_MAX_DEPTH) return null;
    if (depth === 0 && name !== 'svg') return null;
    const el = document.createElementNS(SVG_NS, name);
    for (const a of [...src.attributes]) {
      const attr = (a.name === 'href' || a.name === 'xlink:href') ? 'href' : a.name;
      const allowed = attr === 'href' ? SVG_HREF_ELEMENTS.has(name) : SVG_ATTRIBUTES.has(attr);
      if (!allowed || (depth === 0 && attr === 'style') || el.hasAttribute(attr)) continue;
      if (!svgAttrValueOk(name, attr, a.value)) {
        if (SVG_ANIMATIONS.has(name) && attr === 'attributeName') return null;
        continue;
      }
      el.setAttribute(attr, a.value.trim());
    }
    for (const child of [...src.childNodes]) {
      if (child.nodeType === Node.TEXT_NODE) {
        el.appendChild(document.createTextNode(child.data));
      } else if (child.nodeType === Node.ELEMENT_NODE) {
        const c = copy(child, depth + 1);
        if (c) el.appendChild(c);
      }
    }
    return el;
  };
  return doc.documentElement ? copy(doc.documentElement, 0) : null;
}

/**
 * Filtre côté client des SVG (défense en profondeur, en plus de la reconstruction serveur).
 */
function sanitizeSvg(svgText) {
  const raw = String(svgText || '');
  let clean = svgCache.get(raw);
  if (clean === undefined) {
    try {
      clean = buildCleanSvg(raw);
    } catch (_) {
      clean = null;
    }
    if (!clean && raw !== DEFAULT_SVG) clean = sanitizeSvg(DEFAULT_SVG);
    if (svgCache.size > 300) svgCache.clear();
    svgCache.set(raw, clean);
  }
  return clean ? clean.cloneNode(true) : null;
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

// ============================================================================
// COQUILLE : MENU GAUCHE REPLIABLE & FENÊTRE MODALE GÉNÉRIQUE
// ============================================================================

/** Restaure l'état replié du menu depuis localStorage. */
function restoreNavFold() {
  try {
    const saved = localStorage.getItem('apex_nav_folded');
    if (saved === '1') document.body.classList.add('folded');
  } catch (_) {}
}

/** Bascule le repli du menu latéral (ou l'ouverture en superposition sous 600 px). */
function toggleNavFold() {
  if (window.innerWidth <= 600) {
    document.body.classList.toggle('nav-open');
    return;
  }
  const folded = document.body.classList.toggle('folded');
  try {
    localStorage.setItem('apex_nav_folded', folded ? '1' : '0');
  } catch (_) {}
  if (typeof fitSynopticToScreen === 'function') {
    setTimeout(fitSynopticToScreen, 210);
  }
}

/**
 * Ouvre la fenêtre modale générique sur le formulaire demandé (`driver`, `tag`, `schedule`, `user`)
 * et place le focus sur le premier champ de saisie activé.
 */
function openModal(formKey, title) {
  const bg = document.getElementById('appModal');
  const titleEl = document.getElementById('appModalTitle');
  if (!bg) return;
  if (titleEl) titleEl.textContent = title || 'Formulaire';
  let activePane = null;
  bg.querySelectorAll('.modal-pane, [data-modal-form]').forEach(pane => {
    const match = pane.id === `modalPane-${formKey}` || pane.dataset.modalForm === formKey;
    pane.style.display = match ? 'grid' : 'none';
    if (match) activePane = pane;
  });
  bg.querySelectorAll('.modal-submit').forEach(btn => {
    btn.style.display = btn.dataset.pane === formKey ? 'inline-flex' : 'none';
  });
  bg.classList.add('on');
  if (activePane) {
    setTimeout(() => {
      const first = activePane.querySelector('input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled])');
      if (first) first.focus();
    }, 20);
  }
}

/** Ferme la fenêtre modale générique. */
function closeModal() {
  const bg = document.getElementById('appModal');
  if (bg) bg.classList.remove('on');
}

document.addEventListener('keydown', e => {
  if (e.key !== 'Escape') return;
  const appModal = document.getElementById('appModal');
  if (appModal && appModal.classList.contains('on')) {
    e.stopPropagation();
    closeModal();
    return;
  }
  const symModal = document.getElementById('symbolLibraryModal');
  if (symModal && symModal.style.display !== 'none' && symModal.style.display !== '') {
    e.stopPropagation();
    closeSymbolLibraryModal();
    return;
  }
  const authModal = document.getElementById('authModal');
  if (authModal && authModal.style.display !== 'none' && authModal.style.display !== '' && state.currentUser) {
    e.stopPropagation();
    closeAuthModal();
  }
});

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

/** Tailles maximales acceptées par le serveur (http.rs : PROJECT_IMPORT_MAX_BYTES, SYMBOL_IMPORT_MAX_BYTES). */
const PROJECT_IMPORT_MAX_BYTES = 64 * 1024 * 1024;
const SYMBOL_IMPORT_MAX_BYTES = 16 * 1024 * 1024;

function formatMegabytes(bytes) {
  return `${(bytes / (1024 * 1024)).toLocaleString('fr-FR', { maximumFractionDigits: 1 })} Mo`;
}

/** Message d'un envoi refusé car trop volumineux (HTTP 413 ou contrôle préalable). */
function payloadTooLargeMessage(maxBytes, actualBytes) {
  const size = actualBytes ? ` (${formatMegabytes(actualBytes)})` : '';
  const limit = maxBytes ? ` : ${formatMegabytes(maxBytes)} au maximum` : '';
  return `Contenu trop volumineux${size} : le serveur refuse les envois de cette taille${limit}.`;
}

/** Notification brève et non bloquante (succès ou erreur). */
function notify(message, kind = 'ok') {
  let box = document.getElementById('toastBox');
  if (!box) {
    box = document.createElement('div');
    box.id = 'toastBox';
    box.setAttribute('role', 'status');
    box.setAttribute('aria-live', 'polite');
    document.body.appendChild(box);
  }
  const t = document.createElement('div');
  t.className = 'toast toast-' + kind;
  t.textContent = message;
  box.appendChild(t);
  setTimeout(() => t.remove(), 3500);
}

/** Lit le message d'erreur JSON d'une réponse API. */
async function apiError(res, maxBytes) {
  if (res.status === 413) return payloadTooLargeMessage(maxBytes);
  try {
    const body = await res.json();
    return body.message || `Erreur HTTP ${res.status}`;
  } catch (_) {
    return `Erreur HTTP ${res.status}`;
  }
}

window.addEventListener('DOMContentLoaded', async () => {
  restoreNavFold();
  startSystemClock();
  renderScheduleDaysPicker();
  refreshProtocolHints();
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
  if (!el) return;
  const tick = () => {
    el.textContent = new Date().toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  };
  tick();
  setInterval(tick, 1000);
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
  updateHeaderChips();
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
  if (!viewer) {
    renderDriversCardsOnly();
    renderTagCatalogTable();
  }
  renderSchedulesTable();
  preloadTrendHistory();
  return true;
}

/** Nombre de points tracés par une courbe de tendance de synoptique. */
const TREND_POINTS = 55;

/**
 * Pré-remplit les courbes de tendance des synoptiques avec les derniers points de
 * l'historique, pour qu'elles ne repartent pas vides à chaque chargement de la page.
 */
async function preloadTrendHistory() {
  const tagIds = new Set();
  state.dashboards.forEach(d => (d.widgets || []).forEach(w => {
    const tag = w.bound_tag_id && state.tags[w.bound_tag_id];
    if (w.widget_type === 'TrendChart' && tag && tag.historize) tagIds.add(w.bound_tag_id);
  }));
  await Promise.all([...tagIds].map(async tagId => {
    const res = await getJson(`/api/history/${encodeURIComponent(tagId)}?limit=${TREND_POINTS}`, null);
    if (!res || !Array.isArray(res.points)) return;
    const values = res.points
      .map(p => typeof p.value === 'number' ? p.value : (typeof p.value === 'boolean' ? (p.value ? 1 : 0) : NaN))
      .filter(Number.isFinite);
    if (!values.length) return;
    const live = (state.history[tagId] || []).slice(1);
    state.history[tagId] = values.concat(live).slice(-TREND_POINTS);
  }));
  if (tagIds.size && state.activeTab === 'synoptic') renderDashboard();
}

function updateSysInfoPill() {
  const el = document.getElementById('sysInfoPill');
  const st = state.systemStatus;
  if (!el || !st) return;
  el.textContent = st.simulation_enabled
    ? `v${st.version} · PLC simulé 127.0.0.1:${st.modbus_slave_port}`
    : `v${st.version}`;
}

/** Met à jour les pastilles de l'en-tête (« N alarmes » et « N équipements en ligne »). */
function updateHeaderChips() {
  const visibleAlarms = (state.alarms || []).filter(a => !isShelved(a));
  const unackCount = visibleAlarms.filter(a => a.state === 'ActiveUnack' || a.state === 'ClearedUnack').length;
  const activeCount = visibleAlarms.filter(a => a.state === 'ActiveUnack' || a.state === 'ActiveAck').length;

  const alDot = document.getElementById('hdrAlarmsDot');
  const alTxt = document.getElementById('hdrAlarmsText');
  if (alTxt) {
    alTxt.textContent = `${activeCount} alarme${activeCount > 1 ? 's' : ''}`;
  }
  if (alDot) {
    alDot.style.background = unackCount > 0 ? 'var(--crit)' : (activeCount > 0 ? 'var(--warn)' : 'var(--run)');
  }

  const drvDot = document.getElementById('hdrDriversDot');
  const drvTxt = document.getElementById('hdrDriversText');
  let online = 0;
  let hasFault = false;
  if (Array.isArray(state.drivers) && state.drivers.length > 0) {
    online = state.drivers.filter(d => d.stats && d.stats.running && d.stats.connected).length;
    hasFault = state.drivers.some(d => d.stats && d.stats.running && !d.stats.connected);
  } else if (state.systemStatus) {
    online = Number(state.systemStatus.running_drivers) || 0;
  }
  if (drvTxt) {
    drvTxt.textContent = `${online} équipement${online > 1 ? 's' : ''} en ligne`;
  }
  if (drvDot) {
    drvDot.style.background = hasFault ? 'var(--crit)' : (online > 0 ? 'var(--run)' : 'var(--stop)');
  }
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
    btn.textContent = 'Reprendre acquisition';
    btn.style.borderColor = 'var(--run)';
    btn.style.color = '#86efac';
  } else {
    btn.textContent = 'Pause acquisition';
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
    notify(state.systemStatus.acquisition_paused ? 'Acquisition mise en pause' : 'Acquisition reprise');
  } else {
    notify(await apiError(res), 'error');
  }
}

async function confirmServerRestart() {
  const ok = await confirmDialog(
    'Redémarrer le serveur ?',
    'Confirmer le redémarrage du moteur Apex Supervision ?\nLes drivers actifs seront réinitialisés à chaud (les drivers arrêtés volontairement restent arrêtés).',
    'Redémarrer'
  );
  if (!ok) return;

  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').innerHTML = '<svg class="i" style="width:3.2em;height:3.2em;"><use href="#i-refresh"/></svg>';
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
    if (state.activeTab === 'historian' || state.activeTab === 'system') await loadSystemAndHistorianTab();
  }, 900);
}

async function confirmServerShutdown() {
  const ok = await confirmDialog(
    'Mettre hors tension ?',
    'Confirmer la mise hors tension de l\'application Apex Supervision ?\nTous les drivers industriels seront déconnectés proprement.',
    'Mettre hors tension'
  );
  if (!ok) return;

  let res;
  try {
    res = await fetch('/api/system/standby', { method: 'POST' });
  } catch (_) {
    notify('Serveur injoignable : la mise hors tension n\'a pas été effectuée.', 'error');
    return;
  }
  if (!res.ok) {
    notify(`Mise hors tension refusée : ${await apiError(res)}`, 'error');
    return;
  }
  state.systemStatus = await res.json().catch(() => state.systemStatus);
  showPowerOffOverlay();
}

function showPowerOffOverlay() {
  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').innerHTML = '<svg class="i" style="width:3.2em;height:3.2em;color:var(--crit);"><use href="#i-power"/></svg>';
  document.getElementById('powerOverlayTitle').style.color = '#ef4444';
  document.getElementById('powerOverlayTitle').textContent = 'Application Apex Supervision Arrêtée (Hors Tension)';
  document.getElementById('powerOverlayMsg').innerHTML = `
    Tous les drivers industriels ont été arrêtés proprement.<br>
    L'acquisition temps réel est hors tension.<br><br>
    <div style="display:flex;gap:12px;justify-content:center;margin-top:16px;flex-wrap:wrap;">
      <button class="btn btn-success" style="padding:10px 20px;font-size:14px;" onclick="powerOnFromStandby()">Rallumer / Redémarrer l'application</button>
      <button class="btn btn-danger" style="padding:10px 16px;font-size:12px;" onclick="terminateOsProcess()">Arrêter définitivement le moteur apex-scada</button>
    </div>
  `;
  overlay.style.display = 'flex';
}

async function powerOnFromStandby() {
  const overlay = document.getElementById('powerOverlay');
  document.getElementById('powerOverlayIcon').innerHTML = '<svg class="i" style="width:3.2em;height:3.2em;color:var(--run);"><use href="#i-power"/></svg>';
  document.getElementById('powerOverlayTitle').style.color = '#10b981';
  document.getElementById('powerOverlayTitle').textContent = 'Démarrage de Apex Supervision en cours...';
  document.getElementById('powerOverlayMsg').textContent = 'Remise sous tension des drivers industriels et synchronisation de la base de variables...';

  try {
    const res = await fetch('/api/system/restart', { method: 'POST' });
    if (res.ok) state.systemStatus = await res.json();
  } catch (_) {}

  setTimeout(async () => {
    state.shuttingDown = false;
    overlay.style.display = 'none';
    await bootstrapData();
    if (state.activeTab === 'historian' || state.activeTab === 'system') await loadSystemAndHistorianTab();
  }, 700);
}

async function terminateOsProcess() {
  const ok = await confirmDialog(
    'Arrêter définitivement le moteur ?',
    'Voulez-vous arrêter définitivement le moteur apex-scada ?\nL\'IHM restera indisponible jusqu\'à sa relance.',
    'Arrêter'
  );
  if (!ok) return;

  state.shuttingDown = true;
  let res;
  try {
    res = await fetch('/api/system/shutdown', { method: 'POST' });
  } catch (_) {
    state.shuttingDown = false;
    notify('Serveur injoignable : l\'arrêt n\'a pas pu être demandé.', 'error');
    return;
  }
  if (!res.ok) {
    state.shuttingDown = false;
    notify(`Arrêt refusé : ${await apiError(res)}`, 'error');
    return;
  }
  document.getElementById('powerOverlayMsg').innerHTML = `
    Le moteur <code>apex-scada</code> est arrêté.<br><br>
    Relancez-le depuis l'utilitaire Apex Supervision, ou par le service :
    <code>apex-scada service start</code> (Windows, invite administrateur)
    ou <code>sudo systemctl start apex-scada</code> (Linux).
  `;
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
    if (pill) pill.classList.remove('offline');
    if (txt) txt.textContent = 'En direct';
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
      updateHeaderChips();
      renderTagTree();
      renderDashboard();
      renderAlarmBanner();
      renderAlarmsTable();
      if (state.activeTab === 'tagdb') {
        renderDriversCardsOnly();
        renderTagCatalogTable();
      }
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
      updateHeaderChips();
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
    if (pill) pill.classList.add('offline');
    if (txt) txt.textContent = 'Reconnexion...';
    state.wsRetryTimer = setTimeout(async () => {
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
    if (state.history[st.tag_id].length > TREND_POINTS) state.history[st.tag_id].shift();
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
  if (txt) txt.textContent = `COV (${state.covCount})`;
  const onTagDb = state.activeTab === 'tagdb';
  ids.forEach(id => {
    updateTagRowInPlace(id);
    updateBoundWidgetsInPlace(id);
    if (onTagDb) updateCatalogRowInPlace(id);
  });
}

// ============================================================================
// ÉCRAN FLUX LOGIQUES : ÉDITEUR VISUEL DE FLUX NODE-RED (9 TYPES DE BLOCS)
// ============================================================================

function populateFlowSelect() {
  const sel = document.getElementById('flowSelect');
  if (!sel) return;
  sel.innerHTML = state.flows.map(f =>
    `<option value="${esc(f.id)}" ${f.id === state.currentFlowId ? 'selected' : ''}>${esc(f.name)} (${f.suspended_reason ? 'suspendu' : (f.enabled ? 'actif' : 'inactif')})</option>`
  ).join('');
  updateFlowToggleBtn();
}

function updateFlowToggleBtn() {
  const flow = getActiveFlow();
  const btn = document.getElementById('btnToggleFlow');
  if (!flow || !btn) return;
  btn.textContent = flow.suspended_reason
    ? 'Suspendu (mode sûr)'
    : (flow.enabled ? 'Flux actif' : 'Flux inactif');
  btn.title = flow.suspended_reason || '';
}

function selectFlow(id) {
  state.currentFlowId = id;
  state.selectedFlowNodeId = null;
  state.wiringSource = null;
  updateFlowToggleBtn();
  renderFlowEditor();
  const flowInsp = document.getElementById('flowInspectorContent');
  if (flowInsp) {
    flowInsp.innerHTML = '<p class="hint">Sélectionnez un nœud du graphe pour régler ses paramètres.</p>';
  }
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
    notify('Impossible de supprimer le dernier flux.', 'error');
    return;
  }
  const flow = getActiveFlow();
  if (!flow) return;
  const ok = await confirmDialog('Supprimer le flux ?', `Supprimer le flux « ${flow.name} » ?`, 'Supprimer');
  if (!ok) return;
  const res = await fetch(`/api/flows/${encodeURIComponent(flow.id)}`, { method: 'DELETE' });
  if (!res.ok) {
    notify(await apiError(res), 'error');
    return;
  }
  state.flows = state.flows.filter(f => f.id !== flow.id);
  state.currentFlowId = state.flows[0].id;
  populateFlowSelect();
  renderFlowEditor();
  notify('Flux supprimé');
}

async function triggerCurrentFlowNow() {
  const flow = getActiveFlow();
  if (!flow) return;
  const res = await fetch(`/api/flows/${encodeURIComponent(flow.id)}/trigger`, { method: 'POST' });
  if (!res.ok) notify(await apiError(res), 'error');
  else notify(`Flux « ${flow.name} » exécuté`);
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
    notify(`Flux « ${saved.name} » créé`);
  } else {
    notify(await apiError(res), 'error');
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
  if (!svg) return;
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
    path.onclick = async () => {
      const ok = await confirmDialog('Supprimer la liaison ?', `Supprimer le câble reliant « ${fromN.label} » à « ${toN.label} » ?`, 'Supprimer');
      if (ok) {
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
  if (!container) return;
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
      ? `<span class="port-pill" onclick="handlePortClick(event, ${jsArg(node.id)}, 'in', true)">IN</span>`
      : `<span></span>`;

    const outPortsHtml = outPorts.map(p => {
      const isSource = state.wiringSource && state.wiringSource.nodeId === node.id && state.wiringSource.port === p;
      return `<span class="port-pill ${isSource ? 'wiring-source' : ''}" onclick="handlePortClick(event, ${jsArg(node.id)}, ${jsArg(p)}, false)">OUT:${esc(p)}</span>`;
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
    document.getElementById('wiringStatus').textContent = `Câblage actif depuis ${nodeId} (${port}) : cliquez maintenant sur le port « IN » du nœud destination.`;
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
    document.getElementById('wiringStatus').textContent = 'Câble créé. Cliquez sur « Déployer à chaud » pour l\'activer.';
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

  const html = `
    <div class="form-group">
      <label>Libellé du nœud</label>
      <input value="${esc(node.label)}" oninput="updateFlowNodeLabel(${nid}, this.value)" />
    </div>
    ${specificHtml}
    <button class="btn primary" onclick="saveCurrentFlow()">Déployer à chaud</button>
    <button class="btn btn-danger" onclick="deleteFlowNode(${nid})">Supprimer le nœud</button>
  `;
  const flowBox = document.getElementById('flowInspectorContent');
  if (flowBox) flowBox.innerHTML = html;
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
  const flowBox = document.getElementById('flowInspectorContent');
  if (flowBox) flowBox.innerHTML = '<p class="hint">Sélectionnez un nœud du graphe pour régler ses paramètres.</p>';
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
    notify(`Déploiement refusé : ${await apiError(res)}`, 'error');
    return;
  }
  const views = await getJson('/api/flows', []);
  views.forEach(v => {
    const local = state.flows.find(f => f.id === v.id);
    if (local) {
      local.suspended_reason = v.suspended_reason;
      local.blocked_outputs = v.blocked_outputs;
    }
  });
  populateFlowSelect();
  if (!silent) {
    notify(`Flux « ${flow.name} » déployé à chaud`);
  }
}

// ============================================================================
// ÉCRAN ALARMES ISA-18.2, BANDEAU DE PIED DE PAGE & JOURNAL SQLITE
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

/** Formulaire des réglages système (écran Système). */
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
  if (!enabled) {
    const ok = await confirmDialog(
      'Désactiver la mise en sommeil ?',
      'Toutes les alarmes actuellement en sommeil seront réveillées immédiatement.',
      'Désactiver'
    );
    if (!ok) {
      renderSettingsForm();
      return;
    }
  }
  const res = await fetch('/api/system/settings', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ alarm_shelving_enabled: enabled, alarm_shelve_max_minutes: max })
  });
  if (!res.ok) {
    notify(`Réglages refusés : ${await apiError(res)}`, 'error');
    renderSettingsForm();
    return;
  }
  state.systemStatus = Object.assign({}, state.systemStatus || {}, { settings: await res.json() });
  renderSettingsForm();
  renderAlarmsTable();
  notify('Réglages système appliqués');
}

/** Une alarme en sommeil reste suivie mais est exclue du bandeau et des compteurs. */
function isShelved(a) {
  return !!(a.shelved_until && new Date(a.shelved_until) > new Date());
}

/** Met à jour le bandeau d'alarme en pied de page, le badge du menu et les pastilles d'en-tête. */
function renderAlarmBanner() {
  const visible = state.alarms.filter(a => !isShelved(a));
  const shelvedCount = state.alarms.length - visible.length;
  const unack = visible.filter(a => a.state === 'ActiveUnack' || a.state === 'ClearedUnack');
  const active = visible.filter(a => a.state === 'ActiveUnack' || a.state === 'ActiveAck');

  const navCount = document.getElementById('navAlarmCount');
  if (navCount) {
    navCount.textContent = String(active.length);
    navCount.classList.toggle('zero', active.length === 0);
  }
  updateHeaderChips();

  const bar = document.getElementById('topAlarmBanner');
  const text = document.getElementById('topAlarmText');
  const timeEl = document.getElementById('topAlarmTime');
  const moreEl = document.getElementById('topAlarmMore');
  if (!bar || !text) return;

  bar.classList.toggle('has-unack', unack.length > 0);
  bar.classList.toggle('has-active', unack.length === 0 && active.length > 0);

  if (unack.length > 0 || active.length > 0) {
    const list = unack.length > 0 ? unack : active;
    const latest = list[0];
    if (timeEl) {
      timeEl.textContent = new Date(latest.raised_at).toLocaleTimeString('fr-FR');
    }
    text.innerHTML = `<b>${esc(latest.tag_id)}</b> — ${esc(latest.message)}${unack.length === 0 ? ' <span class="hint">(acquittée)</span>' : ''}`;
    if (moreEl) {
      const extra = list.length - 1;
      const parts = [];
      if (extra > 0) parts.push(`+${extra} autre${extra > 1 ? 's' : ''}`);
      if (shelvedCount > 0) parts.push(`${shelvedCount} en sommeil`);
      moreEl.textContent = parts.join(' · ');
    }
  } else {
    if (timeEl) timeEl.textContent = '';
    text.textContent = 'Aucune alarme active — Système nominal';
    if (moreEl) moreEl.textContent = shelvedCount ? `${shelvedCount} en sommeil` : '';
  }
}

function updateAlarmsViewMode() {
  const mode = (document.getElementById('alarmsViewMode') && document.getElementById('alarmsViewMode').value) || 'both';
  const activeBox = document.getElementById('alarmsActiveTblBox');
  const journalBox = document.getElementById('alarmsJournalTblBox');
  if (activeBox) activeBox.style.display = mode === 'journal' ? 'none' : '';
  if (journalBox) journalBox.style.display = (mode === 'active' || mode === 'unack') ? 'none' : '';
  renderAlarmsTable();
  if (mode === 'both' || mode === 'journal') loadAlarmJournal();
}

function renderAlarmsTable() {
  const tbody = document.getElementById('alarmsTableBody');
  if (!tbody) return;
  const viewMode = (document.getElementById('alarmsViewMode') && document.getElementById('alarmsViewMode').value) || 'both';
  const q = ((document.getElementById('alarmsSearchInput') && document.getElementById('alarmsSearchInput').value) || '').trim().toLowerCase();

  let list = state.alarms.slice();
  if (viewMode === 'unack') {
    list = list.filter(a => a.state === 'ActiveUnack' || a.state === 'ClearedUnack');
  }
  if (q) {
    list = list.filter(a =>
      [a.tag_id, a.message, a.severity, a.state, a.acked_by].some(v => String(v || '').toLowerCase().includes(q))
    );
  }

  if (list.length === 0) {
    tbody.innerHTML = `<tr><td colspan="8" style="text-align:center;color:var(--muted);padding:1.2em;">Aucune alarme à afficher.</td></tr>`;
    return;
  }
  const perms = (state.currentUser && state.currentUser.permissions) || {};
  const allowAck = perms.can_ack_alarms;
  const allowManage = perms.can_manage_alarms;

  tbody.innerHTML = list.map(a => {
    const shelved = isShelved(a);
    const needsAck = a.state === 'ActiveUnack' || a.state === 'ClearedUnack';
    const sevColor = a.severity === 'Critical' ? 'var(--crit)' : 'var(--warn)';
    const ackCell = needsAck
      ? (allowAck ? `<button class="btn primary" onclick="ackSingleAlarm(${jsArg(a.id)})">Acquitter</button>` : '<span class="q warn">Non acquittée</span>')
      : '<span class="q good">Acquittée</span>';
    const shelveCell = !allowManage ? '' : shelved
      ? `<button class="btn" onclick="unshelveAlarm(${jsArg(a.id)})">Réveiller</button>`
      : (a.state !== 'Normal' && canShelve(a) ? `<button class="btn" onclick="shelveAlarm(${jsArg(a.id)})" title="Masquer temporairement une alarme connue">Mettre en sommeil</button>` : '');
    return `
    <tr class="alarm-row-${esc(a.state)}" style="${shelved ? 'opacity:0.55;' : ''}">
      <td class="mono">${esc(new Date(a.raised_at).toLocaleTimeString('fr-FR'))}</td>
      <td><span class="chip"><i style="background:${sevColor}"></i>${esc(a.severity)}</span></td>
      <td>
        <span class="badge-qual">${esc(a.state)}</span>
        ${shelved ? `<div class="hint" style="font-size:.8em;">En sommeil jusqu'à ${esc(new Date(a.shelved_until).toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' }))} (${esc(a.shelved_by || '')})</div>` : ''}
      </td>
      <td class="mono"><b>${esc(a.tag_id)}</b></td>
      <td>${esc(a.message)}</td>
      <td class="mono">${esc(JSON.stringify(a.trigger_value))}</td>
      <td>${esc(a.acked_by || '--')}${a.ack_comment ? `<div class="hint" style="font-size:.8em;">${esc(a.ack_comment)}</div>` : ''}</td>
      <td style="display:flex;gap:.4em;flex-wrap:wrap;align-items:center;">${ackCell}${shelveCell}</td>
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
    notify(`Durée invalide : saisissez un nombre de minutes entre 1 et ${max}.`, 'error');
    return;
  }
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/shelve`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ minutes, comment: ackComment() })
  });
  if (!res.ok) notify(await apiError(res), 'error');
  else notify('Alarme mise en sommeil');
}

async function unshelveAlarm(alarmId) {
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/unshelve`, { method: 'POST' });
  if (!res.ok) notify(await apiError(res), 'error');
  else notify('Alarme réveillée');
}

/** Vide le commentaire d'acquittement une fois utilisé. */
function clearAckComment() {
  const el = document.getElementById('ackComment');
  if (el) el.value = '';
}

function ackComment() {
  const el = document.getElementById('ackComment');
  return el && el.value.trim() ? el.value.trim() : null;
}

async function ackSingleAlarm(alarmId) {
  if (state.currentUser && state.currentUser.role === 'Viewer') {
    const a = state.alarms.find(x => x.id === alarmId);
    const ok = await confirmDialog(
      'Acquitter cette alarme ?',
      `${a ? `${a.tag_id} — ${a.message}` : alarmId}\n\nL'acquittement sera enregistré à votre nom (${state.currentUser.username}).`,
      'Acquitter'
    );
    if (!ok) return;
  }
  const res = await fetch(`/api/alarms/${encodeURIComponent(alarmId)}/ack`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ comment: ackComment() })
  });
  if (res.ok) {
    clearAckComment();
    notify('Alarme acquittée');
    state.alarms = await getJson('/api/alarms', state.alarms);
    renderAlarmBanner();
    renderAlarmsTable();
    loadAlarmJournal();
  } else {
    notify(await apiError(res), 'error');
  }
}

async function ackAllAlarms() {
  const res = await fetch('/api/alarms/ack-all', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ comment: ackComment() })
  });
  if (res.ok) {
    clearAckComment();
    notify('Alarmes acquittées');
    state.alarms = await res.json();
    renderAlarmBanner();
    renderAlarmsTable();
    loadAlarmJournal();
  } else {
    notify(await apiError(res), 'error');
  }
}

async function clearNormalAlarms() {
  const res = await fetch('/api/alarms/clear-normal', { method: 'POST' });
  if (res.ok) {
    state.alarms = await res.json();
    renderAlarmBanner();
    renderAlarmsTable();
    notify('Alarmes résolues effacées');
  } else {
    notify(await apiError(res), 'error');
  }
}

async function loadAlarmJournal() {
  state.alarmJournal = await getJson('/api/alarms/journal', []);
  renderAlarmJournalTable();
}

function renderAlarmJournalTable() {
  const tbody = document.getElementById('alarmJournalTableBody');
  if (!tbody) return;
  const q = ((document.getElementById('alarmsSearchInput') && document.getElementById('alarmsSearchInput').value) || '').trim().toLowerCase();
  const list = (state.alarmJournal || []).filter(e =>
    !q || [e.event_type, e.severity, e.tag_id, e.message, e.operator].some(v => String(v || '').toLowerCase().includes(q))
  );
  if (!list.length) {
    tbody.innerHTML = '<tr><td colspan="6" style="text-align:center;color:var(--muted);padding:1.2em;">Aucun événement archivé dans le journal SQLite.</td></tr>';
    return;
  }
  tbody.innerHTML = list.map(e => `
    <tr>
      <td class="mono">${esc(new Date(e.recorded_at).toLocaleString('fr-FR'))}</td>
      <td><b>${esc(e.event_type)}</b></td>
      <td>${esc(e.severity)}</td>
      <td class="mono">${esc(e.tag_id)}</td>
      <td>${esc(e.message)}</td>
      <td>${esc(e.operator || 'Système')}</td>
    </tr>
  `).join('');
}

// ============================================================================
// ÉCRAN ÉQUIPEMENTS : DRIVERS (CARTES COMPACTES) & BASE DE VARIABLES (MODALES)
// ============================================================================

async function renderDriversAndTagDb() {
  state.drivers = await getJson('/api/drivers', state.drivers);
  renderDriversCardsOnly();
  renderTagCatalogTable();
  refreshDriverFormMode();
}

function renderDriversCardsOnly() {
  const box = document.getElementById('driversCardsContainer');
  const filterSel = document.getElementById('tagDbDriverFilter');
  const dataList = document.getElementById('drvSourceList');
  if (filterSel) {
    const current = filterSel.value;
    filterSel.innerHTML = '<option value="">Tous les équipements</option>' + state.drivers.map(d =>
      `<option value="${esc(d.id)}" ${d.id === current ? 'selected' : ''}>${esc(d.id)} (${esc(PROTOCOL_LABELS[d.protocol] || d.protocol)})</option>`
    ).join('');
  }
  if (dataList) {
    dataList.innerHTML = state.drivers.map(d => `<option value="${esc(d.id)}">${esc(d.name)}</option>`).join('');
  }
  updateHeaderChips();
  if (!box) return;

  const allowEdit = !!(state.currentUser && state.currentUser.permissions && state.currentUser.permissions.can_manage_drivers_tags);

  const cardsHtml = state.drivers.map(d => {
    const st = d.stats || {};
    const id = jsArg(d.id);
    const protoLabel = (PROTOCOL_LABELS[d.protocol] || d.protocol) + (SIMULATED_PROTOCOLS.includes(d.protocol) ? ' · simulé' : '');
    let dotColor = 'var(--stop)';
    let stateLabel = 'arrêté';
    if (st.running) {
      if (st.connected) {
        dotColor = 'var(--run)';
        stateLabel = `${Number(st.last_latency_ms) || 1} ms`;
      } else {
        dotColor = 'var(--crit)';
        stateLabel = st.last_error || 'injoignable';
      }
    }
    const writeWarn = st.last_write_rejected && st.last_write_rejected !== st.last_error
      ? `<div class="hint" style="color:var(--select);font-size:.78em;">Refus d'écriture : ${esc(st.last_write_rejected)}</div>`
      : '';

    return `
      <div class="card">
        <div>
          <span class="proto">${esc(protoLabel)}</span>
          <b>${esc(d.id)}</b>
        </div>
        <div class="st">
          <span class="dot" style="background:${dotColor}"></span>
          <span class="mono">${esc(d.endpoint)}</span>
          <span>· ${esc(stateLabel)}</span>
        </div>
        ${writeWarn}
        <div class="card-actions">
          ${st.running
            ? `<button class="btn" onclick="stopDriver(${id})">Arrêter</button>`
            : `<button class="btn primary" onclick="startDriver(${id})">Démarrer</button>`
          }
          <button class="btn" onclick="restartDriver(${id})">Redémarrer</button>
          <button class="btn" onclick="editDriverInForm(${id})">Modifier</button>
          ${canDiscoverTags(d)
            ? `<button class="btn" onclick="browseDriverTags(${id})" title="Catalogue de démonstration du protocole simulé">Découvrir</button>`
            : ''}
          <button class="btn btn-danger" onclick="deleteDriver(${id})">Suppr.</button>
        </div>
      </div>
    `;
  }).join('');

  const addCardHtml = allowEdit
    ? `<div class="card add" onclick="openNewDriverModal()">+ Ajouter un équipement</div>`
    : '';

  box.innerHTML = cardsHtml + addCardHtml;
}

/** L'Auto-Découverte n'est proposée que pour un protocole simulé, avec la simulation active. */
function canDiscoverTags(driver) {
  return DISCOVERABLE_PROTOCOLS.includes(driver.protocol)
    && !!(state.systemStatus && state.systemStatus.simulation_enabled);
}

async function driverAction(id, action) {
  const res = await fetch(`/api/drivers/${encodeURIComponent(id)}/${action}`, { method: 'POST' });
  if (!res.ok) notify(await apiError(res), 'error');
  await renderDriversAndTagDb();
}

async function startDriver(id) { await driverAction(id, 'start'); }
async function stopDriver(id) { await driverAction(id, 'stop'); }
async function restartDriver(id) { await driverAction(id, 'restart'); }

async function deleteDriver(id) {
  const drv = state.drivers.find(d => d.id === id);
  const n = drv && drv.tags ? drv.tags.length : 0;
  const ok = await confirmDialog(
    'Supprimer l\'équipement ?',
    `Supprimer le driver « ${id} » et ses ${n} variable(s) ?\nLes widgets liés à ces variables afficheront « Non lié ».`,
    'Supprimer'
  );
  if (!ok) return;
  const res = await fetch(`/api/drivers/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (!res.ok) {
    notify(await apiError(res), 'error');
    return;
  }
  Object.keys(state.tags).forEach(tid => {
    if (state.tags[tid].module_source === id) delete state.tags[tid];
  });
  renderTagTree();
  await renderDriversAndTagDb();
  notify(`Équipement « ${id} » supprimé`);
}

function renderTagCatalogTable() {
  const tbody = document.getElementById('tagCatalogTableBody');
  if (!tbody) return;
  const q = ((document.getElementById('tagDbSearch') && document.getElementById('tagDbSearch').value) || '').trim().toLowerCase();
  const drvFilter = (document.getElementById('tagDbDriverFilter') && document.getElementById('tagDbDriverFilter').value) || '';
  const allowEdit = !!(state.currentUser && state.currentUser.permissions && state.currentUser.permissions.can_manage_drivers_tags);

  const all = Object.values(state.tags)
    .filter(t => (!drvFilter || t.module_source === drvFilter) && (!q || [t.id, t.label, t.module_source, t.protocol_address].some(v => String(v || '').toLowerCase().includes(q))))
    .sort((a, b) => a.id.localeCompare(b.id));

  if (!all.length) {
    tbody.innerHTML = '<tr><td colspan="9" style="text-align:center;color:var(--muted);padding:1.2em;">Aucune variable ne correspond aux critères.</td></tr>';
    return;
  }

  tbody.innerHTML = all.map(t => {
    const qual = t.state ? t.state.quality : 'Uncertain';
    const low = t.alarm_config && t.alarm_config.low !== null && t.alarm_config.low !== undefined ? t.alarm_config.low : '-';
    const high = t.alarm_config && t.alarm_config.high !== null && t.alarm_config.high !== undefined ? t.alarm_config.high : '-';
    const sid = cssSafe(t.id);
    const rowClick = allowEdit ? `onclick="editTagInForm(${jsArg(t.id)})"` : '';
    return `
      <tr id="cat-row-${sid}" class="${allowEdit ? 'clickable-row' : ''}" ${rowClick}>
        <td class="mono"><b>${esc(t.id)}</b></td>
        <td>${esc(t.label)}</td>
        <td>${esc(t.module_source)}</td>
        <td class="mono">${esc(t.protocol_address)}</td>
        <td>${esc(t.data_type)} (${t.writable ? 'RW' : 'RO'})</td>
        <td id="cat-val-${sid}" class="mono" style="color:var(--accent);font-weight:700;">${formatTagValue(t)}</td>
        <td id="cat-q-${sid}"><span class="badge-qual q-${esc(qual)}">${esc(qual)}</span></td>
        <td class="mono">B : ${esc(low)} / H : ${esc(high)}</td>
        <td onclick="event.stopPropagation()">
          ${allowEdit ? `<button class="btn" onclick="editTagInForm(${jsArg(t.id)})">Modifier</button>
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
  const idEl = document.getElementById('drvId');
  const submitBtn = document.getElementById('btnDriverSubmit');
  const protoEl = document.getElementById('drvProto');
  if (!idEl || !submitBtn || !protoEl) return;
  const id = idEl.value.trim();
  const existing = state.drivers.find(d => d.id === id);
  submitBtn.textContent = existing ? 'Mettre à jour' : '+ Déclarer & Démarrer';
  protoEl.disabled = !!existing;
}

/** Ouvre la modale de création d'un nouvel équipement. */
function openNewDriverModal() {
  document.getElementById('drvId').value = '';
  document.getElementById('drvProto').value = 'ModbusTcp';
  document.getElementById('drvEndpoint').value = '';
  document.getElementById('drvUnitId').value = '1';
  document.getElementById('drvPoll').value = '1000';
  refreshDriverFormMode();
  refreshProtocolHints();
  openModal('driver', 'Nouvel équipement');
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
  refreshProtocolHints();
  openModal('driver', `Modifier l'équipement ${d.id}`);
}

async function createNewDriver() {
  const id = document.getElementById('drvId').value.trim();
  const protocol = document.getElementById('drvProto').value;
  const endpoint = document.getElementById('drvEndpoint').value.trim();
  const unit_id = parseInt(document.getElementById('drvUnitId').value || '1', 10);
  const poll_interval_ms = parseInt(document.getElementById('drvPoll').value || '1000', 10);

  if (state.drivers.find(d => d.id === id)) {
    const res = await fetch(`/api/drivers/${encodeURIComponent(id)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ endpoint, unit_id, poll_interval_ms })
    });
    if (!res.ok) {
      notify(`Mise à jour refusée : ${await apiError(res)}`, 'error');
      return;
    }
    closeModal();
    notify(`Équipement « ${id} » mis à jour`);
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

  if (!res.ok) {
    notify(await apiError(res), 'error');
    return;
  }
  closeModal();
  notify(`Équipement « ${id} » déclaré et démarré`);
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
    notify(`${discovered.length} variable(s) de démonstration ajoutée(s)`);
  } else {
    notify(await apiError(res), 'error');
  }
}

/** Ouvre la modale de création d'une nouvelle variable. */
function openNewTagModal() {
  const drvFilter = (document.getElementById('tagDbDriverFilter') && document.getElementById('tagDbDriverFilter').value) || '';
  const defaultDrv = drvFilter || (state.drivers[0] ? state.drivers[0].id : '');
  document.getElementById('tagFormId').value = '';
  document.getElementById('tagFormLabel').value = '';
  document.getElementById('tagFormSource').value = defaultDrv;
  document.getElementById('tagFormAddr').value = '';
  document.getElementById('tagFormType').value = 'Float';
  document.getElementById('tagFormUnit').value = '';
  document.getElementById('tagFormRw').value = 'true';
  document.getElementById('tagFormScale').value = '1.0';
  document.getElementById('tagFormLow').value = '';
  document.getElementById('tagFormHigh').value = '';
  if (document.getElementById('tagFormBoolAlarm')) document.getElementById('tagFormBoolAlarm').value = '';
  document.getElementById('tagFormShelve').value = 'false';
  refreshTagAddressHint();
  openModal('tag', 'Nouvelle variable');
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

  const boolAlarmEl = document.getElementById('tagFormBoolAlarm');
  const boolAlarmVal = boolAlarmEl ? boolAlarmEl.value : '';
  const trigger_on_bool = (data_type === 'Bool')
    ? (boolAlarmVal !== '' ? (boolAlarmVal === 'true') : null)
    : null;

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
      enabled: (data_type === 'Bool')
        ? (trigger_on_bool !== null)
        : (low !== null || high !== null || prevAlarm.low_low != null || prevAlarm.high_high != null),
      low: (data_type === 'Bool') ? null : low,
      high: (data_type === 'Bool') ? null : high,
      trigger_on_bool,
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
    closeModal();
    notify(`Variable « ${saved.id} » enregistrée`);
    renderTagTree();
    renderTagCatalogTable();
    populateHistorianSelect();
    populateScheduleTagSelect();
  } else {
    notify(`Enregistrement refusé : ${await apiError(res)}`, 'error');
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
  if (document.getElementById('tagFormBoolAlarm')) {
    document.getElementById('tagFormBoolAlarm').value = (t.alarm_config && t.alarm_config.trigger_on_bool != null)
      ? String(t.alarm_config.trigger_on_bool)
      : '';
  }
  document.getElementById('tagFormShelve').value = String(!!(t.alarm_config && t.alarm_config.shelving_disabled));
  refreshTagAddressHint();
  openModal('tag', `Modifier ${t.id}`);
}

async function deleteTagById(tagId) {
  const ok = await confirmDialog('Supprimer la variable ?', `Supprimer la variable « ${tagId} » ?`, 'Supprimer');
  if (!ok) return;
  const res = await fetch(`/api/tags/${encodeURIComponent(tagId)}`, { method: 'DELETE' });
  if (res.ok) {
    delete state.tags[tagId];
    renderTagTree();
    renderTagCatalogTable();
    notify(`Variable « ${tagId} » supprimée`);
  } else {
    notify(await apiError(res), 'error');
  }
}

// ============================================================================
// ÉCRAN PLANNINGS : PLANIFICATEUR HORAIRE GTB HEBDOMADAIRE
// ============================================================================

function renderScheduleDaysPicker(selectedDays = [1, 2, 3, 4, 5]) {
  const box = document.getElementById('schedDaysBox');
  if (!box) return;
  box.innerHTML = WEEK_DAYS.map(([n, label]) => `
    <label class="day-chip"><input type="checkbox" value="${n}" ${selectedDays.includes(n) ? 'checked' : ''} /> ${label}</label>
  `).join('');
}

function formatDays(days) {
  const list = Array.isArray(days) ? days : [1, 2, 3, 4, 5, 6, 7];
  if (list.length === 7) return 'Tous les jours';
  if (list.length === 5 && [1, 2, 3, 4, 5].every(d => list.includes(d))) return 'Lun → Ven';
  if (!list.length) return 'Aucun jour';
  return WEEK_DAYS.filter(([n]) => list.includes(n)).map(([, l]) => l).join(', ');
}

function populateScheduleTagSelect(selectedTagId) {
  const sel = document.getElementById('schedTagSelect');
  if (!sel) return;
  const current = selectedTagId || sel.value;
  const writableTags = Object.values(state.tags).filter(t => t.writable);
  sel.innerHTML = writableTags.map(t =>
    `<option value="${esc(t.id)}" ${t.id === current ? 'selected' : ''}>${esc(t.id)} (${esc(t.label)})</option>`
  ).join('');
  sel.onchange = updateScheduleValueInputs;
}

function updateScheduleValueInputs() {
  const sel = document.getElementById('schedTagSelect');
  if (!sel) return;
  const tagId = sel.value;
  const t = state.tags[tagId];
  const isBool = t && t.data_type === 'Bool';
  const isInt = t && t.data_type === 'Int';
  const aIn = document.getElementById('schedActiveVal');
  const iIn = document.getElementById('schedInactiveVal');
  if (!aIn || !iIn) return;
  if (isBool) {
    aIn.type = 'text';
    iIn.type = 'text';
    aIn.placeholder = 'true / false';
    iIn.placeholder = 'true / false';
  } else if (isInt) {
    aIn.type = 'number';
    aIn.step = '1';
    iIn.type = 'number';
    iIn.step = '1';
    aIn.placeholder = '0';
    iIn.placeholder = '0';
  } else {
    aIn.type = 'number';
    aIn.step = '0.5';
    iIn.type = 'number';
    iIn.step = '0.5';
    aIn.placeholder = '21.5';
    iIn.placeholder = '17.5';
  }
}

/** État d'un planning : actif, inactif, ou suspendu par le mode sûr. */
function scheduleStatusBadge(s) {
  if (!s.enabled) return '<span class="badge-qual q-Uncertain">INACTIF</span>';
  if (s.suspended_reason) {
    return `<span class="badge-qual q-Suspended" title="${esc(s.suspended_reason)}">SUSPENDU (mode sûr)</span>`;
  }
  return '<span class="badge-qual q-Good">ACTIF</span>';
}

function renderSchedulesTable() {
  const tbody = document.getElementById('schedulesTableBody');
  if (!tbody) return;
  const q = ((document.getElementById('schedSearchInput') && document.getElementById('schedSearchInput').value) || '').trim().toLowerCase();
  const list = state.schedules.filter(s => !q || [s.name, s.target_tag_id].some(v => String(v || '').toLowerCase().includes(q)));

  if (!list.length) {
    tbody.innerHTML = '<tr><td colspan="8" style="text-align:center;color:var(--muted);padding:1.2em;">Aucun planning configuré.</td></tr>';
    return;
  }

  tbody.innerHTML = list.map(s => `
    <tr class="clickable-row" onclick="editScheduleInForm(${jsArg(s.id)})">
      <td><b>${esc(s.name)}</b></td>
      <td class="mono" style="color:var(--accent);">${esc(s.target_tag_id)}</td>
      <td>${esc(formatDays(s.days))}</td>
      <td class="mono">${esc(s.start_time)} → ${esc(s.end_time)}</td>
      <td class="mono" style="color:var(--run);font-weight:700;">${esc(JSON.stringify(s.active_value))}</td>
      <td class="mono" style="color:var(--muted);">${esc(JSON.stringify(s.inactive_value))}</td>
      <td>${scheduleStatusBadge(s)}</td>
      <td onclick="event.stopPropagation()" style="display:flex;gap:.35em;flex-wrap:wrap;">
        <button class="btn" onclick="sendWriteTag(${jsArg(s.target_tag_id)}, ${jsArg(s.active_value)})">Forcer confort</button>
        <button class="btn" onclick="sendWriteTag(${jsArg(s.target_tag_id)}, ${jsArg(s.inactive_value)})">Forcer réduit</button>
        <button class="btn" onclick="toggleScheduleEnabled(${jsArg(s.id)})">${s.enabled ? 'Désactiver' : 'Activer'}</button>
        <button class="btn btn-danger" onclick="deleteScheduleRule(${jsArg(s.id)})">Suppr.</button>
      </td>
    </tr>
  `).join('');
}

/** Ouvre la modale pour créer un nouveau planning. */
function openNewScheduleModal() {
  editingScheduleId = null;
  populateScheduleTagSelect();
  document.getElementById('schedName').value = '';
  document.getElementById('schedStart').value = '07:00';
  document.getElementById('schedEnd').value = '19:00';
  updateScheduleValueInputs();
  const selTag = state.tags[document.getElementById('schedTagSelect').value];
  if (selTag && selTag.data_type === 'Bool') {
    document.getElementById('schedActiveVal').value = 'true';
    document.getElementById('schedInactiveVal').value = 'false';
  } else {
    document.getElementById('schedActiveVal').value = '21.5';
    document.getElementById('schedInactiveVal').value = '17.5';
  }
  renderScheduleDaysPicker([1, 2, 3, 4, 5]);
  openModal('schedule', 'Nouveau planning');
}

/** Ouvre la modale pour modifier un planning existant. */
function editScheduleInForm(id) {
  const s = state.schedules.find(x => x.id === id);
  if (!s) return;
  editingScheduleId = s.id;
  populateScheduleTagSelect(s.target_tag_id);
  document.getElementById('schedName').value = s.name;
  document.getElementById('schedTagSelect').value = s.target_tag_id;
  updateScheduleValueInputs();
  document.getElementById('schedStart').value = s.start_time;
  document.getElementById('schedEnd').value = s.end_time;
  document.getElementById('schedActiveVal').value = String(s.active_value);
  document.getElementById('schedInactiveVal').value = String(s.inactive_value);
  renderScheduleDaysPicker(Array.isArray(s.days) ? s.days : [1, 2, 3, 4, 5, 6, 7]);
  openModal('schedule', `Modifier le planning ${s.name}`);
}

async function postSchedule(payload) {
  const res = await fetch('/api/schedules', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload)
  });
  if (!res.ok) {
    notify(`Planning refusé : ${await apiError(res)}`, 'error');
    return null;
  }
  const saved = await res.json();
  state.schedules = await getJson('/api/schedules', state.schedules
    .filter(s => s.id !== saved.id).concat(saved).sort((a, b) => a.id.localeCompare(b.id)));
  renderSchedulesTable();
  return saved;
}

async function createScheduleRule() {
  const days = [...document.querySelectorAll('#schedDaysBox input:checked')].map(i => parseInt(i.value, 10));
  if (!days.length) {
    notify('Sélectionnez au moins un jour de la semaine.', 'error');
    return;
  }
  const tagId = document.getElementById('schedTagSelect').value;
  const targetTag = state.tags[tagId];
  const isBool = targetTag && targetTag.data_type === 'Bool';
  const isInt = targetTag && targetTag.data_type === 'Int';
  const rawActive = document.getElementById('schedActiveVal').value.trim();
  const rawInactive = document.getElementById('schedInactiveVal').value.trim();

  let active_value;
  let inactive_value;
  if (isBool) {
    active_value = (rawActive.toLowerCase() === 'true' || rawActive === '1');
    inactive_value = (rawInactive.toLowerCase() === 'true' || rawInactive === '1');
  } else if (isInt) {
    active_value = parseInt(rawActive, 10);
    inactive_value = parseInt(rawInactive, 10);
    if (isNaN(active_value) || isNaN(inactive_value)) {
      notify('Valeur de consigne entière invalide.', 'error');
      return;
    }
  } else {
    active_value = parseFloat(rawActive);
    inactive_value = parseFloat(rawInactive);
    if (isNaN(active_value) || isNaN(inactive_value)) {
      notify('Valeur de consigne numérique invalide.', 'error');
      return;
    }
  }

  const existing = editingScheduleId && state.schedules.find(s => s.id === editingScheduleId);
  const saved = await postSchedule({
    id: editingScheduleId || ('sched-' + Date.now()),
    name: document.getElementById('schedName').value.trim(),
    target_tag_id: tagId,
    start_time: document.getElementById('schedStart').value,
    end_time: document.getElementById('schedEnd').value,
    active_value,
    inactive_value,
    enabled: existing ? existing.enabled : true,
    days
  });
  if (saved) {
    editingScheduleId = null;
    closeModal();
    notify(`Planning « ${saved.name} » enregistré`);
  }
}

async function toggleScheduleEnabled(id) {
  const rule = state.schedules.find(s => s.id === id);
  if (!rule) return;
  await postSchedule(Object.assign({}, rule, { enabled: !rule.enabled }));
}

async function deleteScheduleRule(id) {
  const rule = state.schedules.find(s => s.id === id);
  const ok = await confirmDialog('Supprimer le planning ?', `Supprimer le programme horaire « ${rule ? rule.name : id} » ?`, 'Supprimer');
  if (!ok) return;
  const res = await fetch(`/api/schedules/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (!res.ok) {
    notify(await apiError(res), 'error');
    return;
  }
  state.schedules = state.schedules.filter(s => s.id !== id);
  renderSchedulesTable();
  notify('Planning supprimé');
}

// ============================================================================
// ÉCRANS TENDANCES (HISTORIAN SQLITE) & SYSTÈME (CONTRÔLE, BACKUP, AUDIT)
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
    updatePauseButtonUi();
    const setTxt = (id, v) => { const el = document.getElementById(id); if (el) el.textContent = v; };
    setTxt('sysKpiUptime', `PID ${st.pid} (${st.uptime_seconds}s)`);
    setTxt('sysKpiDrivers', `${st.running_drivers} / ${st.total_drivers}`);
    setTxt('sysKpiCov', st.cov_events_total);
    setTxt('sysKpiFlows', st.active_flows);
    const ret = document.getElementById('histRetentionInfo');
    if (ret) {
      ret.textContent = st.history_retention_days > 0
        ? `Rétention : ${st.history_retention_days} j${st.history_dropped ? ` · ${st.history_dropped} écarté(s)` : ''}`
        : 'Rétention illimitée';
    }
  }

  if (state.activeTab === 'historian') {
    await loadHistorianData();
  }
  if (state.activeTab === 'system') {
    await loadAuditLogs();
  }
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
  const sel = document.getElementById('histTagSelect');
  const tagId = sel && sel.value;
  if (!tagId) return;

  const res = await fetch(`/api/history/${encodeURIComponent(tagId)}?${historianQuery()}`);
  if (!res.ok) {
    notify(await apiError(res), 'error');
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
  chartBox.innerHTML = historianChartSvg(data.points, unit, chartBox.clientWidth || 1100);

  const tbody = document.getElementById('historianTableBody');
  tbody.innerHTML = data.points.slice().reverse().slice(0, 40).map(p => `
    <tr>
      <td class="mono">${esc(new Date(p.timestamp).toLocaleString('fr-FR'))}</td>
      <td class="mono"><b>${esc(data.tag_id)}</b></td>
      <td class="mono" style="color:var(--accent);font-weight:700;">${esc(formatHistoryValue(p.value) + unit)}</td>
      <td><span class="badge-qual q-${esc(p.quality)}">${esc(p.quality)}</span></td>
    </tr>
  `).join('');
}

/** Valeur d'historique lisible (nombre arrondi, booléen en clair, texte sans guillemets). */
function formatHistoryValue(v) {
  if (typeof v === 'number') return Number.isInteger(v) ? String(v) : v.toFixed(2);
  if (typeof v === 'boolean') return v ? 'Vrai (1)' : 'Faux (0)';
  return String(v);
}

/** Courbe d'historique en SVG : tracé en escalier, grille, valeurs en ordonnée et heures en abscisse. */
function historianChartSvg(points, unit, width) {
  const pts = points
    .map(p => ({ t: new Date(p.timestamp).getTime(), v: typeof p.value === 'number' ? p.value : (p.value ? 1 : 0) }))
    .filter(p => Number.isFinite(p.t) && Number.isFinite(p.v))
    .sort((a, b) => a.t - b.t);
  if (pts.length < 2) {
    return '<p class="hint" style="padding:.8em;">Pas assez d\'échantillons sur cette période pour tracer une courbe.</p>';
  }
  const w = Math.max(320, Math.round(width - 20)), h = 185;
  const left = 64, right = 12, top = 10, bottom = 24;
  const t0 = pts[0].t, t1 = pts[pts.length - 1].t;
  let vMin = Math.min(...pts.map(p => p.v)), vMax = Math.max(...pts.map(p => p.v));
  if (vMax - vMin < 1e-9) { vMin -= 1; vMax += 1; }
  const pad = (vMax - vMin) * 0.08;
  vMin -= pad;
  vMax += pad;
  const x = t => left + ((t - t0) / ((t1 - t0) || 1)) * (w - left - right);
  const y = v => top + (1 - (v - vMin) / (vMax - vMin)) * (h - top - bottom);
  let d = `M ${x(pts[0].t).toFixed(1)} ${y(pts[0].v).toFixed(1)}`;
  for (let i = 1; i < pts.length; i++) {
    d += ` H ${x(pts[i].t).toFixed(1)} V ${y(pts[i].v).toFixed(1)}`;
  }
  const sameDay = new Date(t0).toDateString() === new Date(t1).toDateString();
  const fmtT = t => new Date(t).toLocaleString('fr-FR', sameDay
    ? { hour: '2-digit', minute: '2-digit', second: '2-digit' }
    : { day: '2-digit', month: '2-digit', hour: '2-digit', minute: '2-digit' });
  const fmtV = v => (Math.abs(vMax - vMin) < 10 ? v.toFixed(2) : v.toFixed(1)) + unit;
  let grid = '';
  for (let k = 0; k <= 4; k++) {
    const yy = y(vMin + (k / 4) * (vMax - vMin)).toFixed(1);
    grid += `<line x1="${left}" x2="${w - right}" y1="${yy}" y2="${yy}" stroke="rgba(120,160,220,.14)" stroke-width="1"/>`
      + `<text x="${left - 6}" y="${yy}" fill="#8ea2c0" font-size="11" text-anchor="end" dominant-baseline="middle">${esc(fmtV(vMin + (k / 4) * (vMax - vMin)))}</text>`;
  }
  for (let k = 0; k <= 4; k++) {
    const t = t0 + (k / 4) * (t1 - t0), xx = x(t).toFixed(1);
    const anchor = k === 0 ? 'start' : (k === 4 ? 'end' : 'middle');
    grid += `<line x1="${xx}" x2="${xx}" y1="${top}" y2="${h - bottom}" stroke="rgba(120,160,220,.1)" stroke-width="1"/>`
      + `<text x="${xx}" y="${h - 6}" fill="#8ea2c0" font-size="11" text-anchor="${anchor}">${esc(fmtT(t))}</text>`;
  }
  return `<svg width="100%" height="${h}" viewBox="0 0 ${w} ${h}" role="img" aria-label="Courbe d'historique">
      ${grid}
      <path d="${d}" fill="none" stroke="#38bdf8" stroke-width="2" stroke-linejoin="round"/>
    </svg>`;
}

/** Télécharge un contenu via un lien temporaire (l'API exige l'en-tête d'authentification). */
async function downloadAuthenticated(url, fallbackName) {
  const res = await fetch(url);
  if (!res.ok) {
    notify(await apiError(res), 'error');
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
  const ok = await confirmDialog(
    'Purger l\'historique ?',
    'Purger toutes les mesures historiques de la base SQLite ?',
    'Purger'
  );
  if (!ok) return;
  const res = await fetch('/api/history/purge', { method: 'POST' });
  if (!res.ok) notify(await apiError(res), 'error');
  else notify('Historique SQLite purgé');
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
      <td class="mono">${esc(new Date(l.timestamp).toLocaleString('fr-FR'))}</td>
      <td><b>${esc(l.actor)}</b></td>
      <td><span class="badge-rw">${esc(l.category)}</span></td>
      <td class="mono">${esc(l.action)}</td>
      <td class="mono" style="color:var(--accent);">${esc(l.target)}</td>
      <td>${esc(l.details)}</td>
    </tr>
  `).join('');
}

async function exportProjectBackup() {
  const res = await fetch('/api/system/export');
  if (!res.ok) {
    notify(await apiError(res), 'error');
    return;
  }
  const bundle = await res.json();
  const blob = new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  const d = new Date();
  const p2 = n => String(n).padStart(2, '0');
  const stamp = `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}_${p2(d.getHours())}-${p2(d.getMinutes())}-${p2(d.getSeconds())}`;
  a.download = `apex_supervision_projet_${stamp}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  notify('Archive du projet exportée');
}

async function importProjectBackup(event) {
  const file = event.target.files[0];
  event.target.value = '';
  if (!file) return;
  let bundle;
  try {
    bundle = JSON.parse(await file.text());
  } catch (_) {
    notify('Fichier JSON invalide.', 'error');
    return;
  }
  const body = JSON.stringify(bundle);
  const size = new Blob([body]).size;
  if (size > PROJECT_IMPORT_MAX_BYTES) {
    notify(`Import refusé : ${payloadTooLargeMessage(PROJECT_IMPORT_MAX_BYTES, size)}`, 'error');
    return;
  }
  const res = await fetch('/api/system/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body
  });
  if (!res.ok) {
    notify(`Import refusé : ${await apiError(res, PROJECT_IMPORT_MAX_BYTES)}`, 'error');
    return;
  }
  const r = await res.json();
  const errs = (r.errors || []);
  notify(`Archive restaurée : ${r.drivers} driver(s), ${r.tags} variable(s), ${r.dashboards} synoptique(s), ${r.flows} flux, ${r.schedules} planning(s)`
    + (errs.length ? ` (${errs.length} avertissement(s))` : ''));
  await bootstrapData();
}

// ============================================================================
// COMMANDES COMMUNES & NAVIGATION
// ============================================================================

/** Envoie une consigne ; renvoie vrai si le serveur l'a acceptée. */
async function sendWriteTag(tagId, value) {
  if (!tagId) return false;
  if (!canWrite()) {
    notify('Votre profil ne permet pas l\'envoi de consignes.', 'error');
    return false;
  }
  const res = await fetch(`/api/tags/${encodeURIComponent(tagId)}/write`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ value })
  });
  if (!res.ok) {
    notify(`Refus d'écriture : ${await apiError(res)}`, 'error');
  }
  return res.ok;
}

/**
 * Fenêtre de confirmation de l'IHM (remplace confirm()) : renvoie une promesse résolue à vrai
 * sur « Confirmer », à faux sur « Annuler », Échap ou clic hors de la fenêtre.
 */
function confirmDialog(title, message, okLabel = 'Confirmer') {
  return new Promise(resolve => {
    const overlay = document.createElement('div');
    overlay.className = 'auth-modal confirm-overlay';
    overlay.style.display = 'grid';
    overlay.innerHTML = `
      <div class="card-box confirm-box" role="alertdialog" aria-modal="true" aria-labelledby="confirmTitle">
        <h3 id="confirmTitle">${esc(title)}</h3>
        <p class="confirm-msg">${esc(message)}</p>
        <div class="confirm-actions">
          <button class="btn" data-r="0">Annuler</button>
          <button class="btn primary" data-r="1">${esc(okLabel)}</button>
        </div>
      </div>`;
    const close = ok => {
      document.removeEventListener('keydown', onKey, true);
      overlay.remove();
      resolve(ok);
    };
    const onKey = e => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        close(false);
      }
    };
    overlay.addEventListener('click', e => {
      if (e.target === overlay) close(false);
      const r = e.target.closest && e.target.closest('button[data-r]');
      if (r) close(r.dataset.r === '1');
    });
    document.addEventListener('keydown', onKey, true);
    document.body.appendChild(overlay);
    overlay.querySelector('button[data-r="1"]').focus();
  });
}

function switchTab(tab, btn) {
  const perms = (state.currentUser && state.currentUser.permissions) || {};
  if (state.currentUser && state.currentUser.role === 'Viewer' && tab !== 'synoptic' && tab !== 'alarms') {
    return;
  }
  if (tab === 'users' && !perms.can_manage_users) {
    return;
  }
  if (tab === 'system' && !perms.can_manage_system && !perms.can_view_historian) {
    return;
  }

  if (tab !== state.activeTab) cancelCommand(false);
  state.activeTab = tab;
  document.body.classList.remove('nav-open');
  document.body.classList.toggle('studio-panel-off', tab !== 'synoptic' && tab !== 'flow');

  const navBtn = btn || document.getElementById(`navBtn-${tab}`);
  document.querySelectorAll('#mainNavTabs .tab-btn').forEach(b => {
    const isTarget = b === navBtn;
    b.classList.toggle('active', isTarget);
    b.classList.toggle('on', isTarget);
  });

  const synScreen = document.getElementById('screen-synoptic');
  if (synScreen) {
    synScreen.classList.toggle('on', tab === 'synoptic');
  }

  ['flow', 'alarms', 'tagdb', 'schedules', 'historian', 'users', 'system'].forEach(t => {
    const el = document.getElementById(`tab-${t}`);
    if (el) el.classList.toggle('on', t === tab);
  });

  if (tab === 'synoptic') renderDashboard();
  if (tab === 'flow') renderFlowEditor();
  if (tab === 'tagdb') renderDriversAndTagDb();
  if (tab === 'schedules') renderSchedulesTable();
  if (tab === 'historian' || tab === 'system') loadSystemAndHistorianTab();
  if (tab === 'users') loadUsersList();
  if (tab === 'alarms') {
    renderAlarmsTable();
    loadAlarmJournal();
  }
}

function cssSafe(str) {
  return String(str).replace(/[^a-zA-Z0-9_-]/g, '_');
}

// ============================================================================
// CONNEXION, COMPTE UTILISATEUR & INTERFACE ADAPTATIVE RBAC
// ============================================================================

function applyUserPermissionsToUi() {
  const user = state.currentUser;
  if (!user) return;

  const role = user.role; // 'Admin' | 'Supervisor' | 'Viewer'
  const perms = user.permissions || {};

  const profileBtn = document.getElementById('userProfileBtn');
  const iconEl = document.getElementById('userRoleIcon');
  const nameEl = document.getElementById('userDisplayName');
  const badgeEl = document.getElementById('userRoleBadge');
  const roleSummaryEl = document.getElementById('accountRoleHint');

  if (nameEl) nameEl.textContent = user.username;
  const initials = String(user.display_name || user.username || 'U')
    .trim()
    .split(/[\s._-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map(s => s[0].toUpperCase())
    .join('') || 'U';
  if (iconEl) iconEl.textContent = initials.slice(0, 2);
  if (profileBtn) {
    profileBtn.title = `${user.display_name || user.username} (${user.role_label || role})`;
  }

  if (role === 'Admin') {
    if (badgeEl) {
      badgeEl.textContent = 'ADMIN';
      badgeEl.style.background = 'rgba(239,68,68,0.22)';
      badgeEl.style.color = '#f87171';
    }
    if (roleSummaryEl) roleSummaryEl.textContent = 'Administrateur — Tous les droits (Système, Utilisateurs, Studio, Conduite)';
  } else if (role === 'Supervisor') {
    if (badgeEl) {
      badgeEl.textContent = 'SUPERVISEUR';
      badgeEl.style.background = 'rgba(56,189,248,0.22)';
      badgeEl.style.color = '#38bdf8';
    }
    if (roleSummaryEl) roleSummaryEl.textContent = 'Superviseur — Conduite, Studio, Équipements & Automatismes (hors Système)';
  } else {
    if (badgeEl) {
      badgeEl.textContent = 'VISUALISATION';
      badgeEl.style.background = 'rgba(34,197,94,0.22)';
      badgeEl.style.color = '#86efac';
    }
    if (roleSummaryEl) roleSummaryEl.textContent = 'Visualisation — Synoptiques en lecture seule et acquittement des alarmes';
  }

  const showEl = (id, visible, displayType = 'flex') => {
    const el = document.getElementById(id);
    if (el) el.style.display = visible ? displayType : 'none';
  };

  showEl('navBtn-flow', perms.can_manage_flows, 'flex');
  showEl('navBtn-alarms', perms.can_view_alarms, 'flex');
  showEl('navBtn-tagdb', perms.can_manage_drivers_tags, 'flex');
  showEl('navBtn-schedules', perms.can_manage_schedules, 'flex');
  showEl('navBtn-historian', perms.can_view_historian, 'flex');
  showEl('navBtn-users', perms.can_manage_users, 'flex');
  showEl('navBtn-system', perms.can_manage_system || perms.can_view_historian, 'flex');

  // Masquer les en-têtes de rubriques vides selon le profil
  showEl('navGrpAuto', !!(perms.can_manage_flows || perms.can_manage_schedules), 'block');
  showEl('navGrpConfig', !!(perms.can_manage_drivers_tags || perms.can_manage_users || perms.can_manage_system || perms.can_view_historian), 'block');

  // Contrôles d'alimentation & Paramètres Système (Admin uniquement), Journal d'audit selon droits actuels
  showEl('adminPowerControls', perms.can_manage_system, 'flex');
  showEl('systemAdminCard', perms.can_manage_system, 'flex');
  showEl('auditTrailCard', !!(perms.can_view_historian || perms.can_manage_system), 'flex');
  showEl('btnPurgeHistorian', perms.can_manage_system, 'inline-flex');

  // Contrôles Studio / Édition (Admin & Superviseur)
  showEl('modeSwitchContainer', perms.can_edit_studio, 'inline-flex');
  showEl('synopticStudioActions', perms.can_edit_studio, 'flex');
  showEl('synopticSaveGroup', perms.can_edit_studio, 'flex');
  showEl('btnBannerAckAll', perms.can_manage_alarms, 'inline-flex');
  showEl('btnAlarmsAckAll', perms.can_manage_alarms, 'inline-flex');
  showEl('btnAlarmsClearNormal', perms.can_manage_alarms, 'inline-flex');
  showEl('btnNewTag', perms.can_manage_drivers_tags, 'inline-flex');

  document.body.classList.toggle('viewer-only-mode', role === 'Viewer');
  if (!perms.can_edit_studio) setMode('run');
  else setMode(state.mode);

  const tabAllowed = {
    synoptic: true,
    flow: perms.can_manage_flows,
    alarms: perms.can_view_alarms,
    tagdb: perms.can_manage_drivers_tags,
    schedules: perms.can_manage_schedules,
    historian: perms.can_view_historian,
    users: perms.can_manage_users,
    system: perms.can_manage_system || perms.can_view_historian
  };
  if (!tabAllowed[state.activeTab]) {
    switchTab('synoptic', document.getElementById('navBtn-synoptic'));
  }
}

/** Affiche la fenêtre de connexion. Sans session valide, elle ne peut pas être fermée. */
function showLoginModal(message) {
  const modal = document.getElementById('authModal');
  const authenticated = !!state.currentUser;
  modal.style.display = 'grid';
  modal.classList.toggle('locked', !authenticated);
  document.getElementById('authModalClose').style.display = authenticated ? 'inline-flex' : 'none';
  document.getElementById('accountSection').style.display = authenticated ? 'block' : 'none';
  document.getElementById('authModalMsg').style.color = '';
  document.getElementById('authModalMsg').textContent = message
    || (authenticated ? 'Gérez votre mot de passe ou changez de session utilisateur.' : 'Identifiez-vous pour accéder à la supervision.');
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
    const msgEl = document.getElementById('authModalMsg');
    msgEl.style.color = '#f87171';
    msgEl.textContent = `Connexion refusée : ${await apiError(res)}`;
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
    notify('Les deux nouveaux mots de passe ne correspondent pas.', 'error');
    return;
  }
  const res = await fetch('/api/auth/password', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ current_password, new_password })
  });
  if (res.ok) {
    ['pwdCurrent', 'pwdNew', 'pwdNew2'].forEach(id => { document.getElementById(id).value = ''; });
    notify('Mot de passe modifié. Vos autres sessions ouvertes ont été fermées.');
  } else {
    notify(`Changement refusé : ${await apiError(res)}`, 'error');
  }
}

async function loadUsersList() {
  if (!isAdmin()) return;
  const res = await fetch('/api/users');
  if (!res.ok) return;
  state.usersList = await res.json();
  renderUsersTable();
}

function renderUsersTable() {
  const tbody = document.getElementById('usersTableBody');
  if (!tbody) return;
  const q = ((document.getElementById('usersSearchInput') && document.getElementById('usersSearchInput').value) || '').trim().toLowerCase();
  const list = (state.usersList || []).filter(u => !q
    || [u.username, u.display_name, u.role, u.role_label, u.department].some(v => String(v || '').toLowerCase().includes(q)));

  if (!list.length) {
    tbody.innerHTML = '<tr><td colspan="7" style="text-align:center;color:var(--muted);padding:1.2em;">Aucun compte utilisateur trouvé.</td></tr>';
    return;
  }

  tbody.innerHTML = list.map(u => {
    const badgeColor = u.role === 'Admin'
      ? 'background:rgba(239,68,68,0.2);color:#f87171;'
      : u.role === 'Supervisor'
        ? 'background:rgba(56,189,248,0.2);color:#38bdf8;'
        : 'background:rgba(34,197,94,0.2);color:#86efac;';
    const canDelete = u.username !== 'admin' && (!state.currentUser || u.username !== state.currentUser.username);
    return `
      <tr class="clickable-row" onclick="fillUserForm(${jsArg(u.username)})">
        <td class="mono" style="color:var(--accent);"><b>${esc(u.username)}</b></td>
        <td><b>${esc(u.display_name)}</b></td>
        <td><span class="badge-rw" style="${badgeColor}">${esc(u.role_label)} (${esc(u.role)})</span></td>
        <td>${esc(u.department || '--')}</td>
        <td><span class="badge-qual ${u.enabled ? 'q-Good' : 'q-BadDisconnect'}">${u.enabled ? 'ACTIF' : 'DÉSACTIVÉ'}</span></td>
        <td class="mono" style="font-size:.85em;">${u.last_login_at ? esc(new Date(u.last_login_at).toLocaleString('fr-FR')) : 'Jamais'}</td>
        <td onclick="event.stopPropagation()" style="display:flex;gap:.35em;">
          <button class="btn" onclick="fillUserForm(${jsArg(u.username)})">Modifier</button>
          ${canDelete ? `<button class="btn btn-danger" onclick="deleteUserAccount(${jsArg(u.username)})">Suppr.</button>` : ''}
        </td>
      </tr>
    `;
  }).join('');
}

/** Ouvre la modale de création d'un nouvel utilisateur. */
function openNewUserModal() {
  document.getElementById('usrFormLogin').value = '';
  document.getElementById('usrFormName').value = '';
  document.getElementById('usrFormRole').value = 'Viewer';
  document.getElementById('usrFormDept').value = '';
  document.getElementById('usrFormPass').value = '';
  document.getElementById('usrFormEnabled').value = 'true';
  openModal('user', 'Nouvel utilisateur');
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
  openModal('user', `Modifier le compte ${u.username}`);
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
    closeModal();
    await loadUsersList();
    notify(`Compte « ${payload.username} » (${payload.role}) enregistré`);
  } else {
    notify(`Erreur : ${await apiError(res)}`, 'error');
  }
}

async function deleteUserAccount(username) {
  const ok = await confirmDialog('Supprimer le compte ?', `Supprimer le compte utilisateur « ${username} » ?`, 'Supprimer');
  if (!ok) return;
  const res = await fetch(`/api/users/${encodeURIComponent(username)}`, { method: 'DELETE' });
  if (res.ok) {
    await loadUsersList();
    notify(`Compte « ${username} » supprimé`);
  } else {
    notify(await apiError(res), 'error');
  }
}

// ============================================================================
// BIBLIOTHÈQUES GÉNÉRIQUES DE SYMBOLES INDUSTRIELS (SVG / P&ID / DRAW.IO / JSON)
// ============================================================================

function renderCustomSymbolsPalette() {
  const container = document.getElementById('customSymbolsPalette');
  const countEl = document.getElementById('customSymCount');
  if (!container) return;
  if (countEl) countEl.textContent = String(state.symbols.length);

  container.innerHTML = state.symbols.map(sym => `
    <div class="palette-item" draggable="true"
         ondragstart="onCustomSymbolDrag(event, ${jsArg(sym.id)})"
         title="${esc(sym.name)} (${esc(sym.standard)})"
         style="display:flex;align-items:center;justify-content:space-between;gap:4px;">
      <span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${esc(sym.name)}</span>
      <span style="font-size:9px;color:#38bdf8;cursor:pointer;" onclick="event.stopPropagation();deleteCustomSymbol(${jsArg(sym.id)})" title="Supprimer">×</span>
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
    notify(`Pack industriel « ${packId} » installé dans la palette Studio`);
  } else {
    notify(`Erreur : ${await apiError(res)}`, 'error');
  }
}

async function handleSymbolFileUpload(event) {
  const file = event.target.files[0];
  if (!file) return;
  if (file.size > SYMBOL_IMPORT_MAX_BYTES / 2) {
    notify(`Fichier trop volumineux (${formatMegabytes(SYMBOL_IMPORT_MAX_BYTES / 2)} maximum).`, 'error');
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
  const body = JSON.stringify(payload);
  const size = new Blob([body]).size;
  if (size > SYMBOL_IMPORT_MAX_BYTES) {
    notify(`Erreur d'import : ${payloadTooLargeMessage(SYMBOL_IMPORT_MAX_BYTES, size)}`, 'error');
    return;
  }
  const res = await fetch('/api/symbols/import', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body
  });
  if (res.ok) {
    const imported = await res.json();
    state.symbols = await getJson('/api/symbols', state.symbols);
    renderCustomSymbolsPalette();
    closeSymbolLibraryModal();
    notify(`${imported.length} symbole(s) industriel(s) importé(s) dans la palette Studio`);
  } else {
    notify(`Erreur d'import : ${await apiError(res, SYMBOL_IMPORT_MAX_BYTES)}`, 'error');
  }
}

async function deleteCustomSymbol(id) {
  const ok = await confirmDialog('Supprimer le symbole ?', 'Supprimer ce symbole de la bibliothèque ?', 'Supprimer');
  if (!ok) return;
  const res = await fetch(`/api/symbols/${encodeURIComponent(id)}`, { method: 'DELETE' });
  if (res.ok) {
    state.symbols = state.symbols.filter(s => s.id !== id);
    renderCustomSymbolsPalette();
    notify('Symbole supprimé');
  } else {
    notify(await apiError(res), 'error');
  }
}
