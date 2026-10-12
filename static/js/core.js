/* static/js/core.js — каркас: окно/вкладки/хост, состояние, настройки, тема, статус-строка, оконные операции.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc, log } from './lib/dom.js';
import { API, api, jsonOpts } from './lib/api.js';
import { HERDR_IV_MAX, buildHerdr } from './herdr.js';
import { TICK_LOADERS, onTick } from './lib/ticker.js';
import { armBoardTick, buildBoard } from './board.js';
import { buildDeps } from './deps.js';
import { buildOrchestrator } from './orchestrator.js';
import { buildProjects, projCache, renderProjects } from './projects.js';
import { buildSearch } from './search.js';
import { buildSources, renderSources, srcCache, updateSyncAge } from './sources.js';
import { buildTerminal, termApplyTheme } from './terminal.js';
import { buildView } from './view.js';
import { openSettingsView, refreshSettingsView, renderSettingsForm, renderWindowsSection } from './setview.js';

/* ---------- какое окно открыто ---------- */
/* Реестр панелей: девять (панель «settings» убрана — ⚙ теперь переключает текущее
   окно в режим настроек на месте). Порядок — фолбэк: живой список приходит из
   GET /api/ui panels/tabs. */
const PANELS = ['projects', 'board', 'deps', 'search', 'sources', 'herdr', 'view', 'terminal', 'orchestrator'];
const TITLES = {
  projects: 'Проекты', board: 'Доска', deps: 'Зависимости', search: 'Поиск', sources: 'Источники',
  herdr: 'herdr', view: 'Просмотр', terminal: 'Терминал', orchestrator: 'Оркестратор',
};
const WIN = (() => {
  const v = new URLSearchParams(location.search).get('win');
  return PANELS.includes(v) ? v : 'projects';
})();
document.documentElement.dataset.win = WIN;
document.title = 'atlas — ' + TITLES[WIN];

/* ---------- состояние ---------- */
const state = {
  selected: localStorage.getItem('atlas.selected') || null,
  themeMtime: null,
  backendUp: null,
};
let uiCache = null; /* последний ответ GET /api/ui (+ gui, port_owner) */
let activeTab = null; /* какая вкладка открыта внутри хоста */
let notifyCache = { items: [], unread: 0 };
let notifyAvailable = null; /* null — неизвестно; false — бэкенд без /api/notify */
let curNotifyIv = 15000;
const builtTabs = new Set(); /* панели, уже построенные во вкладках (builder — один раз) */
const panelRoots = {}; /* корень каждой построенной панели — для адресных запросов к DOM */
const chromeFillers = {}; /* панель → функция заполнения #chrome */
const ch = {}; /* ссылки на узлы статус-строки текущей панели */
const timers = { probe: null, theme: null, retry: {} }; /* tick-интервалы переехали в js/lib/ticker.js */
let lastSyncSec = null;

/* ---------- настройки: дефолты — ровно как в контракте /api/settings ---------- */
const DEFAULT_SETTINGS = {
  'ui.statusbar': 'always',
  'ui.tabbar': 'on',
  'ui.autorefresh_sec': 30,
  'ui.search_k': 20,
  'ui.show_probe': true,
  'ui.time_format': 'relative',
  'ui.show_hybrid_details': true,
  'patrol.enabled': true,
  'patrol.interval_min': 360,
};
let settings = Object.assign({}, DEFAULT_SETTINGS);
let setDefaults = Object.assign({}, DEFAULT_SETTINGS);
let settingsRawJson = null; /* «как отдал бэкенд» — чтобы не перерисовывать зря */
let curRefreshIv = 30;

/* Описание девяти ключей настроек: одна таблица — для «глобальных» (PATCH без win)
   и для «это окно» (PATCH с win + сброс null). Порядок/группы — как раньше. */
/* относительное время: 2м / 1ч / 3д; либо абсолютное, если так настроено */
function rel(sec) {
  if (!sec) return '—';
  const d = Date.now() / 1000 - sec;
  if (d < 0) return 'сейчас';
  if (d < 60) return Math.max(1, Math.floor(d)) + 'с';
  if (d < 3600) return Math.floor(d / 60) + 'м';
  if (d < 86400) return Math.floor(d / 3600) + 'ч';
  return Math.floor(d / 86400) + 'д';
}
function fmtAge(sec) {
  if (!sec) return '—';
  if ((settings['ui.time_format'] || 'relative') !== 'absolute') return rel(sec);
  const d = new Date(sec * 1000);
  return d.toLocaleDateString() + ' ' + d.toLocaleTimeString();
}

function setChrome(fn) {
  const c = $('#chrome');
  if (!c) return;
  c.innerHTML = '';
  fn((tag, cls, html) => { const n = el(tag, cls, html); c.appendChild(n); return n; });
  mountBell(c); /* колокольчик есть в каждом окне, после панели */
}

/* ---------- уведомления (оркестратор): GET/POST /api/notify ----------
   Колокольчик «🔔 N» в #chrome: скрыт, пока эндпоинта нет (404); N>0 — акцент. */
function notifyIv() { return Math.min(15, Math.max(5, Number(settings['ui.autorefresh_sec']) || 10)) * 1000; }
function newestUnread() {
  const items = notifyCache.items.slice().sort((a, b) => (b.ts || 0) - (a.ts || 0));
  return items.find((i) => !i.read) || null;
}
function bellTitle() {
  const first = newestUnread();
  return first ? 'непрочитано: ' + first.text : 'уведомления';
}
function mountBell(c) {
  const b = el('button', 'mini bell' + (notifyCache.unread > 0 ? ' bell-hot' : ''), '🔔 ' + notifyCache.unread);
  b.type = 'button';
  b.id = 'btn-bell';
  b.title = bellTitle();
  b.classList.toggle('hidden', notifyAvailable === false);
  b.addEventListener('click', notifyOverlay);
  c.appendChild(b);
}
function updateBell() {
  const b = $('#btn-bell');
  if (!b) return;
  b.classList.toggle('hidden', notifyAvailable === false);
  b.classList.toggle('bell-hot', notifyCache.unread > 0);
  b.textContent = '🔔 ' + notifyCache.unread;
  b.title = bellTitle();
}
async function loadNotifications() {
  let d = null;
  try {
    d = await api('/api/notify?unread=1&limit=50');
  } catch (e) {
    if (String(e.message).indexOf('404') !== -1) { notifyAvailable = false; updateBell(); }
    /* прочие ошибки (нет связи) — молча держим прежнее состояние */
    return;
  }
  notifyAvailable = true;
  const items = Array.isArray(d.items) ? d.items : [];
  for (const n of items) if (n.ts > 1e12) n.ts = Math.round(n.ts / 1000); /* мс → с */
  notifyCache = { items, unread: d.unread != null ? d.unread : items.filter((i) => !i.read).length };
  updateBell();
}
/* клик по строке: отметить прочитанным (restrike), счётчик и колокольчик сразу */
async function markNotifyRead(n, row) {
  try {
    await api('/api/notify/read', jsonOpts('POST', { id: n.id }));
    n.read = true;
    if (row) row.classList.add('read');
    if (notifyCache.unread > 0) notifyCache.unread--;
    updateBell();
    loadNotifications(); /* сразу после действия чтения */
  } catch (e) { log('✗ не удалось отметить прочитанным: ' + e.message); }
}
function notifyRow(n) {
  const row = el('div', 'nrow' + (n.read ? ' read' : ''));
  const lvl = ['info', 'warn', 'err'].indexOf(n.level) !== -1 ? n.level : 'info';
  row.appendChild(el('span', 'ndot ndot-' + lvl));
  if (n.project) row.appendChild(el('span', 'chip proj', esc(n.project)));
  row.appendChild(el('span', 'nts dim', fmtAge(n.ts)));
  const txt = el('span', 'ntext', esc(n.text || ''));
  txt.title = n.text || '';
  row.appendChild(txt);
  if (n.read) row.title = 'прочитано';
  else {
    row.title = 'отметить прочитанным';
    row.addEventListener('click', () => markNotifyRead(n, row));
  }
  return row;
}
function notifyOverlay() {
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out notify-dlg');
  dlg.appendChild(el('h3', null, 'уведомления'));
  const list = el('div', 'nlist');
  const items = notifyCache.items.slice().sort((a, b) => (b.ts || 0) - (a.ts || 0));
  if (!items.length) list.appendChild(el('div', 'dim', 'уведомлений нет'));
  for (const n of items) list.appendChild(notifyRow(n));
  dlg.appendChild(list);
  const btns = el('div', 'dbtns');
  const readAll = el('button', 'btn95', 'прочитать всё');
  const close = el('button', 'btn95', 'закрыть');
  btns.append(readAll, close);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);
  const done = () => { ov.remove(); document.removeEventListener('keydown', key, true); };
  const key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(); } };
  document.addEventListener('keydown', key, true);
  close.addEventListener('click', done);
  ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(); });
  readAll.addEventListener('click', async () => {
    try {
      await api('/api/notify/read', jsonOpts('POST', { all: true }));
      notifyCache.items.forEach((i) => { i.read = true; });
      notifyCache.unread = 0;
      updateBell();
      loadNotifications();
      log('✓ все уведомления прочитаны');
      list.innerHTML = '';
      list.appendChild(el('div', 'dim', 'уведомлений нет'));
    } catch (e) { log('✗ не удалось отметить: ' + e.message); }
  });
}
function updateProbeText() {
  const n = $('#probe');
  if (!n) return;
  if (state.backendUp) n.textContent = 'бэкенд: ok' + (lastSyncSec ? ' · синк ' + fmtAge(lastSyncSec) : '');
  else n.textContent = 'бэкенд: нет связи';
}
function probeSet(up) {
  state.backendUp = up;
  const n = $('#probe');
  if (!n) return;
  n.classList.toggle('ok', !!up);
  n.classList.toggle('down', !up);
  updateProbeText();
}
/* зонд живости: лёгкий GET /api/sources раз в 60с; заодно возраст последнего синка */
async function probe() {
  try {
    const d = await api('/api/sources');
    probeSet(true);
    const arr = Array.isArray(d.sources) ? d.sources : [];
    const mx = arr.reduce((m, s) => Math.max(m, s.last_sync || 0), 0);
    if (mx) { lastSyncSec = mx; updateProbeText(); }
    updateSyncAge();
  } catch (_) { probeSet(false); }
}

/* ---------- ошибка панели: «нет связи с бэкендом» + повтор через 10с ---------- */
function errFor(p) {
  const r = panelRoots[p];
  return (r && r.querySelector('.errbox')) || document.querySelector('.errbox');
}
function hideErr(p) { const b = errFor(p); if (b) b.classList.add('hidden'); }
function panelError(where, err, p) {
  probeSet(false);
  const box = errFor(p || activePanel());
  if (box) {
    box.classList.remove('hidden');
    box.textContent = 'нет связи с бэкендом (' + where + ': ' + (err && err.message ? err.message : 'ошибка') + ') — повтор через 10с';
  }
}
function scheduleRetry(fn, p) {
  clearTimeout(timers.retry[p || WIN]);
  timers.retry[p || WIN] = setTimeout(() => { try { fn(); } catch (_) {} }, 10000);
}
function refreshIv() { return Math.min(3600, Math.max(5, Number(settings['ui.autorefresh_sec']) || 30)) * 1000; }
function activePanel() { return WIN === 'projects' ? (activeTab || 'projects') : WIN; }
function fillChrome() {
  const f = chromeFillers[activePanel()];
  if (f) { try { f(); } catch (_) {} }
}

/* ---------- настройки: загрузка / сохранение / применение ---------- */
/* эффективные настройки берём для СВОЕЙ панели (?win=<label>): тогда
   переопределения окна реально действуют в этом окне. Неизвестный/не
   поддерживаемый бэкендом win → повтор без параметра (глобальные). */
async function fetchSettings() {
  const w = activePanel();
  try { return await api('/api/settings?win=' + encodeURIComponent(w)); }
  catch (_) { return await api('/api/settings'); }
}
async function loadSettings() {
  try {
    const d = await fetchSettings();
    settingsRawJson = JSON.stringify(d.settings || {});
    settings = Object.assign({}, DEFAULT_SETTINGS, d.settings || {});
    if (d.defaults) setDefaults = Object.assign({}, DEFAULT_SETTINGS, d.defaults);
    applySettings();
  } catch (_) {
    /* бэкенд без /api/settings — работаем на дефолтах, без шума в журнале */
  }
}
async function refreshSettingsSilent() {
  try {
    const d = await fetchSettings();
    const raw = JSON.stringify(d.settings || {});
    if (raw === settingsRawJson) return;
    settingsRawJson = raw;
    settings = Object.assign({}, DEFAULT_SETTINGS, d.settings || {});
    if (d.defaults) setDefaults = Object.assign({}, DEFAULT_SETTINGS, d.defaults);
    applySettings();
  } catch (_) { /* тихо: настройки факультативны */ }
}
/* PATCH /api/settings: без win — глобальное значение; с win — переопределение
   для этой панели; value === null с win — сброс переопределения. */
async function patchSetting(key, value, win) {
  const body = { [key]: value };
  if (win) body.win = win;
  try {
    const d = await api('/api/settings', jsonOpts('PATCH', body));
    if (d && d.settings) { settings = Object.assign({}, DEFAULT_SETTINGS, d.settings); settingsRawJson = JSON.stringify(d.settings); }
    if (d && d.defaults) setDefaults = Object.assign({}, DEFAULT_SETTINGS, d.defaults);
    applySettings();
    const shown = value === null ? 'переопределение сброшено' : String(value);
    log('✓ ' + key + (win ? ' [' + win + ']' : '') + ': ' + shown);
  } catch (e) {
    const rejected = String(e.message).indexOf('422') !== -1;
    log('✗ не сохранено ' + key + ': ' + (rejected ? 'бэкенд отклонил значение' : e.message));
  }
  refreshSettingsView(); /* no-op, если экран настроек закрыт */
  fillChrome();
}
/* «сбросить всё» (глобальные значения) — кнопка в разделе «глобальные» */
async function resetAllSettings() {
  const body = {};
  for (const k of Object.keys(setDefaults)) body[k] = setDefaults[k];
  log('сброс настроек…');
  try {
    const d = await api('/api/settings', jsonOpts('PATCH', body));
    if (d && d.settings) { settings = Object.assign({}, DEFAULT_SETTINGS, d.settings); settingsRawJson = JSON.stringify(d.settings); }
    applySettings();
    log('✓ настройки сброшены к значениям по умолчанию');
  } catch (e) { log('✗ сброс не удался: ' + e.message); }
  refreshSettingsView();
  fillChrome();
}
/* применить настройки к окну: статус-строка, зонд, интервалы, формат времени, вкладки */
function applySettings() {
  document.body.dataset.sb = settings['ui.statusbar'] || 'always';
  const pr = $('#probe');
  if (pr) pr.classList.toggle('hidden', settings['ui.show_probe'] === false);
  const iv = refreshIv();
  if (iv !== curRefreshIv) {
    curRefreshIv = iv;
    for (const k of ['sources']) if (TICK_LOADERS[k]) onTick(k, iv, TICK_LOADERS[k]);
    if (panelRoots.board) armBoardTick(true); /* доска: 10 с, пока есть бегущая делегация */
    /* herdr: свой потолок 10 с, но не чаще ui.autorefresh_sec */
    if (TICK_LOADERS.herdr) onTick('herdr', Math.min(HERDR_IV_MAX, iv), TICK_LOADERS.herdr);
  }
  const niv = notifyIv();
  if (niv !== curNotifyIv) {
    curNotifyIv = niv;
    onTick('notify', niv, loadNotifications);
  }
  if ((settings['ui.time_format'] || 'relative') === 'absolute') {
    if (panelRoots.projects && projCache.length) renderProjects(projCache);
    if (panelRoots.sources && srcCache.length) renderSources(srcCache);
    updateProbeText();
  }
  fillChrome();
  if (WIN === 'projects') renderTabsUi();
  renderSettingsForm(); /* no-op, если экран настроек закрыт */
}

/* ---------- тема (omarchy) ---------- */
const THEME_KEYS = ['accent', 'selection', 'muted', 'background', 'dark_background',
  'darker_background', 'lighter_background', 'foreground', 'light_foreground',
  'bright_foreground', 'red', 'yellow', 'green', 'cyan', 'blue'];

/* цвет из var-строки: '#rgb' | '#rrggbb' | 'rgb(...)' | 'rgba(...)' → [r,g,b] */
function cssRGB(v, fb) {
  const s = String(v || '').trim();
  let m = s.match(/^#([0-9a-f]{6})$/i);
  if (m) return [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16));
  m = s.match(/^#([0-9a-f]{3})$/i);
  if (m) return [0, 1, 2].map((i) => parseInt(m[1][i] + m[1][i], 16));
  m = s.match(/^rgba?\(([^)]+)\)$/i);
  if (m) return m[1].split(/[,\/\s]+/).filter(Boolean).slice(0, 3).map((x) => Math.round(parseFloat(x)));
  return fb;
}
/* производные цвета — в КОНКРЕТНЫЕ rgb/rgba вместо color-mix(): style-recalc
   на горячем пути (:hover строки грида, см. --hover-row/--row-border в css)
   больше не вычисляет color-mix для каждого элемента. Значения те же самые —
   только посчитанные один раз, а не на каждую перерисовку стилей. */
function applyDerivedColors() {
  const cs = getComputedStyle(document.documentElement);
  const g = (k, fb) => cssRGB(cs.getPropertyValue('--palette-' + k), fb);
  const sel = g('selection', [19, 78, 90]);
  const light = g('light-foreground', [167, 201, 198]);
  const darkBg = g('dark-background', [3, 18, 34]);
  const green = g('green', [2, 131, 145]);
  const bright = g('bright-foreground', [246, 220, 172]);
  const lighterBg = g('lighter-background', [10, 37, 64]);
  const darkerBg = g('darker-background', [2, 12, 23]);
  const mix = (a, b, wa) => a.map((v, i) => Math.round(v * wa + b[i] * (1 - wa)));
  const st = document.documentElement.style;
  st.setProperty('--hover-row', 'rgba(' + sel.join(',') + ',0.45)');
  st.setProperty('--row-border', 'rgba(' + darkerBg.join(',') + ',0.7)');
  st.setProperty('--dim-fg', 'rgb(' + mix(light, darkBg, 0.75).join(',') + ')');
  st.setProperty('--ok-fg', 'rgb(' + mix(green, bright, 0.55).join(',') + ')');
  st.setProperty('--bevel-mid', 'rgb(' + mix(lighterBg, darkerBg, 0.5).join(',') + ')');
}

async function fetchTheme() {
  try {
    const t = await api('/api/theme');
    if (state.themeMtime === t.mtime) { applyDerivedColors(); termApplyTheme(); return; }
    state.themeMtime = t.mtime;
    const st = document.documentElement.style;
    for (const k of THEME_KEYS) {
      if (t.palette && t.palette[k]) st.setProperty('--palette-' + k.replace(/_/g, '-'), t.palette[k]);
    }
    document.documentElement.dataset.mode = t.mode || '';
  } catch (_) { /* бэкенд недоступен — работаем на фолбэке */ }
  applyDerivedColors(); /* производные — из актуальных --palette-* (или фолбэка) */
  termApplyTheme(); /* панель «Терминал» пересобирает тему из --palette-* */
}

/* ---------- окна: свернуть во вкладку / вернуть из вкладки ---------- */
async function winMinimize(w) {
  log('свёртывание «' + (TITLES[w] || w) + '» во вкладку…');
  try {
    const r = await api('/api/windows/minimize', jsonOpts('POST', { win: w }));
    if (r && r.ok) log('✓ свернуто: ' + String(r.detail || r.plan || 'ок') + (Array.isArray(r.tabs) ? ' · вкладки: ' + r.tabs.join(', ') : ''));
    else log('✗ свернуть не удалось: ' + ((r && r.detail) || 'причина не указана'));
  } catch (e) {
    log('✗ свернуть не удалось: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/minimize' : e.message));
  }
  if (WIN === 'projects') pollUi();
}
async function winRestore(w) {
  log('восстановление «' + (TITLES[w] || w) + '» в отдельное окно…');
  try {
    const r = await api('/api/windows/restore', jsonOpts('POST', { win: w }));
    if (r && r.ok) log('✓ восстановлено: ' + String(r.detail || r.plan || 'ок'));
    else log('✗ восстановить не удалось: ' + ((r && r.detail) || 'причина не указана'));
  } catch (e) {
    log('✗ восстановить не удалось: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/restore' : e.message));
  }
  if (WIN === 'projects') pollUi();
}

/* ---------- GET /api/ui: список окон/вкладок (хост опрашивает раз в 5с) ---------- */
async function pollUi() {
  try { uiCache = await api('/api/ui'); } catch (_) { uiCache = null; }
  if (WIN === 'projects') renderTabsUi();
  if ($('#set-wins')) renderWindowsSection();
  applyWindowGate();
}
function renderTabsUi() {
  const bar = $('#tabbar');
  if (!bar) return;
  const tabs = (uiCache && Array.isArray(uiCache.tabs) ? uiCache.tabs : [])
    .filter((p) => p !== 'projects' && BUILDERS[p]);
  /* вкладка исчезла (восстановлена в окно) — возвращаемся к списку проектов */
  if (activeTab && tabs.indexOf(activeTab) === -1) {
    activeTab = null;
    const hp = $('#host-projects'), ht = $('#host-tab');
    if (hp) hp.classList.remove('hidden');
    if (ht) ht.classList.add('hidden');
    fillChrome();
  }
  const show = settings['ui.tabbar'] !== 'off' && tabs.length > 0;
  bar.classList.toggle('hidden', !show);
  bar.innerHTML = '';
  if (!show) return;
  bar.appendChild(mkTab('projects', !activeTab));
  for (const p of tabs) bar.appendChild(mkTab(p, activeTab === p));
}
function mkTab(p, isActive) {
  const t = el('div', 'tab95' + (isActive ? ' active' : ''));
  t.appendChild(el('span', null, esc(TITLES[p] || p)));
  t.title = p === 'projects' ? 'показать список проектов'
    : 'показать «' + (TITLES[p] || p) + '» внутри главного окна';
  t.addEventListener('click', () => (p === 'projects' ? showHostProjects() : activateTab(p)));
  if (p !== 'projects') {
    const up = el('button', 'mini', '↗');
    up.type = 'button';
    up.title = 'вернуть «' + (TITLES[p] || p) + '» в отдельное окно';
    up.addEventListener('click', (e) => { e.stopPropagation(); winRestore(p); });
    t.appendChild(up);
  }
  return t;
}
/* лениво строит панель во вкладке хоста; builder — строго один раз на панель */
function activateTab(p) {
  if (!p || p === 'projects') return showHostProjects();
  activeTab = p;
  const hostTab = $('#host-tab');
  let cont = document.getElementById('tab-' + p);
  if (!cont) {
    cont = el('div', 'tabpage hidden');
    cont.id = 'tab-' + p;
    hostTab.appendChild(cont);
  }
  if (!builtTabs.has(p)) {
    builtTabs.add(p);
    try { BUILDERS[p](cont); } catch (e) { log('ошибка отрисовки: ' + e.message); }
  }
  for (const n of hostTab.children) n.classList.toggle('hidden', n !== cont);
  $('#host-projects').classList.add('hidden');
  hostTab.classList.remove('hidden');
  refreshSettingsSilent(); /* у вкладки свои переопределения — применяем её эффективные */
  fillChrome();
  renderTabsUi();
}
function showHostProjects() {
  activeTab = null;
  const hp = $('#host-projects'), ht = $('#host-tab');
  if (hp) hp.classList.remove('hidden');
  if (ht) ht.classList.add('hidden');
  refreshSettingsSilent();
  fillChrome();
  renderTabsUi();
}

/* ---------- кнопки «⌄ свернуть» / «⚙ настройки» в тулбарах.
   Строится в момент постройки панели: у вкладки хоста activePanel() — её панель,
   у отдельного окна — WIN. Поэтому хост-проекты без «⌄», вкладки — с «⌄». */
function winControlsHtml() {
  const parts = [];
  const p = activePanel();
  if (p !== 'projects') parts.push('<button class="mini" type="button" data-winact="minimize" title="свернуть это окно во вкладку главного окна">⌄ свернуть</button>');
  parts.push('<button class="mini" type="button" data-winact="settings" title="настройки этого окна: глобальные значения и переопределения для этой панели">⚙ настройки</button>');
  return parts.join(' ');
}
/* какая панель соответствует корню, в который встроен тулбар (у хоста-вкладки
   это её tabpage, у отдельного окна — #app) */
function panelOfRoot(root) {
  for (const k of Object.keys(panelRoots)) if (panelRoots[k] === root) return k;
  return activePanel();
}
function bindWinControls(root) {
  root.addEventListener('click', (e) => {
    const b = e.target && e.target.closest ? e.target.closest('[data-winact]') : null;
    if (!b) return;
    /* в хосте «⌄ свернуть» из вкладки относится к панели вкладки, не к хосту */
    if (b.dataset.winact === 'minimize') winMinimize(activePanel());
    else openSettingsView(panelOfRoot(root), root); /* ⚙ — режим настроек на месте */
  });
  applyWindowGate(root);
}
/* ---------- честное состояние оконных операций (GET /api/ui: gui, port_owner):
   без GUI или когда порт держит терминал-сервер — кнопки гасим заранее,
   чтобы клик не заканчивался ошибкой. ---------- */
function windowOpsBlocked() {
  return !!uiCache && (uiCache.gui === false || uiCache.port_owner === 'other');
}
function gateTooltip() {
  if (uiCache && uiCache.port_owner === 'other') return 'терминал-сервер владеет портом; запусти GUI ярлыком atlas-gui';
  if (uiCache && uiCache.gui === false) return 'GUI не запущен; запусти ярлыком atlas-gui';
  return '';
}
function applyWindowGate(root) {
  const blocked = windowOpsBlocked();
  const tip = gateTooltip();
  (root || document).querySelectorAll('[data-winact]').forEach((b) => {
    /* ⚙ — локальное действие (режим настроек на месте), GUI не требуется:
       гасим только оконные операции (⌄ свернуть) */
    if (b.dataset.winact !== 'minimize') {
      b.disabled = false;
      b.classList.remove('gate-dim');
      b.title = 'настройки этого окна: глобальные значения и переопределения для этой панели';
      return;
    }
    b.disabled = blocked;
    b.classList.toggle('gate-dim', blocked);
    b.title = blocked ? tip : 'свернуть это окно во вкладку главного окна';
  });
}

/* ---------- пуш темы: SSE GET /api/theme/stream ----------
   ОДИН EventSource на всё приложение (переживает смены панелей и вкладки).
   Событие → немедленный apply-путь fetchTheme() (xterm перекрашивается тем же
   хуком). Ошибка/404 (старый бэкенд) — остаётся 30-с опрос ниже; попытка
   восстановить стрим раз в 60 с. visibilitychange-опрос сохранён. */
let themeStream = null;
let themeStreamRetry = null;
function themeStreamStart() {
  if (themeStream || typeof EventSource !== 'function') return;
  try {
    const es = new EventSource(API + '/api/theme/stream');
    themeStream = es;
    es.onmessage = () => { fetchTheme(); };
    es.onerror = () => {
      themeStreamStop();
      if (!themeStreamRetry) themeStreamRetry = setTimeout(() => { themeStreamRetry = null; themeStreamStart(); }, 60000);
    };
  } catch (_) { /* EventSource недоступен — работает 30-с опрос */ }
}
function themeStreamStop() {
  if (themeStream) { try { themeStream.close(); } catch (_) {} themeStream = null; }
}
/* ---------- реестр строителей панелей (переехал из «запуска») ---------- */
const BUILDERS = {
  projects: buildProjects, board: buildBoard, deps: buildDeps, search: buildSearch,
  sources: buildSources, herdr: buildHerdr, view: buildView, terminal: buildTerminal,
  orchestrator: buildOrchestrator,
};

/* uiCache перезаписывают и панельные открытия (поиск/терминал/оркестратор) —
   ES-модуль не даёт импортёру присваивать let, поэтому запись — через функцию */
function setUiCache(u) { uiCache = u; }

/* старт уведомлений (переехал из «запуска»: boot не должен писать чужой let) */
function armNotifications() {
  curNotifyIv = notifyIv();
  loadNotifications();
  onTick('notify', curNotifyIv, loadNotifications);
}

export { BUILDERS, DEFAULT_SETTINGS, PANELS, TITLES, WIN, activateTab, activePanel, activeTab, applyDerivedColors, armNotifications, bindWinControls, ch, chromeFillers, curRefreshIv, fetchTheme, fillChrome, fmtAge, hideErr, lastSyncSec, loadNotifications, loadSettings, panelError, panelRoots, patchSetting, pollUi, probe, probeSet, refreshIv, refreshSettingsSilent, resetAllSettings, scheduleRetry, setChrome, setUiCache, settings, state, themeStreamStart, timers, uiCache, winControlsHtml, winMinimize, winRestore };
