/* static/js/projects.js — панель «Проекты»: грид Tabulator, архив/избранное, выбор (localStorage-синк), фокус, задачи make.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, copyText, el, esc, log } from './lib/dom.js';
import { api, jsonOpts, localOnlyUrl } from './lib/api.js';
import { applySearchFilter, searchProjectVault } from './search.js';
import { askDialog } from './lib/modal.js';
import { bindWinControls, ch, chromeFillers, fmtAge, hideErr, panelError, panelRoots, probeSet, scheduleRetry, setChrome, state, winControlsHtml } from './core.js';
import { boardCache, renderBoard } from './board.js';
import { depsCache, renderDeps } from './deps.js';
import { fmtSecs } from './lib/fmt.js';
import { mkMenu, mkSelect } from './lib/menu.js';
import { openInView } from './view.js';
import { openOrchestratorPanel } from './orchestrator.js';
import { openTerminalPanel } from './terminal.js';
import { smoothCancel } from './lib/smooth.js';

let projCache = [];
let statsProj = { shown: 0, total: 0, arch: 0, archShown: 0 };
/* ---------- POST /api/focus + журнал в статус-строку ----------
   Кнопки строятся из GET /api/focus/targets; если тот недоступен — четыре
   прежние. plan/reused показываем в логе и во всплывающей подсказке. */
/* Фолбэк целей фокуса (живой список приходит из GET /api/focus/targets):
   H/T — herdr, W — терминал atlas (проектная сессия). O убран. */
const FOCUS_DEFAULT = [
  { target: 'herdr_workspace', letter: 'H', label: 'herdr workspace' },
  { target: 'herdr_tab', letter: 'T', label: 'herdr tab' },
  { target: 'terminal', letter: 'W', label: 'терминал atlas' },
];
const PLAN_LABELS = { focus: 'переключение', create: 'создание', reuse: 'переиспользование', none: 'без изменений' };
let focusTargets = FOCUS_DEFAULT;
async function loadFocusTargets() {
  try {
    const d = await api('/api/focus/targets');
    if (d && Array.isArray(d.targets) && d.targets.length) {
      focusTargets = d.targets.filter((t) => t && t.target && t.target !== 'dry_run');
    }
  } catch (_) { /* цели не объявлены — остаются четыре кнопки по умолчанию */ }
}

/* краткий ✓/✗ в ячейке «фокус» (детали — в title), исчезает через 4с.
   Состояние живёт в fbMap (на проект), поэтому перерисовки грида его держат. */
const fbMap = new Map(); /* имя проекта → {ok, detail, until} */
function rowFeedback(name, ok, detail) {
  fbMap.set(name, { ok: !!ok, detail: detail || '', until: Date.now() + 4000 });
  reformatProjRow(name);
  setTimeout(() => {
    const cur = fbMap.get(name);
    if (cur && cur.until <= Date.now()) { fbMap.delete(name); reformatProjRow(name); }
  }, 4100);
}
function reformatProjRow(name) {
  if (!projReady()) return;
  try { const row = projTable.getRow(name); if (row) row.reformat(); } catch (_) {}
}

async function focusProject(project, target, label) {
  log('фокус: ' + (label || target) + ' → ' + project + '…');
  try {
    /* dry_run сознательно не отправляем: клик — настоящее действие */
    const r = await api('/api/focus', jsonOpts('POST', { project, target }));
    const extra = [];
    if (r.plan) extra.push('план: ' + (PLAN_LABELS[r.plan] || r.plan));
    if (r.reused) extra.push('переиспользовано готовое');
    log((r.ok ? '✓ ' : '✗ ') + (r.detail || (r.ok ? 'ok' : 'ошибка')) + (extra.length ? ' [' + extra.join(', ') + ']' : ''));
    rowFeedback(project, !!r.ok, (r.detail || '') + (extra.length ? ' · ' + extra.join(', ') : ''));
    probeSet(true);
    /* W-фокус терминала вернул сессию проекта — открываем «Терминал» на ней */
    if (r.ok && r.session) openTerminalPanel(String(r.session));
  } catch (e) {
    log('✗ нет связи с бэкендом: ' + (e && e.message ? e.message : 'ошибка'));
    rowFeedback(project, false, e && e.message ? e.message : 'ошибка');
    probeSet(false);
  }
}

/* ---------- выбор проекта ---------- */
function selectProject(name) {
  if (state.selected === name) return;
  state.selected = name;
  try { localStorage.setItem('atlas.selected', name); } catch (_) {}
  log('выбран проект: ' + name);
  onSelectionChanged();
}

/* реакция текущих панелей на смену выбора (в т.ч. из других окон) */
function onSelectionChanged() {
  try {
    if (projTable) syncProjSelection();
    if (panelRoots.projects) renderArchived(archCache);
    updateArchBtn(); /* при старой таблице это делал renderProjects внутри onSelectionChanged */
    if (panelRoots.board && boardCache) renderBoard(boardCache);
    if (panelRoots.deps && depsCache) renderDeps(depsCache);
    if (panelRoots.search) applySearchFilter();
    if (panelRoots.projects) loadMakeTasks();
  } catch (_) {}
}
/* грид держит одну выбранную строку; сюда приходят и чужие окна (storage) */
function syncProjSelection() {
  if (!projReady()) return;
  selSyncing = true;
  try {
    const cur = projTable.getSelectedData().map((d) => d.name);
    for (const s of cur) if (s !== state.selected) projTable.deselectRow(s);
    if (state.selected && !cur.includes(state.selected)) {
      const row = projTable.getRow(state.selected);
      if (row) projTable.selectRow(row);
    }
  } catch (_) {}
  selSyncing = false;
}
/* хранилище могло поменяться, пока окно было не в фокусе */
function syncSelectedFromStore() {
  let v = null;
  try { v = localStorage.getItem('atlas.selected'); } catch (_) {}
  if (v !== state.selected) { state.selected = v; onSelectionChanged(); }
}
window.addEventListener('storage', (e) => { if (!e.key || e.key === 'atlas.selected') syncSelectedFromStore(); });
window.addEventListener('focus', syncSelectedFromStore);
/* =====================================================================
   панель «проекты» (+ архив, изменяемые колонки, кнопки целей фокуса)
   =================================================================== */
const SRC_BADGES = [['herdr', 'H'], ['omp', 'S'], ['git', 'G'], ['vault', 'V'], ['zen', 'Z']];

function agentIcon(p) {
  if (!p.agents_count) return '<span class="ag-none">○ нет</span>';
  const working = (p.agent_status || '') === 'working';
  const icon = working ? '<span class="ag-work">◆</span>' : '<span class="ag-ok">●</span>';
  return icon + ' ' + esc(p.agent_status || '—') + ' (' + p.agents_count + ')';
}

/* кнопка «⤓ выбросить вниз» / «⬆ вернуть» в тулбаре: активна, когда выбран ряд */
function updateArchBtn() {
  const b = $('#btn-arch');
  if (!b) return;
  const p = projCache.find((x) => x.name === state.selected) || null;
  b.disabled = !p;
  if (p && p.archived) {
    b.textContent = '⬆ вернуть';
    b.title = 'вернуть «' + p.name + '» из подвала в основной список';
  } else {
    b.textContent = '⤓ выбросить вниз';
    b.title = 'в подвал, ничего не удаляется';
  }
}

/* ---------- якорение прокрутки при перерисовке списка проектов ----------
   Архив/возврат перерисовывает грид (setData) и подвал; содержимое скроллера
   `.scroll.well.grow` на миг схлопывается, и браузер сбрасывает scrollTop в
   начало. Вместо копирования scrollTop запоминаем ЯКОРЬ: имя верхней видимой
   строки (первой [data-name] — в гриде и в подвале — чей верх не выше верха
   скроллера) и её сдвиг от верха скроллера. Действующая строка (её выбрасы-
   вают/возвращают) якорем не бывает: после перерисовки её имя найдётся в
   другом контейнере и утащит прокрутку к нему. После перерисовки возвращаем
   прокрутку по якорю; если якорной строки больше нет — берём соседнюю по
   порядку: следующую, затем предыдущую, в крайнем случае просто зажимаем
   старый scrollTop в новые границы (честный кламп у дна).
   smoothWheel гасим: его rAF-анимация спорила бы с восстановлением. */
function projScroller() {
  return panelRoots.projects ? panelRoots.projects.querySelector('.scroll.well.grow') : null;
}

/* строка реально видима: у содержимого ЗАКРЫТОГО details в chromium остаётся
   геометрия (высота > 0), хотя не рисуется и в якоря оно годиться не может */
function projRowVisible(r) {
  return r.getBoundingClientRect().height > 0 && (typeof r.checkVisibility !== 'function' || r.checkVisibility());
}

function projSnapScroll(sc, skipName) {
  const snap = { scrollTop: sc.scrollTop, name: null, off: 0, next: null, prev: null };
  /* действующую строку в якоря не берём: после перерисовки её имя появится
     в ДРУГОМ контейнере (грид ↔ подвал) и утащит прокрутку к себе */
  const rows = Array.from(sc.querySelectorAll('[data-name]'))
    .filter((r) => r.dataset.name !== skipName && projRowVisible(r));
  const stop = sc.getBoundingClientRect().top;
  let idx = rows.findIndex((r) => r.getBoundingClientRect().top >= stop - 0.5);
  if (idx < 0 && rows.length) idx = rows.length - 1; /* виден только хвост последней строки */
  if (idx >= 0) {
    snap.name = rows[idx].dataset.name;
    snap.off = rows[idx].getBoundingClientRect().top - stop;
    if (rows[idx + 1]) snap.next = rows[idx + 1].dataset.name;
    if (rows[idx - 1]) snap.prev = rows[idx - 1].dataset.name;
  }
  return snap;
}

function projApplySnap(sc, snap) {
  if (!sc || !snap) return;
  smoothCancel(sc); /* анимация колеса не должна доигрывать поверх восстановления */
  let row = null;
  for (const nm of [snap.name, snap.next, snap.prev]) {
    if (!nm) continue;
    row = Array.from(sc.querySelectorAll('[data-name]')).find((r) =>
      r.dataset.name === nm && projRowVisible(r)) || null;
    if (row) break;
  }
  let top = snap.scrollTop;
  if (row) top = row.getBoundingClientRect().top - sc.getBoundingClientRect().top + sc.scrollTop - snap.off;
  const max = sc.scrollHeight - sc.clientHeight;
  sc.scrollTop = Math.max(0, Math.min(top, max));
}

async function setArchived(p, archived) {
  log((archived ? 'архивирование' : 'возврат') + ': ' + p.name + '…');
  try {
    const r = await api('/api/projects/archive', jsonOpts('POST', { name: p.name, archived }));
    if (r && r.ok) {
      /* якорь снимаем в последний момент перед перерисовкой;
         саму действующую строку якорем не назначаем */
      const sc = projScroller();
      const snap = sc ? projSnapScroll(sc, p.name) : null;
      p.archived = archived;
      renderProjects(projCache).then(() => projApplySnap(sc, snap));
      log(archived ? 'архивирован: ' + p.name + ' → подвал' : 'возвращён: ' + p.name);
    } else log('✗ не удалось: ' + ((r && r.detail) || 'причина не указана'));
  } catch (e) {
    log('✗ архив недоступен: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/projects/archive' : e.message));
  }
}

/* ---------- избранное: ★/☆ (кнопка в ячейке фокуса и пункт меню) ----------
   Бэкенд отдаёт избранное первым, поэтому после POST список ПЕРЕЧИТЫВАЕМ и
   перерисовываем тем же якорным путём, что архив/возврат: порядок строк
   меняется (своя уезжает наверх), а прокрутка под курсором должна остаться
   на месте. Свою строку якорем не берём — иначе прокрутка ускачет за ней.
   Ключа favorite в строке нет (старый бэкенд) — ни кнопки, ни значка, ни
   пункта меню, и ничего не логируем. */
async function setFavorite(p, favorite) {
  log('избранное: ' + p.name + '…');
  try {
    const r = await api('/api/projects/favorite', jsonOpts('POST', { name: p.name, favorite }));
    if (r && r.ok) {
      const d = await api('/api/projects'); /* порядок задаёт бэкенд: избранное наверх */
      projCache = Array.isArray(d.projects) ? d.projects : projCache;
      const sc = projScroller();
      const snap = sc ? projSnapScroll(sc, p.name) : null;
      renderProjects(projCache).then(() => projApplySnap(sc, snap));
      log(favorite ? 'добавлен в избранное: ' + p.name : 'убран из избранного: ' + p.name);
    } else log('✗ не удалось: ' + ((r && r.detail) || 'причина не указана'));
  } catch (e) {
    log('✗ избранное недоступно: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/projects/favorite' : e.message));
  }
}

/* состояние избранного берём из данных строки; ключа favorite нет — фичи нет */
function favKnown(d) { return !!d && typeof d.favorite === 'boolean'; }
function favMark(d) {
  return d && d.favorite === true ? '<span class="favmark" title="избранное">★</span>' : '';
}
function favMenuItem(d) {
  if (!favKnown(d)) return null;
  const on = d.favorite === true;
  return { label: on ? 'Убрать из избранного' : 'В избранное', action: () => setFavorite(d, !on) };
}

/* назначить/сбросить веб-URL проекта (панель «Просмотр» ловит его позже).
   Пусто — сброс (web_url:null); нелокальный URL отклоняем и на клиенте. */
async function assignWebUrl() {
  const p = projCache.find((x) => x.name === state.selected);
  if (!p) { log('✗ проект не выбран'); return; }
  const vals = await askDialog('веб-URL для «' + p.name + '»', [
    { label: 'http://127.0.0.1:порт/ (пусто — сбросить)', value: p.web_url || '' },
  ]);
  if (!vals) return;
  const raw = (vals[0] || '').trim();
  let web_url = null;
  if (raw) {
    const v = localOnlyUrl(raw);
    if (!v.ok) { log('✗ URL не локальный: ' + v.reason); return; }
    web_url = v.url;
  }
  log((web_url ? 'назначаю URL' : 'сбрасываю URL') + ': ' + p.name + '…');
  try {
    const r = await api('/api/projects/weburl', jsonOpts('POST', { name: p.name, web_url }));
    if (r && r.ok === false) { log('✗ не сохранено: ' + (r.detail || 'причина не указана')); return; }
    p.web_url = web_url;
    renderProjects(projCache);
    log(web_url ? '✓ URL назначен: ' + web_url : '✓ URL сброшен');
  } catch (e) {
    log('✗ не сохранено: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/projects/weburl' : e.message));
  }
}

function archRow(p) {
  const tr = el('tr');
  tr.dataset.name = p.name;
  if (p.name === state.selected) tr.classList.add('sel');
  const name = el('td', null, (p.name === state.selected ? '<span class="sel-mark">›</span> ' : '') + favMark(p) + '<b>' + esc(p.name) + '</b>');
  const act = el('td', 'num dim', fmtAge(p.last_activity));
  const path = el('td', 'path-cell dim', esc(p.path || '—'));
  path.title = p.path || '';
  const btns = el('td');
  const back = el('button', 'mini', '⬆');
  back.type = 'button';
  back.title = 'вернуть в основной список (archived: false)';
  back.addEventListener('click', (ev) => { ev.stopPropagation(); setArchived(p, false); });
  btns.appendChild(back);
  tr.append(name, act, path, btns);
  tr.tabIndex = 0;
  tr.addEventListener('click', () => selectProject(p.name));
  return tr;
}

/* =====================================================================
   грид «проектов» на Tabulator 6.6.1 (static/vendor): сортировка, ширины
   колонок и группировка — библиотечные; наш — Win95-вид и форматиры.
   Выбор строки живёт в atlas.selected, ✓/✗ фокуса — в fbMap.
   =================================================================== */
let projTable = null;   /* экземпляр Tabulator на панели проектов */
let projBuilt = false;  /* Tabulator строится асинхронно — данные ждём в tableBuilt */
let archCache = [];     /* выброшенные — для перерисовки подвала без грида */
let selSyncing = false; /* true, пока выбор в грид ставим программно */
let ctxHintShown = false; /* «ПКМ по строке — действия» — один раз за сессию */
const PROJ_GROUP_KEY = 'atlas.projects.group';
/* варианты группировки — те же значения, что понимает projGroupMode() */
const PROJ_GROUP_OPTS = [
  { value: 'none', label: 'группировка: нет' },
  { value: 'dir', label: 'группировка: каталог' },
  { value: 'src', label: 'группировка: источники' },
];
const PROJ_PERSIST_ID = 'atlas-ptbl2'; /* v2: пересмотр ширин колонок — старые сохранённые ширины не должны побеждать новые дефолты */
function projReady() { return !!(projTable && projBuilt); }

function projGroupMode() {
  let v = null;
  try { v = localStorage.getItem(PROJ_GROUP_KEY); } catch (_) {}
  return ['none', 'dir', 'src'].indexOf(v) !== -1 ? v : 'none';
}

/* «каталог»: первый сегмент после …/Projects/; корневые и внешние — «—» */
function projDirGroup(d) {
  const m = String(d.path || '').match(/\/Projects\/([^/]+)\//);
  return m ? m[1] : '—';
}

/* «источники»: компактная комбинация присутствующих значков (H·G·Z) */
function projSrcGroup(d) {
  const s = d.sources || {};
  return SRC_BADGES.filter(([k]) => !!s[k]).map(([, l]) => l).join('·') || '—';
}

/* значок источника — интерактивный: ЛЕВЫЙ клик и ПКМ открывают меню источника
   (значки крошечные — левый клик обязана понимать). stopPropagation: клик по
   значку не выбирает строку; выбор строки остаётся на остальной ячейке. */
function srcCell(cell) {
  const d = cell.getData();
  const wrap = el('div');
  for (const [key, letter] of SRC_BADGES) {
    const on = !!(d.sources && d.sources[key]);
    const b = el('span', 'badge sq ' + (on ? 'on' : 'off'), letter);
    b.title = key + (on ? '' : ': нет данных');
    const open = (x, y) => openSourceMenu(key, on, d, x, y);
    b.addEventListener('click', (e) => {
      e.stopPropagation();
      const r = b.getBoundingClientRect();
      open(r.left, r.bottom + 2);
    });
    b.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      e.stopPropagation();
      open(e.clientX, e.clientY);
    });
    wrap.appendChild(b);
  }
  return wrap;
}
/* меню источника: есть — открыть/фокус по роду (herdr → workspace, git →
   терминал проекта, vault → поиск по заметкам проекта); нет — «создать запись»
   (POST /api/projects/{name}/sources/{kind}/create); omp/zen — отключённый
   пункт «появится позже». */
function openSourceMenu(kind, on, d, x, y) {
  const name = d.name;
  if (on) {
    const items = [];
    if (kind === 'herdr') items.push({ label: 'фокус herdr workspace', hint: 'H', action: () => focusProject(name, 'herdr_workspace', 'herdr workspace') });
    else if (kind === 'git') items.push({ label: 'терминал проекта', hint: 'W', action: () => focusProject(name, 'terminal', 'терминал atlas') });
    else if (kind === 'vault') items.push({ label: 'поиск по заметкам vault', hint: 'V', action: () => searchProjectVault(name) });
    else items.push({ label: 'открыть', disabled: true, hint: 'появится позже' });
    mkMenu(items, x, y);
    return;
  }
  if (kind === 'omp' || kind === 'zen') {
    mkMenu([{ label: 'создать запись', disabled: true, hint: 'появится позже' }], x, y);
    return;
  }
  mkMenu([{
    label: 'создать запись',
    action: () => {
      log('создаю запись «' + kind + '» для ' + name + '…');
      api('/api/projects/' + encodeURIComponent(name) + '/sources/' + kind + '/create', jsonOpts('POST', {}))
        .then((r) => {
          if (r && r.ok === false) { log('✗ не удалось: ' + (r.detail || 'причина не указана')); return; }
          log('✓ запись создана: ' + kind + ' · ' + name);
          loadProjects();
        })
        .catch((e) => {
          const c = String(e.message).indexOf('422') !== -1;
          log('✗ ' + (c ? 'источник «' + kind + '» появится позже' : e.message));
        });
    },
  }], x, y);
}
function nameCell(cell) {
  const d = cell.getData();
  /* маркер выбора «›» лежит в ТОМ ЖЕ блоке, что и имя (место под него занято
     всегда, выбор лишь переключает visibility): псевдоэлемент в ячейке с
     блочным <div> внутри создавал бы анонимную строку ВЫШЕ него — див уезжал
     на вторую строку и обрезался (ячейка 25px, у вендора overflow:hidden),
     из-за чего выбранная строка выглядела пустой */
  return el('div', null, '<span class="sel-mark">›</span> ' + favMark(d) + '<b>' + esc(d.name) + '</b>');
}
function depsCell(cell) {
  return el('div', null, (cell.getData().deps || []).map((d) => {
    const strict = d.kind === 'strict';
    const tip = (strict ? 'строгая' : 'мягкая') + (d.note ? ': ' + esc(d.note) : '');
    return '<span class="chip dep-' + (strict ? 'strict' : 'soft') + '" title="' + tip + '">' +
      (strict ? '▲ ' : '◇ ') + esc(d.to_project) + '</span>';
  }).join(''));
}
function agentsCell(cell) {
  return el('div', null, agentIcon(cell.getData()));
}
function agentsSorter(a, b, aRow, bRow) {
  const da = aRow.getData(), db = bRow.getData();
  return (da.agents_count || 0) - (db.agents_count || 0) ||
    String(da.agent_status || '').localeCompare(String(db.agent_status || ''));
}
function actCell(cell) {
  return el('div', 'num dim', fmtAge(cell.getData().last_activity));
}
/* путь: без ширины — впитывает остаток (колонка «путь»), длинный режется
   многоточием (CSS .cell-path); клик копирует путь (copyText: Clipboard API +
   execCommand-фолбэк, отзыв — в журнал), не задевая выбор строки и её меню */
function pathCell(cell) {
  const d = cell.getData();
  const n = el('div', 'path-cell dim' + (d.path ? ' copyable' : ''), esc(d.path || '—'));
  n.title = d.path ? d.path + ' — клик: скопировать путь' : '';
  if (d.path) {
    n.addEventListener('click', (e) => { e.stopPropagation(); copyText(d.path, 'путь ' + d.name); });
    /* даблклик по пути — не «фокус herdr» строки */
    n.addEventListener('dblclick', (e) => e.stopPropagation());
  }
  return n;
}
/* кнопки H T W O + ⤓ и краткий ✓/✗: Tabulator очищает ячейку и вставляет
   только возвращённый узел, поэтому ✓/✗ (прямой ребёнок ячейки, после
   кнопок) довешивается в onRendered — после вставки содержимого */
function focusCell(cell, params, onRendered) {
  const d = cell.getData();
  const wrap = el('div', 'rowbtns');
  /* ★/☆ — первой: избранное важнее целей фокуса; нет ключа favorite — нет кнопки */
  if (favKnown(d)) {
    const on = d.favorite === true;
    const fav = el('button', 'mini fav' + (on ? ' on' : ''), on ? '★' : '☆');
    fav.type = 'button';
    fav.title = on ? 'убрать из избранного' : 'в избранное';
    fav.addEventListener('click', (ev) => { ev.stopPropagation(); setFavorite(d, !on); });
    wrap.appendChild(fav);
  }
  for (const t of focusTargets) {
    const b = el('button', 'mini', esc(t.letter || '?'));
    b.type = 'button';
    b.title = (t.label || t.target) + ' — фокус проекта; план может быть: ' +
      'focus (переключить существующее), create (создать), reuse (переиспользовать готовое)';
    b.addEventListener('click', (ev) => { ev.stopPropagation(); focusProject(d.name, t.target, t.label || t.target); });
    wrap.appendChild(b);
  }
  const arch = el('button', 'mini', '⤓');
  arch.type = 'button';
  arch.title = 'выбросить вниз списка — ничего не удаляется';
  arch.addEventListener('click', (ev) => { ev.stopPropagation(); setArchived(d, true); });
  wrap.appendChild(arch);
  /* у проекта задан локальный веб-URL — мини-кнопка «открыть в Просмотре» */
  if (d.web_url) {
    const web = el('button', 'mini', '🌐');
    web.type = 'button';
    web.title = 'открыть в «Просмотр»: ' + d.web_url;
    web.addEventListener('click', (ev) => { ev.stopPropagation(); openInView(d.web_url); });
    wrap.appendChild(web);
  }
  const fb = fbMap.get(d.name);
  if (fb) {
    onRendered(() => {
      const c = cell.getElement();
      const old = c.querySelector('.fb');
      if (old) old.remove();
      const s = el('span', fb.ok ? 'fb fok' : 'fb ferr', fb.ok ? '✓' : '✗');
      s.title = fb.detail;
      c.appendChild(s);
    });
  }
  return wrap;
}

/* ---------- контекстное меню строки проекта (ПКМ) ----------
   Все действия уже существовали кнопками — меню их просто выставляет.
   Архивная строка (подвал) получает урезанное меню: путь + «Вернуть». */
function openProjectRowMenu(d, x, y) {
  mkMenu([
    { label: 'Фокус herdr workspace', hint: 'H', action: () => focusProject(d.name, 'herdr_workspace', 'herdr workspace') },
    { label: 'Фокус herdr вкладка', hint: 'T', action: () => focusProject(d.name, 'herdr_tab', 'herdr tab') },
    { label: 'Терминал проекта', hint: 'W', action: () => focusProject(d.name, 'terminal', 'терминал atlas') },
    { sep: true },
    { label: 'Задачи make', action: () => { selectProject(d.name); openMakeDrawer(); } },
    { label: 'Скопировать путь', disabled: !d.path, action: () => copyText(d.path, 'путь ' + d.name) },
    favMenuItem(d),
    { sep: true },
    d.archived
      ? { label: 'Вернуть', action: () => setArchived(d, false) }
      : { label: 'Выбросить вниз', danger: true, action: () => setArchived(d, true) },
  ], x, y);
}
function openMakeDrawer() {
  const det = panelRoots.projects && panelRoots.projects.querySelector('#mtasks');
  if (!det) return;
  det.open = true;
  loadMakeTasks();
  try { det.scrollIntoView({ block: 'nearest' }); } catch (_) {}
}

function projColumnDefs() {
  return [
    { title: 'источники', field: 'sources', width: 92, headerSort: false, formatter: srcCell, cssClass: 'cell-sources' }, /* 5 значков 14px + 4 зазора = 78 */
    { title: 'проект', field: 'name', sorter: 'string', width: 190, formatter: nameCell, cssClass: 'cell-name' },
    { title: 'зависимости', field: 'deps', headerSort: false, width: 150, formatter: depsCell }, /* лишнее режет ячейка, целиком — в title чипа */
    { title: 'агенты', field: 'agents_count', sorter: agentsSorter, width: 104, formatter: agentsCell }, /* «◆ working (12)» + паддинги */
    { title: 'сессии', field: 'sessions_count', sorter: 'number', width: 56, hozAlign: 'right', formatter: (c) => el('div', 'num', c.getData().sessions_count ? String(c.getData().sessions_count) : '—') }, /* число — только шапка задаёт минимум */
    { title: 'активность', field: 'last_activity', sorter: 'number', width: 88, hozAlign: 'right', formatter: actCell, cssClass: 'cell-act' }, /* «5 мин» + шапка */
    { title: 'путь', field: 'path', headerSort: false, formatter: pathCell, cssClass: 'cell-path' }, /* без ширины: впитывает остаток, как раньше */
    { title: 'фокус', field: 'focus', headerSort: false, width: 148, formatter: focusCell, cssClass: 'cell-focus' },
  ];
}

function buildProjTable(mount) {
  projTable = new Tabulator(mount, {
    index: 'name',
    columns: projColumnDefs(),
    layout: 'fitColumns',
    selectableRows: 1, /* в 6.x опция переименована из selectable */
    movableColumns: false,
    groupToggleElement: 'header', /* клик по шапке группы сворачивает её */
    persistence: { sort: false, filter: false, group: false, columns: true },
    persistenceID: PROJ_PERSIST_ID,
    persistenceMode: 'local',
    /* сортировка: 6.6 не рисует стрелки сама (только aria-sort) — рисуем тут */
    headerSortElement: (component, dir) => {
      const s = el('span', 'sortmark');
      s.textContent = dir === 'asc' ? '▲' : dir === 'desc' ? '▼' : '↕';
      s.title = 'сортировка';
      return s;
    },
    groupHeader: (value, count) => value + ' · ' + count,
    /* data-name на строках грида — тем же атрибутом, что и у строк подвала:
       якорение прокрутки (архив/возврат) ищет верхнюю видимую строку по нему */
    rowFormatter: (row) => { row.getElement().dataset.name = row.getData().name; },
  });
  /* в 6.x колбэки-опции заменены событиями */
  projTable.on('rowSelected', (row) => {
    const d = row.getData();
    state.selected = d.name;
    try { localStorage.setItem('atlas.selected', d.name); } catch (_) {}
    if (!selSyncing) log('выбран проект: ' + d.name);
    renderArchived(archCache); /* ›-маркер выбранных и в подвале */
    updateArchBtn();
  });
  /* клик по уже выбранной строке снимает выбор (selectableRows toggle) —
     без этого state.selected остаётся, а в грид выделения нет */
  projTable.on('rowDeselected', (row) => {
    if (selSyncing) return; /* программная рассинхронизация в syncProjSelection */
    const d = row.getData();
    if (state.selected !== d.name) return;
    state.selected = null;
    try { localStorage.removeItem('atlas.selected'); } catch (_) {}
    log('выбор сброшен: ' + d.name);
    renderArchived(archCache);
    updateArchBtn();
  });
  projTable.on('rowDblClick', (e, row) => focusProject(row.getData().name, 'herdr_workspace', 'herdr workspace'));
  /* клавиатура: строки Tabulator сами не фокусируются (tabIndex=-1) — 0 здесь */
  projTable.on('renderComplete', () => {
    projTable.getRows().forEach((r) => {
      const n = r.getElement();
      n.tabIndex = 0;
      n.__trow = r;
    });
  });
  /* Tabulator строится асинхронно: данные и группировка — после tableBuilt */
  projTable.on('tableBuilt', () => {
    projBuilt = true;
    try {
      projTable.setData(projCache.filter((p) => !p.archived)).then(() => syncProjSelection()).catch(() => {});
    } catch (_) {}
    applyProjGroup();
    applyProjFilter();
  });
}

function applyProjGroup() {
  if (!projReady()) return;
  const mode = projGroupMode();
  try {
    if (mode === 'dir') projTable.setGroupBy(projDirGroup);
    else if (mode === 'src') projTable.setGroupBy(projSrcGroup);
    else projTable.setGroupBy(false);
  } catch (_) {}
}

/* фильтр-бокс #filter: по имени и пути (подвал фильтруется тем же правилом) */
function projFilterValue() {
  const inp = panelRoots.projects && panelRoots.projects.querySelector('#filter');
  return ((inp && inp.value) || '').trim().toLowerCase();
}
function applyProjFilter() {
  if (!projReady()) return;
  const f = projFilterValue();
  try {
    /* параметры — только объект: строка-параметр Tabulator считает
       фильтром «поле-тип-значение» и прячет все строки (type: '') */
    projTable.setFilter((data, p) => !p || !p.q ||
      String(data.name).toLowerCase().includes(p.q) ||
      String(data.path || '').toLowerCase().includes(p.q), { q: f });
  } catch (_) {}
}

/* сброс ширин/раскладки из панели настроек */
function resetProjCols() {
  try { localStorage.removeItem('tabulator-' + PROJ_PERSIST_ID + '-columns'); } catch (_) {}
  if (projReady()) { try { projTable.setColumns(projColumnDefs()); } catch (_) {} }
  log('✓ ширины колонок сброшены');
}


function updateProjectsChrome() {
  if (!ch.p) return;
  if (ch.p.count) ch.p.count.textContent = 'проектов: ' + statsProj.shown + ' / ' + statsProj.total;
  if (ch.p.arch) ch.p.arch.textContent = '· выброшено: ' + statsProj.arch;
  if (ch.p.filt) {
    ch.p.filt.textContent = '· фильтр: ' + (statsProj.shown + statsProj.archShown) + ' совп.';
    ch.p.filt.classList.toggle('hidden', !statsProj.filt);
  }
}

function renderProjects(list) {
  const active = [], archived = [];
  for (const p of list) (p.archived ? archived : active).push(p);
  archCache = archived;
  /* активные — в грид; выброшенные — в рукодельный подвал (не в грид).
     До tableBuilt грид пуст — tableBuilt сам возьмёт projCache.
     Промис оседания данных возвращаем: якорение прокрутки ждёт его. */
  let settled = Promise.resolve();
  if (projReady()) {
    try {
      settled = projTable.setData(active)
        .then(() => { syncProjSelection(); applyProjFilter(); })
        .catch(() => {});
    } catch (_) {}
  }
  renderArchived(archived);
  const flt = projFilterValue();
  const matches = (p) => !flt || p.name.toLowerCase().includes(flt) || (p.path || '').toLowerCase().includes(flt);
  const shown = active.filter(matches).length;
  const archShown = archived.filter(matches).length;
  statsProj = { shown, total: active.length, arch: archived.length, archShown, filt: !!flt };
  updateProjectsChrome();
  updateArchBtn();
  return settled;
}

function renderArchived(archived) {
  const det = $('#arch'), atb = $('#archtbl tbody'), acnt = $('#arch-cnt');
  if (!det || !atb) return;
  const flt = projFilterValue();
  const matches = (p) => !flt || p.name.toLowerCase().includes(flt) || (p.path || '').toLowerCase().includes(flt);
  atb.innerHTML = '';
  for (const p of archived) if (matches(p)) atb.appendChild(archRow(p));
  det.classList.toggle('hidden', !archived.length);
  if (acnt) acnt.textContent = String(archived.length);
}

async function loadProjects() {
  try {
    const d = await api('/api/projects');
    projCache = Array.isArray(d.projects) ? d.projects : [];
    /* выбранный проект исчез из списка целиком — сбрасываем выбор */
    if (state.selected && !projCache.some((p) => p.name === state.selected)) {
      state.selected = null;
      try { localStorage.removeItem('atlas.selected'); } catch (_) {}
      log('выбранный проект исчез из списка — выбор сброшен');
    }
    renderProjects(projCache);
    hideErr('projects');
    probeSet(true);
  } catch (e) {
    panelError('проекты', e, 'projects');
    scheduleRetry(loadProjects, 'projects');
  }
}

function fillChromeProjects() {
  ch.p = {};
  setChrome((mk) => {
    ch.p.count = mk('span', 'dim');
    ch.p.arch = mk('span', 'dim');
    ch.p.filt = mk('span', 'dim hidden');
    const b = mk('button', 'btn95', '⟳ Обновить');
    b.type = 'button';
    b.title = 'перечитать список проектов';
    b.addEventListener('click', () => { log('обновление…'); loadProjects(); });
  });
  updateProjectsChrome();
}

function buildProjects(root) {
  panelRoots.projects = root;
  const q = (s) => root.querySelector(s);
  /* рукодельные ширины ушли в persistence Tabulator — старый хлам долой */
  try {
    for (const k of Object.keys(localStorage)) {
      if (k.indexOf('atlas.colw.') === 0) localStorage.removeItem(k);
    }
  } catch (_) {}
  root.innerHTML =
    '<div class="panel">' +
      '<div class="toolbar">' +
        '<input id="filter" class="fld" style="width:180px" placeholder="фильтр: (/)" autocomplete="off" spellcheck="false">' +
        '<span id="groupsel-wrap"></span>' +
        '<button id="btn-arch" class="btn95" type="button" disabled title="в подвал, ничего не удаляется">⤓ выбросить вниз</button>' +
        '<button id="btn-refresh" class="btn95 tb-fallback" type="button" title="перечитать список">⟳ Обновить</button>' +
        '<button id="btn-sync" class="btn95" type="button" title="синхронизировать все источники">Синхронизировать источники</button>' +
        '<button id="btn-weburl" class="btn95" type="button" title="назначить локальный веб-URL выбранному проекту (http://127.0.0.1|localhost:порт; пусто — сбросить)">⊕ URL</button>' +
        '<span class="spacer"></span>' +
        winControlsHtml() +
        '<button id="btn-orch" class="btn95" type="button" title="открыть панель «Оркестратор» (rpc-канал, не терминал)">Открыть оркестратор</button>' +
      '</div>' +
      '<div class="scroll well grow">' +
        '<div id="ptbl"></div>' +
        '<details id="arch" class="arch hidden"><summary>подвал — выброшенные (<span id="arch-cnt">0</span>)</summary>' +
          '<table class="tbl" id="archtbl"><thead><tr>' +
            '<th>проект</th><th>активность</th><th>путь</th><th></th>' +
          '</tr></thead><tbody></tbody></table>' +
        '</details>' +
        '<details id="mtasks" class="tasks"><summary id="mtasks-sum">задачи make</summary>' +
          '<div class="tasks-body" id="mtasks-body"><div class="dim">проект не выбран</div></div>' +
        '</details>' +
      '</div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  q('#groupsel-wrap').appendChild(mkSelect({
    id: 'groupsel',
    title: 'группировка строк: каталог — первый сегмент пути после Projects/; источники — присутствующие значки',
    options: PROJ_GROUP_OPTS,
    value: projGroupMode(),
    onchange: (v, opt) => {
      try { localStorage.setItem(PROJ_GROUP_KEY, v); } catch (_) {}
      applyProjGroup();
      log('группировка: ' + String((opt && opt.label) || v).replace('группировка: ', ''));
    },
  }));
  q('#btn-arch').addEventListener('click', () => {
    const p = projCache.find((x) => x.name === state.selected);
    if (p) setArchived(p, !p.archived);
  });
  q('#btn-refresh').addEventListener('click', () => { log('обновление…'); loadProjects(); });
  q('#btn-sync').addEventListener('click', async () => {
    log('синхронизация источников…');
    try { const r = await api('/api/sync', jsonOpts('POST', {})); log('синхронизация готова за ' + (r.duration != null ? r.duration + 'с' : '?')); }
    catch (e) { log('✗ синхронизация не удалась: ' + e.message); probeSet(false); }
  });
  q('#btn-weburl').addEventListener('click', assignWebUrl);
  q('#btn-orch').addEventListener('click', () => openOrchestratorPanel());
  q('#filter').addEventListener('input', applyProjFilter);
  q('#filter').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      const first = q('#ptbl .tabulator-row');
      if (first) { e.preventDefault(); first.focus(); }
    }
  });
  /* клавиатура: Enter на сфокусированной строке выбирает проект */
  q('#ptbl').addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    const rowEl = e.target && e.target.closest ? e.target.closest('.tabulator-row') : null;
    if (rowEl && rowEl.__trow) { e.preventDefault(); try { projTable.selectRow(rowEl.__trow); } catch (_) {} }
  });
  /* ПКМ по строке грида: меню действий; нативное меню подавлено на всём гриде.
     Левый клик по строке работает как раньше (выбор). */
  q('#ptbl').addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const rowEl = e.target && e.target.closest ? e.target.closest('.tabulator-row') : null;
    if (!rowEl || !rowEl.__trow) return;
    openProjectRowMenu(rowEl.__trow.getData(), e.clientX, e.clientY);
  });
  /* ПКМ в подвале (выброшенные): урезанное меню — путь, избранное и «Вернуть» */
  q('#archtbl').addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const tr = e.target && e.target.closest ? e.target.closest('tr') : null;
    if (!tr || !tr.dataset.name) return;
    const p = archCache.find((x) => x.name === tr.dataset.name);
    if (!p) return;
    mkMenu([
      { label: 'Скопировать путь', disabled: !p.path, action: () => copyText(p.path, 'путь ' + p.name) },
      favMenuItem(p),
      { sep: true },
      { label: 'Вернуть', action: () => setArchived(p, false) },
    ], e.clientX, e.clientY);
  });
  /* открытие/закрытие подвала не двигает прокрутку: якорь снимаем на клик по
     summary (клавиатурные Enter/Space тоже дают click) — до смены open, а
     возвращаем в toggle — когда open уже сменился и браузер мог зажать scrollTop */
  const archDet = q('#arch');
  archDet.addEventListener('click', (e) => {
    if (!(e.target && e.target.closest && e.target.closest('summary'))) return;
    const sc = projScroller();
    archDet.__snap = sc ? projSnapScroll(sc) : null;
  });
  archDet.addEventListener('toggle', () => {
    const snap = archDet.__snap;
    archDet.__snap = null;
    if (snap) projApplySnap(projScroller(), snap);
  });
  if (!ctxHintShown) { ctxHintShown = true; log('ПКМ по строке — действия'); }
  bindWinControls(root);
  buildProjTable(q('#ptbl'));
  loadProjects();
  /* цели фокуса — с бэкенда; недоступны → четыре кнопки по умолчанию */
  loadFocusTargets().then(() => renderProjects(projCache));
  renderMakeDrawer();
  loadMakeTasks();
  chromeFillers.projects = fillChromeProjects;
  fillChromeProjects();
}

/* =====================================================================
   хост-окно «проекты»: вкладки свёрнутых панелей над списком
   =================================================================== */
function buildHost(root) {
  root.innerHTML =
    '<div id="tabbar" class="tabbar hidden"></div>' +
    '<div id="host-projects"></div>' +
    '<div id="host-tab" class="hidden"></div>';
  buildProjects($('#host-projects'));
}
/* =====================================================================
   «задачи make» — сворачиваемый ящик под гридом проектов (выбранный проект).
   GET /api/project/{name}/tasks, POST .../tasks/run, GET .../tasks/run, POST .../tasks/kill
   =================================================================== */
let makeTimer = null;   /* опрос запуска, пока активен (+2 с после выхода) */
let makeExitAt = 0;     /* deadline добора вывода после завершения */
let makeProj = null;    /* проект, для которого загружены цели */
let makeTasks = null;   /* {makefile, tasks, error?} */
let makeRun = null;     /* последний ответ GET .../tasks/run */
let makeTarget = null;  /* выбранная цель */

function makeUrl(name, sub) { return '/api/project/' + encodeURIComponent(name) + '/tasks' + (sub || ''); }
function stopMakePoll() { if (makeTimer) { clearInterval(makeTimer); makeTimer = null; } }
function startMakePoll() {
  stopMakePoll();
  makeTimer = setInterval(makePoll, 1000);
}
async function makePoll() {
  if (!makeProj) { stopMakePoll(); return; }
  try {
    const r = await api(makeUrl(makeProj, '/run'));
    makeRun = r && typeof r === 'object' ? r : null;
    const active = !!(makeRun && makeRun.active);
    if (active) makeExitAt = 0;
    else if (!makeExitAt) makeExitAt = Date.now() + 2000; /* добираем финальный вывод */
    renderMakeDrawer();
    if (!active && Date.now() >= makeExitAt) { stopMakePoll(); makeExitAt = 0; }
  } catch (e) {
    stopMakePoll();
    if (String(e.message).indexOf('404') === -1) log('✗ задачи make: ' + e.message);
  }
}
async function loadMakeTasks() {
  const name = state.selected;
  if (!name) {
    stopMakePoll(); makeProj = null; makeTasks = null; makeRun = null; makeTarget = null; makeExitAt = 0;
    renderMakeDrawer();
    return;
  }
  if (name === makeProj && makeTasks) { renderMakeDrawer(); return; }
  stopMakePoll();
  makeProj = name; makeTasks = null; makeRun = null; makeTarget = null; makeExitAt = 0;
  renderMakeDrawer();
  try {
    const d = await api(makeUrl(name, ''));
    if (makeProj !== name) return;
    makeTasks = d && typeof d === 'object' ? d : { makefile: null, tasks: [] };
    const tasks = Array.isArray(makeTasks.tasks) ? makeTasks.tasks : [];
    if (tasks.length && !makeTarget) makeTarget = tasks[0].name;
    renderMakeDrawer();
    /* уже идущий процесс (панель открыли посреди сборки) */
    try {
      const r = await api(makeUrl(name, '/run'));
      if (makeProj !== name) return;
      makeRun = r && typeof r === 'object' ? r : null;
      if (makeRun && makeRun.active) { makeExitAt = 0; startMakePoll(); }
      renderMakeDrawer();
    } catch (_) { /* нет состояния — не беда */ }
  } catch (e) {
    if (makeProj !== name) return;
    makeTasks = { makefile: null, tasks: [], error: String(e.message).indexOf('404') !== -1
      ? 'бэкенд без /api/project/*/tasks' : ('нет связи: ' + e.message) };
    renderMakeDrawer();
  }
}
async function makeRunStart() {
  if (!makeProj || !makeTarget) { log('✗ цель make не выбрана'); return; }
  log('make: запуск «' + makeTarget + '» в ' + makeProj + '…');
  try {
    const r = await api(makeUrl(makeProj, '/run'), jsonOpts('POST', { target: makeTarget }));
    if (r && r.ok === false) { log('✗ запуск: ' + (r.detail || 'причина не указана')); return; }
    makeRun = null; makeExitAt = 0;
    renderMakeDrawer();
    startMakePoll();
  } catch (e) {
    const busy = String(e.message).indexOf('409') !== -1;
    log(busy ? '✗ задача уже выполняется' : ('✗ запуск не удался: ' + e.message));
    if (busy) { makeExitAt = 0; startMakePoll(); renderMakeDrawer(); }
  }
}
async function makeKill() {
  log('make: остановка…');
  try { await api(makeUrl(makeProj, '/kill'), jsonOpts('POST', {})); }
  catch (e) { log('✗ остановка: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без tasks/kill' : e.message)); }
  makeExitAt = 0;
  startMakePoll();
  renderMakeDrawer();
}
function renderMakeDrawer() {
  const root = panelRoots.projects;
  const det = root && root.querySelector('#mtasks');
  if (!det) return;
  const sum = det.querySelector('#mtasks-sum');
  const body = det.querySelector('#mtasks-body');
  if (!body) return;
  const name = state.selected;
  if (sum) sum.innerHTML = 'задачи make' + (name ? ' — <b>' + esc(name) + '</b>' : '');
  if (!name) { body.innerHTML = '<div class="dim">проект не выбран</div>'; return; }
  if (makeProj !== name || !makeTasks) { body.innerHTML = '<div class="dim">загрузка…</div>'; return; }
  if (makeTasks.error) { body.innerHTML = '<div class="dim">' + esc(makeTasks.error) + '</div>'; return; }
  const tasks = Array.isArray(makeTasks.tasks) ? makeTasks.tasks : [];
  if (!makeTasks.makefile) { body.innerHTML = '<div class="dim">нет Makefile</div>'; return; }
  if (!tasks.length) { body.innerHTML = '<div class="dim">в Makefile нет целей</div>'; return; }
  const active = !!(makeRun && makeRun.active);
  body.innerHTML = '';
  const list = el('div', 'mtasks-list');
  for (const t of tasks) {
    const b = el('button', 'mini mtask' + (t.name === makeTarget ? ' sel' : '') + (active ? ' gate-dim' : ''), esc(t.name));
    b.type = 'button';
    b.title = t.doc || t.name;
    if (!active) b.addEventListener('click', () => { makeTarget = t.name; renderMakeDrawer(); });
    list.appendChild(b);
  }
  body.appendChild(list);
  const bar = el('div', 'tasks-bar');
  if (active) {
    bar.appendChild(el('span', 'spin', '⏳ выполняется…'));
    const stop = el('button', 'btn95', '■ остановить');
    stop.type = 'button';
    stop.title = 'послать kill текущей задаче';
    stop.addEventListener('click', makeKill);
    bar.appendChild(stop);
  } else {
    const go = el('button', 'btn95', '▶ запустить');
    go.type = 'button';
    go.title = 'запустить цель «' + (makeTarget || '?') + '»';
    go.addEventListener('click', makeRunStart);
    bar.appendChild(go);
  }
  body.appendChild(bar);
  const runLines = makeRun && (Array.isArray(makeRun.lines) ? makeRun.lines
    : (typeof makeRun.lines === 'string' ? makeRun.lines.split('\n') : []));
  if (makeRun && (active || (runLines && runLines.length) || makeRun.exit != null)) {
    const out = el('div', 'tasks-out');
    out.textContent = runLines && runLines.length ? runLines.join('\n') + '\n' : '';
    if (!active && makeRun.exit != null) {
      const exit = el('span', 'exit ' + (Number(makeRun.exit) === 0 ? 'ok' : 'err'),
        'exit ' + makeRun.exit + ' · ' + fmtSecs(makeRun.duration_s));
      out.appendChild(exit);
    }
    body.appendChild(out);
    out.scrollTop = out.scrollHeight; /* автопрокрутка к свежему выводу */
  }
}

export { buildHost, buildProjects, loadProjects, projCache, renderProjects, resetProjCols, selectProject };
