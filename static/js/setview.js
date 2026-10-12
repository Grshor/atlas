/* static/js/setview.js — настройки «на месте»: слой .setview поверх панели + раздел «Окна».
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc, log } from './lib/dom.js';
import { DEFAULT_SETTINGS, PANELS, TITLES, patchSetting, pollUi, resetAllSettings, settings, uiCache, winMinimize, winRestore } from './core.js';
import { api } from './lib/api.js';
import { mkSelect, mselCloseOpen } from './lib/menu.js';
import { resetProjCols } from './projects.js';

const SET_SECTIONS = [
  { h: 'Хром', rows: [
    { key: 'ui.statusbar', label: 'статус-строка', type: 'select', options: [['always', 'всегда'], ['hover', 'при наведении'], ['hidden', 'скрыта']] },
    { key: 'ui.tabbar', label: 'вкладки в главном окне', type: 'select', options: [['on', 'включены'], ['off', 'выключены']] },
    { key: 'ui.show_probe', label: 'показывать «бэкенд: ok · синк N»', type: 'check' },
  ] },
  { h: 'Обновление', rows: [
    { key: 'ui.autorefresh_sec', label: 'интервал автообновления панелей, с', type: 'num', min: 5, max: 3600 },
  ] },
  { h: 'Поиск', rows: [
    { key: 'ui.search_k', label: 'результатов на запрос (k)', type: 'num', min: 1, max: 200 },
    { key: 'ui.show_hybrid_details', label: 'показывать детали гибрида (модель, мс)', type: 'check' },
  ] },
  { h: 'Время', rows: [
    { key: 'ui.time_format', label: 'формат времени', type: 'select', options: [['relative', 'относительное (2м)'], ['absolute', 'абсолютное (дата, время)']] },
  ] },
  { h: 'Обходы', rows: [
    { key: 'patrol.enabled', label: 'обходы включены', type: 'check' },
    { key: 'patrol.interval_min', label: 'минут между обходами (15..10080)', type: 'num', min: 15, max: 10080,
      hint: 'минут между обходами; первый обход ≈ через 2 мин после старта сервера' },
  ] },
];
const SET_WIN_SECTIONS = [{ h: '', rows: SET_SECTIONS.reduce((a, s) => a.concat(s.rows), []) }];
/* =====================================================================
   настройки «на месте»: ⚙ переключает ТЕКУЩУЮ панель в режим настроек —
   поверх её .panel кладётся слой .setview; DOM и состояние панели не трогаются,
   «✕ закрыть»/Esc просто снимает слой. Разделы: «глобальные» (PATCH без win),
   «это окно — <Имя>» (PATCH с win, «сбросить» → null), «Окна», «Таблицы».
   =================================================================== */
let setView = null; /* { label, root, el, data:{settings,global,windows}, key } */

function renderWindowsSection() {
  const box = $('#set-wins');
  if (!box) return;
  box.innerHTML = '';
  const panels = (uiCache && Array.isArray(uiCache.panels) && uiCache.panels.length ? uiCache.panels : PANELS).slice();
  if (!uiCache) box.appendChild(el('div', 'dim', 'нет данных: бэкенд не отвечает на /api/ui'));
  const host = (uiCache && uiCache.host) || 'projects';
  const wins = uiCache && Array.isArray(uiCache.windows) ? uiCache.windows : [];
  const tabs = uiCache && Array.isArray(uiCache.tabs) ? uiCache.tabs : [];
  for (const p of panels) {
    const inTab = tabs.indexOf(p) !== -1;
    const inWin = wins.indexOf(p) !== -1;
    const row = el('div', 'setwin');
    row.appendChild(el('span', null, '<b>' + esc(TITLES[p] || p) + '</b>'));
    row.appendChild(el('span', 'st dim', inTab ? 'во вкладке' : inWin ? 'в окне' : 'закрыто'));
    if (p === host) {
      row.appendChild(el('span', 'st dim', '· хост-окно, не сворачивается'));
    } else {
      const b = el('button', 'mini', inTab ? '↗ в окно' : (inWin ? '⌄ во вкладку' : '↗ открыть'));
      b.type = 'button';
      b.disabled = !uiCache;
      b.title = inTab ? 'вернуть панель в отдельное окно'
        : (inWin ? 'свернуть окно во вкладку главного окна' : 'открыть панель отдельным окном');
      b.addEventListener('click', async () => {
        if (inTab || !inWin) await winRestore(p);
        else await winMinimize(p);
        pollUi();
      });
      row.appendChild(b);
    }
    box.appendChild(row);
  }
}

/* нормализация ответа /api/settings[?win=]: новый бэкенд отдаёт
   {settings(effective),global,windows}; старый — только {settings,defaults},
   тогда global := settings, windows := {}. */
function normSettings(d) {
  const eff = Object.assign({}, DEFAULT_SETTINGS, (d && d.settings) || {});
  const globalV = Object.assign({}, DEFAULT_SETTINGS, (d && d.global) || (d && d.settings) || {});
  const windows = (d && d.windows) || {};
  return { settings: eff, global: globalV, windows };
}

/* одна строка настройки: select | checkbox | number; commit(value) — сразу PATCH */
function setRow(row, value, win, commit) {
  const r = el('div', 'setrow');
  const id = 'sv-' + row.key.replace(/\W/g, '-') + (win ? '-w' : '-g');
  const lab = el('label', null, esc(row.label));
  lab.setAttribute('for', id);
  if (row.hint) lab.title = row.hint;
  r.appendChild(lab);
  let inp;
  if (row.type === 'select') {
    inp = mkSelect({
      id: id,
      options: row.options.map(([v, t]) => ({ value: v, label: t })),
      value: String(value),
      onchange: (v) => commit(v),
    });
  } else if (row.type === 'check') {
    inp = document.createElement('input'); inp.type = 'checkbox'; inp.id = id;
    inp.checked = value !== false;
    inp.addEventListener('change', () => commit(inp.checked));
  } else {
    inp = document.createElement('input'); inp.type = 'number'; inp.className = 'fld'; inp.id = id;
    if (row.min != null) inp.min = row.min;
    if (row.max != null) inp.max = row.max;
    inp.value = String(value);
    inp.addEventListener('change', () => {
      const v = parseInt(inp.value, 10);
      if (!Number.isFinite(v) || (row.min != null && v < row.min) || (row.max != null && v > row.max)) {
        renderSettingsForm(); log('✗ недопустимое значение: ' + row.key); return;
      }
      commit(v);
    });
  }
  inp.dataset.setkey = row.key;
  inp.dataset.winscope = win || '';
  if (row.hint) inp.title = row.hint;
  r.appendChild(inp);
  if (row.hint) {
    const h = el('span', 'set-hint dim', esc(row.hint));
    h.title = row.hint;
    r.appendChild(h);
  }
  return r;
}

/* раздел настроек: несколько setrow + бейдж/«сбросить» при переопределении */
function buildSetSection(section, values, win, overrides) {
  const sec = el('section', 'set-sec');
  if (section.h) sec.appendChild(el('h4', null, esc(section.h)));
  for (const row of section.rows) {
    const line = setRow(row, values[row.key], win, (v) => patchSetting(row.key, v, win || null));
    if (overrides && overrides[row.key] !== undefined) {
      line.appendChild(el('span', 'ovr-badge', 'переопределено'));
      const rst = el('button', 'mini reset', 'сбросить');
      rst.type = 'button';
      rst.title = 'убрать переопределение для этой панели (PATCH null + win)';
      rst.addEventListener('click', () => patchSetting(row.key, null, win));
      line.appendChild(rst);
    }
    sec.appendChild(line);
  }
  return sec;
}

async function openSettingsView(label, root) {
  closeSettingsView();
  const panelEl = root.querySelector('.panel') || root;
  const host = el('div', 'setview');
  const header = el('header');
  header.appendChild(el('span', 'ttl', 'настройки — ' + esc(TITLES[label] || label)));
  header.appendChild(el('span', 'spacer'));
  const x = el('button', 'mini', '✕ закрыть');
  x.type = 'button';
  x.title = 'вернуться к панели без потери состояния (Esc)';
  x.addEventListener('click', closeSettingsView);
  header.appendChild(x);
  host.appendChild(header);
  host.appendChild(el('div', 'set-scroll'));
  panelEl.appendChild(host);
  setView = { label, root, el: host, data: null, key: null };
  setView.key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); closeSettingsView(); } };
  document.addEventListener('keydown', setView.key, true);
  await refreshSettingsView();
}

function closeSettingsView() {
  if (!setView) return;
  mselCloseOpen(); /* список живёт в body — слой снимаем вместе с ним */
  if (setView.key) document.removeEventListener('keydown', setView.key, true);
  if (setView.el && setView.el.parentNode) setView.el.parentNode.removeChild(setView.el);
  setView = null;
}

async function refreshSettingsView() {
  if (!setView) return;
  try {
    const d = await api('/api/settings?win=' + encodeURIComponent(setView.label));
    setView.data = normSettings(d);
  } catch (_) {
    setView.data = { settings: settings, global: settings, windows: {} };
  }
  renderSettingsForm();
}

function renderSettingsForm() {
  if (!setView) return;
  mselCloseOpen(); /* строки пересобираются — открытый список снимаем */
  const scroll = setView.el.querySelector('.set-scroll');
  if (!scroll) return;
  scroll.innerHTML = '';
  const d = setView.data || { settings: settings, global: settings, windows: {} };
  const label = setView.label;
  const overrides = (d.windows && d.windows[label]) || {};

  const ghead = el('section', 'set-sec');
  ghead.appendChild(el('h4', null, 'глобальные'));
  scroll.appendChild(ghead);
  for (const s of SET_SECTIONS) scroll.appendChild(buildSetSection(s, d.global, null, null));
  const grow = el('section', 'set-sec');
  const growRow = el('div', 'setrow');
  const rb = el('button', 'btn95', 'сбросить всё');
  rb.type = 'button';
  rb.title = 'вернуть все глобальные настройки к значениям по умолчанию';
  rb.addEventListener('click', resetAllSettings);
  growRow.appendChild(rb);
  grow.appendChild(growRow);
  scroll.appendChild(grow);

  const wsec = el('section', 'set-sec');
  wsec.appendChild(el('h4', null, 'это окно — ' + esc(TITLES[label] || label)));
  scroll.appendChild(wsec);
  for (const s of SET_WIN_SECTIONS) scroll.appendChild(buildSetSection(s, d.settings, label, overrides));
  scroll.appendChild(el('div', 'set-note', 'переопределения этого окна действуют только в этой панели («' + esc(TITLES[label] || label) + '»)'));

  const osec = el('section', 'set-sec');
  osec.appendChild(el('h4', null, 'Окна'));
  const wins = el('div', 'setwins');
  wins.id = 'set-wins';
  osec.appendChild(wins);
  scroll.appendChild(osec);

  const tsec = el('section', 'set-sec');
  tsec.appendChild(el('h4', null, 'Таблицы'));
  const trow = el('div', 'setrow');
  const cr = el('button', 'btn95', 'сбросить ширины колонок');
  cr.type = 'button';
  cr.title = 'очистить сохранённые ширины и раскладку колонок грида проектов';
  cr.addEventListener('click', resetProjCols);
  trow.appendChild(cr);
  tsec.appendChild(trow);
  scroll.appendChild(tsec);

  renderWindowsSection();
}

export { openSettingsView, refreshSettingsView, renderSettingsForm, renderWindowsSection };
