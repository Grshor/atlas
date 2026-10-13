/* static/js/herdr.js — панель «Рабочие столы» (herdr).
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { api, jsonOpts } from './lib/api.js';
import { bindWinControls, ch, chromeFillers, fillChrome, loadNotifications, panelRoots, refreshIv, setChrome, winControlsHtml } from './core.js';
import { el, esc, log } from './lib/dom.js';
import { onTick } from './lib/ticker.js';

/* =====================================================================
   панель «herdr» (?win=herdr): дерево workspace → tabs, фокус кликом.
   GET /api/herdr; POST /api/herdr/focus {kind,id}; клиент-чип живёт в #chrome.
   =================================================================== */
const HERDR_IV_MAX = 10000; /* автообновление 10 с, но не реже ui.autorefresh_sec */
let herdrCache = null;      /* последний ok-ответ /api/herdr */
let herdrErr = null;        /* текст недоступности (404 / сеть) */
let herdrPending = null;    /* {kind,id} — оптимистично подсвеченный ряд */

function agentSummary(agents) {
  const list = Array.isArray(agents) ? agents : [];
  if (!list.length) return '<span class="ag-none">○ нет агентов</span>';
  return list.map((a) => {
    const st = String((a && a.status) || '—');
    const working = /work|busy|active|running|think/i.test(st);
    const cnt = a && a.count != null ? ' (' + esc(a.count) + ')' : '';
    return '<span class="' + (working ? 'ag-work' : 'ag-ok') + '">' + (working ? '◆' : '●') + '</span> ' + esc(st) + cnt;
  }).join(' · ');
}
function herdrFilterValue() {
  const inp = panelRoots.herdr && panelRoots.herdr.querySelector('#herdr-filter');
  return ((inp && inp.value) || '').trim().toLowerCase();
}
function herdrRow(kind, item, isWs) {
  const row = el('div', 'hrow ' + (isWs ? 'hws' : 'htab') + (item.focused ? ' focused' : ''));
  if (herdrPending && herdrPending.kind === kind && String(herdrPending.id) === String(item.id)) row.classList.add('pending');
  row.dataset.kind = kind;
  row.dataset.id = String(item.id);
  row.dataset.label = String(item.label || item.id);
  row.tabIndex = 0;
  row.title = 'сфокусировать ' + (isWs ? 'рабочий стол' : 'вкладку') + ': ' + (item.label || item.id);
  row.appendChild(el('span', 'hdot', '●'));
  row.appendChild(el('span', 'hlabel', esc(item.label || item.id)));
  if (isWs) {
    row.appendChild(el('span', 'hmeta', agentSummary(item.agents)));
    const n = Array.isArray(item.tabs) ? item.tabs.length : 0;
    row.appendChild(el('span', 'hmeta', '· вкладок ' + n));
  } else {
    row.appendChild(el('span', 'hmeta', '· панелей ' + (item.pane_count != null ? esc(item.pane_count) : '?')));
  }
  row.addEventListener('click', () => herdrFocus(kind, item.id, item.label));
  row.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); herdrFocus(kind, item.id, item.label); }
  });
  return row;
}
function herdrDown() {
  if (herdrErr) return true;
  return !!(herdrCache && (herdrCache.ok === false || herdrCache.server === false));
}
function renderHerdr() {
  const box = panelRoots.herdr && panelRoots.herdr.querySelector('#herdr-tree');
  if (!box) return;
  box.innerHTML = '';
  if (herdrErr) { box.appendChild(el('div', 'dim', herdrErr)); return; }
  /* HTTP 200 с ok:false/server:false — herdr-сервер лежит, столов не будет */
  if (herdrCache && (herdrCache.ok === false || herdrCache.server === false)) {
    box.appendChild(el('div', 'dim', 'herdr сервер недоступен — пульт покажет столы, когда он поднимется'));
    return;
  }
  if (!herdrCache) { box.appendChild(el('div', 'dim', 'загрузка…')); return; }
  const flt = herdrFilterValue();
  const ws = Array.isArray(herdrCache.workspaces) ? herdrCache.workspaces : [];
  let shown = 0;
  for (const w of ws) {
    const wMatch = !flt || String(w.label || '').toLowerCase().includes(flt);
    const tabs = (Array.isArray(w.tabs) ? w.tabs : [])
      .filter((t) => wMatch || String(t.label || '').toLowerCase().includes(flt));
    if (!wMatch && !tabs.length) continue;
    shown++;
    box.appendChild(herdrRow('workspace', w, true));
    for (const t of tabs) box.appendChild(herdrRow('tab', t, false));
  }
  if (!shown) box.appendChild(el('div', 'dim', flt ? '— ничего не найдено по фильтру —' : '— рабочих столов нет —'));
}
async function herdrFocus(kind, id, label) {
  herdrPending = { kind, id };
  renderHerdr();
  log('herdr: фокус ' + kind + ' «' + (label || id) + '»…');
  try {
    const r = await api('/api/herdr/focus', jsonOpts('POST', { kind, id }));
    if (r && r.ok === false) log('✗ herdr: ' + (r.detail || 'не удалось сфокусировать'));
    else {
      const extra = [];
      if (r && r.detail) extra.push(r.detail);
      if (r && r.reused) extra.push('переиспользовано готовое');
      log('✓ herdr: ' + (extra.length ? extra.join(' · ') : 'сфокусировано'));
    }
  } catch (e) {
    log('✗ herdr фокус: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/herdr/focus' : e.message));
  }
  herdrPending = null;
  await loadHerdr();
  loadNotifications();
}
async function loadHerdr() {
  try {
    const d = await api('/api/herdr');
    herdrCache = d && typeof d === 'object' ? d : null;
    herdrErr = null;
  } catch (e) {
    herdrCache = null;
    herdrErr = String(e.message).indexOf('404') !== -1
      ? 'бэкенд без /api/herdr — пульт недоступен'
      : 'нет связи с бэкендом: ' + e.message;
  }
  renderHerdr();
  herdrFillChrome();
  herdrFillStat();
}
function herdrClientText() {
  if (herdrDown()) return { text: 'сервер недоступен', cls: 'dim' };
  const c = herdrCache && herdrCache.client;
  if (!c) return { text: 'клиент: неизвестно', cls: 'dim' };
  if (c.attached) {
    /* бэкенд отдаёт полный путь устройства (/dev/pts/8) — в чипе он лишний */
    const tty = c.tty ? String(c.tty).replace(/^\/dev\//, '') : '';
    return { text: 'клиент: подключён' + (tty ? ' (' + tty + ')' : ''), cls: 'ok' };
  }
  return { text: 'клиент не запущен — откроется при фокусе', cls: 'dim' };
}
function herdrFillStat() {
  const stat = panelRoots.herdr && panelRoots.herdr.querySelector('#herdr-stat');
  if (!stat) return;
  const ws = herdrCache && Array.isArray(herdrCache.workspaces) ? herdrCache.workspaces.length : 0;
  stat.textContent = herdrDown() ? '' : ('столов: ' + ws);
}
function herdrFillChrome() {
  if (!ch.h || !ch.h.client) return;
  const st = herdrClientText();
  ch.h.client.textContent = st.text;
  ch.h.client.className = st.cls;
}
function buildHerdr(root) {
  panelRoots.herdr = root;
  const q = (s) => root.querySelector(s);
  root.innerHTML =
    '<div class="panel" style="grid-template-rows:auto minmax(0,1fr) auto">' +
      '<div class="toolbar">' +
        '<input id="herdr-filter" class="fld" style="width:180px" placeholder="фильтр: по label (/)" autocomplete="off" spellcheck="false">' +
        '<button id="herdr-refresh" class="btn95 ic" type="button" title="перечитать состояние herdr" aria-label="обновить">⟳</button>' +
        '<span id="herdr-stat" class="dim"></span>' +
        '<span class="spacer"></span>' +
        winControlsHtml() +
      '</div>' +
      '<div class="scroll well grow"><div class="htree" id="herdr-tree"></div></div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  q('#herdr-filter').addEventListener('input', renderHerdr);
  q('#herdr-refresh').addEventListener('click', () => { log('herdr: обновление…'); loadHerdr(); });
  bindWinControls(root);
  loadHerdr();
  onTick('herdr', Math.min(HERDR_IV_MAX, refreshIv()), loadHerdr);
  chromeFillers.herdr = () => {
    ch.h = {};
    setChrome((mk) => {
      ch.h.client = mk('span', 'dim', 'клиент: неизвестно');
      const b = mk('button', 'btn95 ic', '⟳');
      b.type = 'button';
      b.title = 'перечитать состояние herdr';
      b.setAttribute('aria-label', 'обновить');
      b.addEventListener('click', loadHerdr);
    });
    herdrFillChrome();
  };
  fillChrome();
  herdrFillStat();
}

export { HERDR_IV_MAX, buildHerdr, loadHerdr, renderHerdr };
