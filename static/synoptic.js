// ============================================================================
// Apex Supervision — Synoptiques : arborescence des variables, rendu des vues,
// édition (mode Studio) et commandes en deux temps.
// Chargé avant app.js ; partage son état global (state, esc, jsArg, getJson…).
// ============================================================================

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
    grp.innerHTML = `<div class="tree-group-title">${esc(grpName)}</div>`;

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
    <p style="color:#8b9bb4;font-size:11px;">${tag.writable ? 'Variable pilotable (sélectionnez un widget synoptique pour armer une commande en deux temps).' : 'Variable en lecture seule (Capteur / Mesure).'}</p>
  `;
}

function parseInputForTag(dataType, rawStr) {
  if (dataType === 'Bool') {
    const s = String(rawStr).toLowerCase().trim();
    if (s === 'true' || s === '1' || s === 'on' || s === 'marche') {
      return true;
    }
    if (s === 'false' || s === '0' || s === 'off' || s === 'arret' || s === 'arrêt') {
      return false;
    }
    if (typeof notify === 'function') {
      notify(`Valeur booléenne invalide : « ${s} ». Valeurs acceptées : 1/0, on/off, marche/arret, true/false.`, 'error');
    }
    return null;
  }
  if (dataType === 'Float') {
    const v = parseFloat(rawStr);
    if (!Number.isFinite(v)) {
      if (typeof notify === 'function') notify('Valeur flottante invalide.', 'error');
      return null;
    }
    return v;
  }
  if (dataType === 'Int') {
    const v = parseFloat(rawStr);
    if (!Number.isFinite(v)) {
      if (typeof notify === 'function') notify('Valeur entière invalide.', 'error');
      return null;
    }
    return Math.round(v);
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
  cancelCommand(false);
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
    // Écran cible par défaut d'une nouvelle vue (réglable dans le Studio)
    canvas_width: 1920,
    canvas_height: 1080,
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
    synFit();
    renderDashboard();
  } else {
    notify(await apiError(res), 'error');
  }
}

function duplicateSelectedWidget() {
  const dash = getActiveDashboard();
  if (!dash || !state.selectedWidgetId) return;
  const orig = dash.widgets.find(w => w.id === state.selectedWidgetId);
  if (!orig) return;
  const copy = JSON.parse(JSON.stringify(orig));
  copy.id = 'w-' + Date.now();
  copy.x += (dash.grid_size || 20);
  copy.y += (dash.grid_size || 20);
  dash.widgets.push(copy);
  state.selectedWidgetId = copy.id;
  renderDashboard();
  renderWidgetInspector(copy);
}

async function deleteCurrentDashboard() {
  if (state.dashboards.length <= 1) {
    notify('Impossible de supprimer le dernier synoptique.', 'error');
    return;
  }
  const dash = getActiveDashboard();
  if (!dash || !(await confirmDialog('Supprimer ce synoptique ?', dash.name, 'Supprimer'))) return;
  const res = await fetch(`/api/dashboards/${encodeURIComponent(dash.id)}`, { method: 'DELETE' });
  if (!res.ok) {
    notify(await apiError(res), 'error');
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
    notify(`Synoptique « ${dash.name} » enregistré`);
  } else {
    notify(`Enregistrement refusé : ${await apiError(res)}`, 'error');
  }
}



// ============================================================================
// FENÊTRE DU SYNOPTIQUE : ÉCRAN CIBLE MIS À L'ÉCHELLE
// ============================================================================
// Une vue est dessinée sur un écran cible (canvas_width × canvas_height, en px). Cette fenêtre
// garde ses dimensions et est mise à l'échelle pour tenir dans l'espace disponible : en
// exploitation elle remplit toujours l'écran ; en Studio on peut zoomer pour travailler fin.

const SYN_MIN_WIDTH = 1280;
const SYN_MIN_HEIGHT = 720;
const SYN_SIZES = [[1280, 720], [1366, 768], [1600, 900], [1920, 1080], [2560, 1440], [1280, 1024]];
const synView = { zoom: 1, fitted: true, observer: null };

/** Taille de l'écran cible : celle de la vue, sinon déduite de son contenu (vues antérieures). */
function synCanvasSize(dash) {
  if (dash && dash.canvas_width && dash.canvas_height) return [dash.canvas_width, dash.canvas_height];
  let w = SYN_MIN_WIDTH, h = SYN_MIN_HEIGHT;
  (dash ? dash.widgets : []).forEach(x => {
    w = Math.max(w, x.x + x.width + 40);
    h = Math.max(h, x.y + x.height + 40);
  });
  return [Math.ceil(w / 16) * 16, Math.ceil(h / 16) * 16];
}

/** Place et met à l'échelle la fenêtre dans la zone du synoptique. */
function fitSynopticToScreen() {
  const stage = document.getElementById('synopticStage');
  const frame = document.getElementById('synopticSurface');
  if (!stage || !frame) return;
  if (!synView.observer && window.ResizeObserver) {
    synView.observer = new ResizeObserver(() => fitSynopticToScreen());
    synView.observer.observe(stage);
  }
  if (!stage.clientWidth || !stage.clientHeight) return;
  const [W, H] = synCanvasSize(getActiveDashboard());
  frame.style.width = `${W}px`;
  frame.style.height = `${H}px`;
  const studio = state.mode === 'studio';
  const pad = studio ? 32 : 0;
  if (synView.fitted || !studio) {
    synView.fitted = true;
    synView.zoom = Math.max(0.05, Math.min((stage.clientWidth - 2 * pad) / W, (stage.clientHeight - 2 * pad) / H));
  }
  const z = synView.zoom, fw = W * z, fh = H * z;
  const left = Math.max(pad, (stage.clientWidth - fw) / 2);
  const top = Math.max(pad, (stage.clientHeight - fh) / 2);
  frame.style.transform = `translate(${left}px, ${top}px) scale(${z})`;
  // Zoom manuel : la zone défile ; un repère réserve la place de la fenêtre agrandie
  let spacer = document.getElementById('synopticSpacer');
  if (!spacer) {
    spacer = document.createElement('div');
    spacer.id = 'synopticSpacer';
    spacer.style.cssText = 'position:absolute;width:1px;height:1px;pointer-events:none;';
    stage.appendChild(spacer);
  }
  spacer.style.left = `${left + fw + pad - 1}px`;
  spacer.style.top = `${top + fh + pad - 1}px`;
  const zl = document.getElementById('synZoomLabel');
  if (zl) zl.textContent = synView.fitted ? 'Ajusté' : `${Math.round(z * 100)} %`;
  const fl = document.getElementById('synFrameLabel');
  if (fl) fl.textContent = `Écran ${W} × ${H}`;
  const sizeSel = document.getElementById('synCanvasSize');
  if (sizeSel) {
    const dash = getActiveDashboard();
    sizeSel.value = dash && dash.canvas_width ? `${dash.canvas_width}x${dash.canvas_height}` : 'auto';
  }
}

function synZoom(delta) {
  synView.fitted = false;
  synView.zoom = Math.max(0.1, Math.min(3, +(synView.zoom + delta).toFixed(2)));
  fitSynopticToScreen();
}

function synFit() {
  synView.fitted = true;
  fitSynopticToScreen();
}

/** Studio : taille de l'écran cible de la vue (« auto » = déduite du contenu). */
function setSynopticCanvasSize(value) {
  const dash = getActiveDashboard();
  if (!dash) return;
  if (value === 'auto') {
    dash.canvas_width = null;
    dash.canvas_height = null;
  } else {
    const [w, h] = value.split('x').map(Number);
    dash.canvas_width = w;
    dash.canvas_height = h;
  }
  synFit();
}

/** Grille d'aimantation du Studio (pas de la vue, 20 px par défaut). */
function synSnap(v) {
  const box = document.getElementById('synSnap');
  if (box && !box.checked) return Math.round(v);
  const dash = getActiveDashboard();
  const g = (dash && dash.grid_size) || 20;
  return Math.round(v / g) * g;
}

// ============================================================================
// RENDU DES ÉLÉMENTS
// ============================================================================

function renderDashboard() {
  const container = document.getElementById('widgetsContainer');
  if (!container) return;
  const dash = getActiveDashboard();
  const frag = document.createDocumentFragment();
  if (dash) dash.widgets.forEach(w => frag.appendChild(buildWidgetCard(w)));
  container.innerHTML = '';
  container.appendChild(frag);
  fitSynopticToScreen();
}

const SYN_HANDLES = ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w'];

/** Élément du synoptique : symbole animé, libellé ; poignées et cotes en Studio. */
function buildWidgetCard(w) {
  const card = document.createElement('div');
  card.className = 'scada-widget syn-el' + (state.selectedWidgetId === w.id ? ' selected' : '');
  if (state.armedCommand && state.armedCommand.widgetId === w.id) card.classList.add('cmd-armed');
  card.id = `widget-${cssSafe(w.id)}`;
  card.dataset.widgetId = w.id;
  placeWidgetCard(card, w);

  const body = document.createElement('div');
  body.className = 'widget-body';
  body.id = `widget-body-${cssSafe(w.id)}`;
  renderWidgetBody(body, w);

  const label = document.createElement('div');
  label.className = 'syn-label';
  label.textContent = w.title || '';

  card.appendChild(body);
  card.appendChild(label);
  SYN_HANDLES.forEach(h => {
    const hd = document.createElement('span');
    hd.className = 'syn-handle';
    hd.dataset.h = h;
    card.appendChild(hd);
  });
  const dim = document.createElement('span');
  dim.className = 'syn-dim';
  card.appendChild(dim);

  card.addEventListener('mousedown', e => startWidgetEdit(e, w, card));
  card.ondragover = (e) => {
    if (state.mode !== 'studio') return;
    e.preventDefault();
    e.stopPropagation();
    card.classList.add('drop-hover');
  };
  card.ondragleave = () => card.classList.remove('drop-hover');
  card.ondrop = (e) => {
    card.classList.remove('drop-hover');
    if (state.mode !== 'studio') return;
    let data;
    try { data = JSON.parse(e.dataTransfer.getData('text/plain')); } catch (_) { return; }
    // Une variable lâchée sur un élément le relie à cette variable
    if (data && data.kind === 'tag') {
      e.preventDefault();
      e.stopPropagation();
      w.bound_tag_id = data.tag_id;
      if (state.tags[data.tag_id]) w.title = state.tags[data.tag_id].label;
      state.selectedWidgetId = w.id;
      renderDashboard();
      renderWidgetInspector(w);
    }
  };
  return card;
}

function placeWidgetCard(card, w) {
  card.style.left = `${w.x}px`;
  card.style.top = `${w.y}px`;
  card.style.width = `${w.width}px`;
  card.style.height = `${w.height}px`;
}

// ============================================================================
// ÉDITION (STUDIO) : SÉLECTION, DÉPLACEMENT, REDIMENSIONNEMENT
// ============================================================================

const SYN_MIN_SIZE = 16;

function startWidgetEdit(e, w, card) {
  if (state.mode !== 'studio' || e.button !== 0) return;
  // Les champs d'un élément (consigne…) restent utilisables sans déclencher de déplacement
  if (e.target.closest('input, select, textarea')) return;
  e.preventDefault();
  e.stopPropagation();
  if (state.selectedWidgetId !== w.id) {
    state.selectedWidgetId = w.id;
    document.querySelectorAll('.syn-el.selected').forEach(n => n.classList.remove('selected'));
    card.classList.add('selected');
    renderWidgetInspector(w);
  }
  const handle = e.target.dataset ? e.target.dataset.h : undefined;
  const dash = getActiveDashboard();
  // Écran « selon le contenu » : la fenêtre s'agrandit avec les éléments, seules les bornes 0 s'appliquent
  const fixed = !!(dash && dash.canvas_width && dash.canvas_height);
  const [W, H] = fixed ? synCanvasSize(dash) : [Infinity, Infinity];
  const sx = e.clientX, sy = e.clientY;
  const o = { x: w.x, y: w.y, width: w.width, height: w.height };
  const dim = card.querySelector('.syn-dim');
  card.classList.add('moving');

  function onMove(ev) {
    const dx = (ev.clientX - sx) / synView.zoom, dy = (ev.clientY - sy) / synView.zoom;
    if (!handle) {
      // Déplacement : l'élément reste dans l'écran cible
      w.x = Math.max(0, Math.min(Math.max(0, W - w.width), synSnap(o.x + dx)));
      w.y = Math.max(0, Math.min(Math.max(0, H - w.height), synSnap(o.y + dy)));
    } else {
      // Redimensionnement : le bord opposé reste fixe, la taille est bornée par l'écran cible
      const clamp = (v, max) => Math.max(SYN_MIN_SIZE, Math.min(max, v));
      if (handle.includes('e')) w.width = clamp(synSnap(o.width + dx), W - o.x);
      if (handle.includes('s')) w.height = clamp(synSnap(o.height + dy), H - o.y);
      if (handle.includes('w')) { w.width = clamp(synSnap(o.width - dx), o.x + o.width); w.x = o.x + o.width - w.width; }
      if (handle.includes('n')) { w.height = clamp(synSnap(o.height - dy), o.y + o.height); w.y = o.y + o.height - w.height; }
    }
    placeWidgetCard(card, w);
    dim.textContent = handle ? `${w.width} × ${w.height}` : `x ${w.x} · y ${w.y}`;
    syncInspectorGeometry(w);
  }
  function onUp() {
    card.classList.remove('moving');
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    // Un redimensionnement change la mise en page du symbole : on le redessine
    if (handle) renderWidgetBody(card.querySelector('.widget-body'), w);
    // Écran « selon le contenu » : la fenêtre suit la nouvelle étendue des éléments
    if (!fixed) fitSynopticToScreen();
  }
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

/** Clic dans la fenêtre hors de tout élément : désélection. */
function onSynopticBackgroundDown(e) {
  if (state.mode !== 'studio' || e.target.closest('.syn-el')) return;
  state.selectedWidgetId = null;
  document.querySelectorAll('.syn-el.selected').forEach(n => n.classList.remove('selected'));
  const box = document.getElementById('inspectorContent');
  if (box) box.innerHTML = '<p class="hint">Sélectionnez un élément pour régler sa position, sa taille et sa variable.</p>';
}

/** Studio : déplacement au clavier (1 px, 10 px avec Maj) et suppression. */
document.addEventListener('keydown', e => {
  if (state.mode !== 'studio' || state.activeTab !== 'synoptic' || !state.selectedWidgetId) return;
  if (e.target.closest && e.target.closest('input, select, textarea, [contenteditable]')) return;
  const w = findDashboardWidget(state.selectedWidgetId);
  if (!w) return;
  if (e.key === 'Delete') {
    e.preventDefault();
    deleteWidget(w.id);
    return;
  }
  const step = e.shiftKey ? 10 : 1;
  const d = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
  if (!d) return;
  e.preventDefault();
  const [W, H] = synCanvasSize(getActiveDashboard());
  w.x = Math.max(0, Math.min(W - w.width, w.x + d[0]));
  w.y = Math.max(0, Math.min(H - w.height, w.y + d[1]));
  const card = document.getElementById(`widget-${cssSafe(w.id)}`);
  if (card) placeWidgetCard(card, w);
  syncInspectorGeometry(w);
});

function onPaletteDrag(e, widgetType) {
  e.dataTransfer.setData('text/plain', JSON.stringify({ kind: 'palette', widget_type: widgetType }));
}

function onCustomSymbolDrag(e, symbolId) {
  e.dataTransfer.setData('text/plain', JSON.stringify({ kind: 'custom_symbol', symbol_id: symbolId }));
}

/** Dépôt d'un symbole ou d'une variable sur la fenêtre, à la position du pointeur (px de l'écran cible). */
function handleCanvasDrop(e) {
  e.preventDefault();
  if (state.mode !== 'studio') return;
  const dash = getActiveDashboard();
  if (!dash) return;
  let payload;
  try { payload = JSON.parse(e.dataTransfer.getData('text/plain')); } catch (_) { return; }
  const rect = document.getElementById('synopticSurface').getBoundingClientRect();
  const px = (e.clientX - rect.left) / synView.zoom;
  const py = (e.clientY - rect.top) / synView.zoom;

  let newW = null;
  const place = (width, height) => ({
    id: 'w-' + Date.now(),
    x: Math.max(0, synSnap(px - width / 2)),
    y: Math.max(0, synSnap(py - height / 2)),
    width,
    height
  });
  if (payload.kind === 'tag') {
    const tag = state.tags[payload.tag_id];
    if (!tag) return;
    let wType = 'NumericDisplay';
    if (tag.data_type === 'Bool') wType = tag.writable ? 'Pump' : 'IndicatorLed';
    else if (tag.writable) wType = 'SetpointButton';
    else wType = 'Gauge';
    newW = { ...place(160, 140), widget_type: wType, bound_tag_id: tag.id, title: tag.label, properties: { min: 0, max: 100, step: 0.5 } };
  } else if (payload.kind === 'palette' && WIDGET_TYPES.includes(payload.widget_type)) {
    const size = payload.widget_type === 'TrendChart' ? [360, 200] : payload.widget_type === 'PipeSegment' ? [240, 40] : [140, 140];
    newW = { ...place(size[0], size[1]), widget_type: payload.widget_type, bound_tag_id: null, title: '', properties: { min: 0, max: 100, step: 0.5 } };
  } else if (payload.kind === 'custom_symbol') {
    const sym = state.symbols.find(s => s.id === payload.symbol_id);
    if (!sym) return;
    newW = {
      ...place(sym.default_width || 140, sym.default_height || 140),
      widget_type: 'CustomSvgSymbol',
      bound_tag_id: null,
      title: sym.name,
      properties: { symbol_id: sym.id, animate_rotation: sym.animate_rotation, svg_content: sym.svg_content }
    };
  }
  if (!newW) return;
  dash.widgets.push(newW);
  state.selectedWidgetId = newW.id;
  renderDashboard();
  renderWidgetInspector(newW);
}

// ============================================================================
// INSPECTEUR
// ============================================================================

function renderWidgetInspector(w) {
  const box = document.getElementById('inspectorContent');
  if (!box) return;
  const tagOpts = Object.values(state.tags).map(t =>
    `<option value="${esc(t.id)}" ${w.bound_tag_id === t.id ? 'selected' : ''}>${esc(t.id)} — ${esc(t.label)}</option>`
  ).join('');
  const props = w.properties || {};
  box.dataset.widgetId = w.id;
  box.innerHTML = `
    <div class="insp">
      <label>Libellé<input value="${esc(w.title)}" oninput="updateWidgetField('title', this.value)" /></label>
      <label>Symbole
        <select onchange="updateWidgetField('widget_type', this.value)">
          ${WIDGET_TYPES.map(t => `<option value="${t}" ${w.widget_type === t ? 'selected' : ''}>${t}</option>`).join('')}
        </select>
      </label>
      <label>Variable liée
        <select onchange="updateWidgetField('bound_tag_id', this.value || null)">
          <option value="">— Non liée —</option>${tagOpts}
        </select>
      </label>
      <div class="insp-sec">Position et taille (px de l'écran)</div>
      <div class="insp-row">
        <label>X<input type="number" data-geo="x" value="${Math.round(w.x)}" oninput="setWidgetGeometry('x', this.value)" /></label>
        <label>Y<input type="number" data-geo="y" value="${Math.round(w.y)}" oninput="setWidgetGeometry('y', this.value)" /></label>
      </div>
      <div class="insp-row">
        <label>Largeur<input type="number" min="${SYN_MIN_SIZE}" data-geo="width" value="${Math.round(w.width)}" oninput="setWidgetGeometry('width', this.value)" /></label>
        <label>Hauteur<input type="number" min="${SYN_MIN_SIZE}" data-geo="height" value="${Math.round(w.height)}" oninput="setWidgetGeometry('height', this.value)" /></label>
      </div>
      <label class="insp-check"><input type="checkbox" id="synKeepRatio" /> Conserver les proportions</label>
      <div class="insp-sec">Aligner sur l'écran</div>
      <div class="insp-actions">
        <button class="btn" title="Aligner à gauche" onclick="alignWidget('left')">Gauche</button>
        <button class="btn" title="Centrer horizontalement" onclick="alignWidget('center')">Centre</button>
        <button class="btn" title="Aligner en haut" onclick="alignWidget('top')">Haut</button>
        <button class="btn" title="Centrer verticalement" onclick="alignWidget('middle')">Milieu</button>
      </div>
      <div class="insp-sec">Échelle et consigne</div>
      <div class="insp-row3">
        <label>Min<input type="number" value="${Number(props.min ?? 0)}" onchange="updateWidgetProp('min', parseFloat(this.value))" /></label>
        <label>Max<input type="number" value="${Number(props.max ?? 100)}" onchange="updateWidgetProp('max', parseFloat(this.value))" /></label>
        <label>Pas<input type="number" step="0.1" min="0.01" value="${Number(props.step ?? 0.5)}" onchange="updateWidgetProp('step', parseFloat(this.value))" /></label>
      </div>
      <div class="insp-sec">Élément</div>
      <div class="insp-actions">
        <button class="btn" onclick="duplicateSelectedWidget()">Dupliquer</button>
        <button class="btn" onclick="moveWidgetLayer(1)" title="Passer devant">Devant</button>
        <button class="btn" onclick="moveWidgetLayer(-1)" title="Passer derrière">Derrière</button>
        <button class="btn btn-danger" onclick="deleteWidget(${jsArg(w.id)})">Supprimer</button>
      </div>
      <p class="hint">Glisser pour déplacer, poignées pour redimensionner. Flèches : 1 px (Maj : 10 px). Suppr : supprimer.</p>
    </div>`;
}

/** Met à jour les champs de position et de taille sans reconstruire l'inspecteur (saisie en cours). */
function syncInspectorGeometry(w) {
  const box = document.getElementById('inspectorContent');
  if (!box || box.dataset.widgetId !== w.id) return;
  box.querySelectorAll('input[data-geo]').forEach(inp => {
    if (inp !== document.activeElement) inp.value = Math.round(w[inp.dataset.geo]);
  });
}

function setWidgetGeometry(key, raw) {
  const w = findDashboardWidget(state.selectedWidgetId);
  if (!w) return;
  const v = Math.round(Number(raw));
  if (!Number.isFinite(v)) return;
  const keep = document.getElementById('synKeepRatio');
  if ((key === 'width' || key === 'height') && keep && keep.checked && w.width > 0 && w.height > 0) {
    const ratio = w.width / w.height;
    if (key === 'width') { w.width = Math.max(SYN_MIN_SIZE, v); w.height = Math.max(SYN_MIN_SIZE, Math.round(w.width / ratio)); }
    else { w.height = Math.max(SYN_MIN_SIZE, v); w.width = Math.max(SYN_MIN_SIZE, Math.round(w.height * ratio)); }
  } else {
    w[key] = key === 'width' || key === 'height' ? Math.max(SYN_MIN_SIZE, v) : Math.max(0, v);
  }
  const card = document.getElementById(`widget-${cssSafe(w.id)}`);
  if (card) {
    placeWidgetCard(card, w);
    if (key === 'width' || key === 'height') renderWidgetBody(card.querySelector('.widget-body'), w);
  }
  syncInspectorGeometry(w);
}

function alignWidget(where) {
  const w = findDashboardWidget(state.selectedWidgetId);
  if (!w) return;
  const [W, H] = synCanvasSize(getActiveDashboard());
  if (where === 'left') w.x = 0;
  if (where === 'center') w.x = Math.round((W - w.width) / 2);
  if (where === 'top') w.y = 0;
  if (where === 'middle') w.y = Math.round((H - w.height) / 2);
  renderDashboard();
  syncInspectorGeometry(w);
}

/** Ordre d'affichage : l'ordre de la liste est l'ordre de superposition. */
function moveWidgetLayer(dir) {
  const dash = getActiveDashboard();
  if (!dash) return;
  const i = dash.widgets.findIndex(w => w.id === state.selectedWidgetId);
  const j = i + dir;
  if (i < 0 || j < 0 || j >= dash.widgets.length) return;
  [dash.widgets[i], dash.widgets[j]] = [dash.widgets[j], dash.widgets[i]];
  renderDashboard();
}

function updateWidgetField(field, val) {
  const w = findDashboardWidget(state.selectedWidgetId);
  if (!w) return;
  w[field] = val;
  const card = document.getElementById(`widget-${cssSafe(w.id)}`);
  if (field === 'title' && card) {
    card.querySelector('.syn-label').textContent = val;
    return;
  }
  renderDashboard();
}

function updateWidgetProp(prop, val) {
  const w = findDashboardWidget(state.selectedWidgetId);
  if (!w) return;
  if (!w.properties) w.properties = {};
  w.properties[prop] = val;
  renderDashboard();
}

// ============================================================================
// DESSIN DES ÉQUIPEMENTS
// ============================================================================

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
      <button class="btn" style="margin-top:6px;" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!boolVal})">
        ${boolVal ? 'MARCHE (Arrêter)' : 'ARRÊT (Démarrer)'}
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
      <button class="btn" style="margin-top:6px;" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!boolVal})">
        ${boolVal ? 'SOUFFLAGE ACTIF' : 'VENTILATEUR ARRÊTÉ'}
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
      <input type="range" min="0" max="100" value="${pct}" aria-label="${esc(w.title || w.bound_tag_id || 'Commande')}" onchange="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, parseFloat(this.value))" />
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
      <input type="range" min="0" max="100" value="${pct}" aria-label="${esc(w.title || w.bound_tag_id || 'Commande')}" onchange="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, parseFloat(this.value))" />
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'SetpointButton') {
    const step = Number(w.properties && w.properties.step) || 0.5;
    return `
      <div class="big-num" style="color:#38bdf8;">${numVal.toFixed(1)} ${unit}</div>
      <div class="setpoint-controls">
        <button class="btn" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${(numVal - step).toFixed(2)})">-${step}</button>
        <input type="number" step="${step}" value="${numVal}" id="sp-in-${cssSafe(w.id)}" aria-label="Nouvelle consigne : ${esc(w.title || w.bound_tag_id || '')}" />
        <button class="btn" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${(numVal + step).toFixed(2)})">+${step}</button>
        <button class="btn btn-primary" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, parseFloat(document.getElementById(${jsArg('sp-in-' + cssSafe(w.id))}).value))">OK</button>
      </div>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'SwitchButton') {
    return `
      <div style="margin:8px 0;">
        <button class="btn ${boolVal ? 'btn-primary' : ''}" style="padding:10px 18px;font-size:13px;"
                onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!boolVal})">
          ${boolVal ? 'COMMUTATEUR : ON (1)' : 'COMMUTATEUR : OFF (0)'}
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
        <span class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}${tag && !tag.historize ? ' · non historisée (courbe depuis l\'ouverture de la page)' : ''}</span>
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
        ${allowWrite ? `<button class="btn" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!boolVal})">Tester</button>` : ''}
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
      <button class="btn" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!open})">
        ${open ? 'VANNE OUVERTE' : 'VANNE FERMÉE'}
      </button>
      <div class="widget-tag-sub">${esc(w.bound_tag_id) || 'Non lié'}</div>
    `;
  }

  if (w.widget_type === 'Compressor') {
    const active = boolVal || numVal > 0.5;
    const col = active ? '#10b981' : '#8b9bb4';
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
      <div style="font-weight:700;font-size:12px;color:${col};">${clogged ? 'COLMATAGE FILTRE ΔP' : 'FILTRE PROPRE'}</div>
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
      <button class="btn" onclick="selectCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, ${!closed})">
        ${closed ? 'ENCLENCHÉ (FERMÉ)' : 'OUVERT (ISOLÉ)'}
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
    const col = run ? '#10b981' : '#8b9bb4';
    return `
      <svg width="86" height="82" viewBox="0 0 100 94">
        <circle cx="50" cy="48" r="34" fill="#0b1120" stroke="${col}" stroke-width="3.5"/>
        <text x="50" y="46" text-anchor="middle" fill="${col}" font-size="18" font-weight="bold">M</text>
        <text x="50" y="64" text-anchor="middle" fill="#94a3b8" font-size="13">3~</text>
      </svg>
      <div style="font-weight:700;font-size:12px;color:${col};">${run ? 'MOTEUR 3~ ACTIF' : 'ARRÊTÉ'}</div>
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
    // Bouton impulsionnel : un premier appui l'arme (sélection), l'impulsion part ensuite tant
    // que le bouton est maintenu, dans le délai de sélection
    const armed = state.armedCommand && state.armedCommand.widgetId === w.id;
    return `
      <button class="btn btn-primary" style="padding:12px 20px;border-radius:30px;font-weight:800;"
              onmousedown="pulseCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, true)"
              onmouseup="pulseCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, false)"
              onmouseleave="if (event.buttons & 1) pulseCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, false)"
              ontouchstart="event.preventDefault(); pulseCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, true)"
              ontouchend="event.preventDefault(); pulseCommand(${jsArg(w.id)}, ${jsArg(w.bound_tag_id)}, false)">
        ${armed ? 'ARMÉ : MAINTENIR POUR L\'IMPULSION' : 'IMPULSION MARCHE'}
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
    const spin = isActive && ((sym && sym.animate_rotation) || (w.properties && w.properties.animate_rotation));
    // Emplacement vide : le SVG assaini y est inséré par le DOM (mountCustomSvg), jamais en HTML
    return `
      <div class="custom-svg-wrapper" data-spin="${spin ? '1' : '0'}" style="color:${dynColor};"></div>
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

/** Insère le SVG assaini d'un symbole générique dans son emplacement, par le DOM. */
function mountCustomSvg(container, w) {
  if (w.widget_type !== 'CustomSvgSymbol') return;
  const slot = container.querySelector('.custom-svg-wrapper');
  if (!slot) return;
  const symId = w.properties && w.properties.symbol_id;
  const sym = state.symbols.find(s => s.id === symId);
  const svg = sanitizeSvg(sym ? sym.svg_content : (w.properties && w.properties.svg_content) || DEFAULT_SVG);
  if (!svg) {
    slot.replaceChildren();
    return;
  }
  if (slot.dataset.spin === '1') {
    svg.querySelectorAll('.sym-rotatable').forEach(part => part.classList.add('spin-active'));
  }
  slot.replaceChildren(svg);
}

/** (Re)dessine le corps d'un widget : gabarit HTML échappé, puis SVG inséré par le DOM. */
function renderWidgetBody(el, w) {
  el.innerHTML = buildWidgetBodyHtml(w);
  mountCustomSvg(el, w);
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
      // Ne pas remplacer le corps DOM d'un bouton impulsionnel armé ou en cours d'appui
      if (w.widget_type === 'PushButton' && state.armedCommand && state.armedCommand.widgetId === w.id) {
        return;
      }
      const el = document.getElementById(`widget-body-${cssSafe(w.id)}`);
      if (el) renderWidgetBody(el, w);
    }
  });
}









async function deleteWidget(id) {
  const dash = getActiveDashboard();
  const w = dash && dash.widgets.find(x => x.id === id);
  if (!w || !(await confirmDialog('Supprimer cet élément ?', w.title || w.widget_type, 'Supprimer'))) return;
  dash.widgets = dash.widgets.filter(x => x.id !== id);
  state.selectedWidgetId = null;
  renderDashboard();
  onSynopticBackgroundDown({ target: document.body });
}

// ============================================================================
// COMMANDES DES SYNOPTIQUES : SÉLECTION PUIS EXÉCUTION
// ============================================================================
// Un clic sur une commande d'un synoptique ne l'envoie pas : il la sélectionne. L'équipement
// s'encadre et un bandeau « Exécuter / Annuler » apparaît ; la commande ne part que sur
// « Exécuter », dans le délai de sélection. Passé ce délai, sur « Annuler », sur Échap ou en
// quittant le synoptique, elle est abandonnée sans rien envoyer.

const COMMAND_SELECT_SECONDS = 10;

/** Valeur de commande lisible (booléen en clair, nombre avec l'unité de la variable). */
function describeCommandValue(tagId, value) {
  if (typeof value === 'boolean') return value ? 'MARCHE / OUVERT (1)' : 'ARRÊT / FERMÉ (0)';
  const tag = state.tags[tagId];
  const unit = tag && tag.unit ? ` ${tag.unit}` : '';
  return typeof value === 'number' ? `${Number(value.toFixed(3))}${unit}` : String(value);
}

function findDashboardWidget(widgetId) {
  const dash = getActiveDashboard();
  return dash ? dash.widgets.find(w => w.id === widgetId) : null;
}

/** Sélectionne une commande (premier temps) ; une nouvelle sélection remplace la précédente. */
function selectCommand(widgetId, tagId, value, momentary = false) {
  // En Studio, un clic sert à éditer l'élément, jamais à commander l'équipement
  if (!tagId || state.mode === 'studio') return;
  if (!canWrite()) {
    notify('Votre profil ne permet pas l\'envoi de consignes.', 'error');
    return;
  }
  if (typeof value === 'number' && !Number.isFinite(value)) {
    notify('Valeur de consigne invalide', 'error');
    return;
  }
  cancelCommand(false);
  const w = findDashboardWidget(widgetId);
  state.armedCommand = {
    widgetId,
    tagId,
    value,
    momentary,
    pressed: false,
    title: (w && w.title) || tagId,
    expiresAt: Date.now() + COMMAND_SELECT_SECONDS * 1000
  };
  state.armedTimer = setInterval(tickArmedCommand, 250);
  refreshArmedWidget(widgetId);
  renderCommandBar();
}

function cleanupGlobalPulseListeners() {
  if (window.__pulseReleaseHandler) {
    window.removeEventListener('mouseup', window.__pulseReleaseHandler);
    window.removeEventListener('touchend', window.__pulseReleaseHandler);
    window.removeEventListener('touchcancel', window.__pulseReleaseHandler);
    window.__pulseReleaseHandler = null;
  }
}

/** Abandonne la commande sélectionnée (sans rien envoyer, sauf remise à zéro d'impulsion si en cours). */
function cancelCommand(announce = true) {
  cleanupGlobalPulseListeners();
  const c = state.armedCommand;
  if (state.armedTimer) clearInterval(state.armedTimer);
  state.armedTimer = null;
  state.armedCommand = null;
  const bar = document.getElementById('commandBar');
  if (bar) bar.remove();
  if (c) {
    refreshArmedWidget(c.widgetId);
    if (c.pressed) {
      c.pressed = false;
      const writePromise = c.pendingWrite;
      (async () => {
        if (writePromise) {
          try { await writePromise; } catch (_) {}
        }
        await sendWriteTag(c.tagId, false);
      })();
    }
    if (announce) notify('Commande annulée : rien n\'a été envoyé');
  }
}

/** Second temps : envoie la commande sélectionnée. */
async function executeArmedCommand() {
  const c = state.armedCommand;
  if (!c || c.momentary) return;
  cancelCommand(false);
  if (await sendWriteTag(c.tagId, c.value)) {
    notify(`Commande envoyée : ${c.title} → ${describeCommandValue(c.tagId, c.value)}`);
  }
}

/** Bouton impulsionnel : armement au premier appui, puis impulsion (1 puis 0) une seule fois. */
async function pulseCommand(widgetId, tagId, down) {
  const c = state.armedCommand;
  const armed = c && c.momentary && c.widgetId === widgetId;
  if (down) {
    if (!armed) {
      selectCommand(widgetId, tagId, true, true);
      return;
    }
    c.pressed = true;
    renderCommandBar();
    cleanupGlobalPulseListeners();
    const releaseHandler = () => {
      cleanupGlobalPulseListeners();
      pulseCommand(widgetId, tagId, false);
    };
    window.__pulseReleaseHandler = releaseHandler;
    window.addEventListener('mouseup', releaseHandler);
    window.addEventListener('touchend', releaseHandler);
    window.addEventListener('touchcancel', releaseHandler);

    c.pendingWrite = sendWriteTag(tagId, true);
    await c.pendingWrite;
  } else if (armed && c.pressed) {
    cleanupGlobalPulseListeners();
    c.pressed = false;
    const writePromise = c.pendingWrite;
    cancelCommand(false);
    if (writePromise) {
      try { await writePromise; } catch (_) {}
    }
    if (await sendWriteTag(tagId, false)) notify(`Impulsion envoyée : ${c.title}`);
  }
}

function tickArmedCommand() {
  const c = state.armedCommand;
  if (!c) return;
  // Pas d'expiration pendant une impulsion : le relâchement doit toujours envoyer le 0
  if (!c.pressed && Date.now() >= c.expiresAt) {
    cancelCommand(false);
    notify('Sélection expirée : aucune commande envoyée', 'error');
    return;
  }
  const left = document.getElementById('commandBarLeft');
  if (left) left.textContent = `${Math.max(0, Math.ceil((c.expiresAt - Date.now()) / 1000))} s`;
}

/** Encadre (ou libère) l'équipement de la commande sélectionnée. */
function refreshArmedWidget(widgetId) {
  const card = document.getElementById(`widget-${cssSafe(widgetId)}`);
  if (!card) return;
  card.classList.toggle('cmd-armed', !!(state.armedCommand && state.armedCommand.widgetId === widgetId));
  const w = findDashboardWidget(widgetId);
  const body = document.getElementById(`widget-body-${cssSafe(widgetId)}`);
  if (w && body && w.widget_type === 'PushButton') renderWidgetBody(body, w);
}

function renderCommandBar() {
  const c = state.armedCommand;
  if (!c) return;
  let bar = document.getElementById('commandBar');
  if (!bar) {
    bar = document.createElement('div');
    bar.id = 'commandBar';
    bar.setAttribute('role', 'alertdialog');
    bar.setAttribute('aria-label', 'Commande sélectionnée');
    document.body.appendChild(bar);
  }
  const what = c.momentary
    ? `Bouton impulsionnel armé : <b>${esc(c.title)}</b> (${esc(c.tagId)}). Maintenez le bouton pour envoyer l'impulsion.`
    : `Commande sélectionnée : <b>${esc(c.title)}</b> (${esc(c.tagId)}) → <b class="cmd-value">${esc(describeCommandValue(c.tagId, c.value))}</b>`;
  bar.innerHTML = `
    <span class="cmd-text">${what}</span>
    <span id="commandBarLeft" class="cmd-left">${COMMAND_SELECT_SECONDS} s</span>
    ${c.momentary ? '' : '<button id="commandBarExec" class="btn btn-primary" onclick="executeArmedCommand()">▶ Exécuter</button>'}
    <button class="btn" onclick="cancelCommand()">Annuler</button>
  `;
  tickArmedCommand();
  const exec = document.getElementById('commandBarExec');
  if (exec) exec.focus();
}

document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && state.armedCommand) cancelCommand();
});

/**
 * Exploitation (commandes, fenêtre ajustée à l'écran) ou Studio (édition : bibliothèque,
 * fenêtre de l'écran cible avec grille, inspecteur). Le Studio est réservé aux profils autorisés.
 */
function setMode(mode) {
  if (!state.currentUser || !state.currentUser.permissions || !state.currentUser.permissions.can_edit_studio) {
    mode = 'run';
  }
  if (mode !== 'run') cancelCommand(false);
  state.mode = mode;
  const studio = mode === 'studio';
  document.body.classList.toggle('run-mode', !studio);
  document.body.classList.toggle('studio', studio);
  const bs = document.getElementById('btnModeStudio'), br = document.getElementById('btnModeRun');
  if (bs) { bs.classList.toggle('on', studio); bs.classList.toggle('active-studio', studio); }
  if (br) { br.classList.toggle('on', !studio); br.classList.toggle('active-run', !studio); }
  if (!studio) {
    state.selectedWidgetId = null;
    document.querySelectorAll('.syn-el.selected').forEach(n => n.classList.remove('selected'));
  }
  synView.fitted = true;
  fitSynopticToScreen();
  // Les panneaux du Studio changent la place disponible : nouvel ajustement après la transition
  setTimeout(fitSynopticToScreen, 250);
}

