/* static/js/search.js — панель «Поиск» (fts/вектор) + мост atlas.search.request.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc, log } from './lib/dom.js';
import { BUILDERS, WIN, activateTab, bindWinControls, ch, chromeFillers, fillChrome, hideErr, panelError, panelRoots, pollUi, probeSet, scheduleRetry, setChrome, setUiCache, settings, state, uiCache, winControlsHtml } from './core.js';
import { api, jsonOpts } from './lib/api.js';
import { openOrchestratorPanel } from './orchestrator.js';
import { selectProject } from './projects.js';

let searchHits = [];
let searchQuery = '';
let searchMeta = null; /* {vector_enabled, model, took_ms, vector_error} */
/* =====================================================================
   панель «поиск»
   =================================================================== */
const KIND_LABELS = { memory_fact: 'память', vault_note: 'vault', omp_session: 'сессия', board_item: 'дело', project: 'проект', zen_tab: 'вкладка' };
const KIND_YELLOW = new Set(['memory_fact', 'board_item']); /* текстовые (fts-подобные) — жёлтый */
const KIND_CYAN = new Set(['vault_note', 'omp_session', 'project', 'zen_tab']); /* векторные — голубой */

function kindBadge(kind) {
  const label = KIND_LABELS[kind] || kind;
  const cls = KIND_YELLOW.has(kind) ? 'k-yellow' : KIND_CYAN.has(kind) ? 'k-cyan' : '';
  return '<span class="badge k-kind ' + cls + '" title="' + esc(kind) + '">' + esc(label) + '</span>';
}

/* сниппет бэкенд подсвечивает сам (<b>…</b>); если нет — подсвечиваем здесь */
function snippetHtml(snippet, q) {
  const raw = String(snippet == null ? '' : snippet);
  if (raw.includes('<b>')) return raw;
  const e = esc(raw);
  if (!q) return e;
  try {
    return e.replace(new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi'), (m) => '<b>' + m + '</b>');
  } catch (_) { return e; }
}

let openAvailable = null; /* null — неизвестно; false — бэкенд без /api/open (кнопки убираем) */

/* «↗ файл» в конце строки результата → POST /api/open {ref}. Только для
   попаданий с ненулевым file (у остальных file=null — мёртвый контрол не рисуем).
   Пока /api/open отвечает 404 — кнопки скрываем (деградация, не ошибка). */
async function openHitFile(h, btn) {
  if (!h || !h.source_ref) { log('✗ у попадания нет source_ref'); return; }
  try {
    const r = await api('/api/open', jsonOpts('POST', { ref: h.source_ref }));
    if (r && r.ok === false) { log('✗ открыть не удалось: ' + (r.detail || 'причина не указана')); if (btn) btn.classList.add('ferr'); }
    else log('✓ открыто: ' + (h.file || h.source_ref));
    openAvailable = true;
  } catch (e) {
    if (String(e.message).indexOf('404') !== -1) {
      openAvailable = false;
      log('✗ открытие файлов недоступно: бэкенд без /api/open');
      const box = $('#sres');
      if (box) box.querySelectorAll('.r-file').forEach((b) => b.remove());
    } else {
      log('✗ открыть не удалось: ' + e.message);
      if (btn) btn.classList.add('ferr');
    }
  }
}

function renderSearch(hits, q) {
  const box = $('#sres');
  if (!box) return;
  box.innerHTML = '';
  for (const h of hits) {
    const r = el('div', 'result');
    const head = el('div', 'r-head');
    head.innerHTML = kindBadge(h.kind) + matchBadge(h.match) +
      '<span class="r-title">' + esc(h.title || '(без названия)') + '</span>' +
      '<span class="scores">' + scoreText(h) + '</span>';
    if (h.file && openAvailable !== false) {
      const fb = el('button', 'mini ic r-file', '↗');
      fb.type = 'button';
      fb.title = 'открыть файл: ' + h.file;
      fb.setAttribute('aria-label', 'открыть файл');
      fb.addEventListener('click', () => openHitFile(h, fb));
      head.appendChild(fb);
    }
    r.appendChild(head);
    r.appendChild(el('div', 'snippet', snippetHtml(h.snippet, q)));
    r.appendChild(el('div', 'r-ref dim', esc(h.source_ref || '')));
    box.appendChild(r);
  }
  if (!hits.length) box.appendChild(el('div', 'dim', '— ничего не найдено —'));
}

/* значок способа retrieval: fts жёлтый / вектор голубой / оба зелёный */
function matchBadge(match) {
  if (match === 'fts') return '<span class="badge k-kind k-yellow" title="полнотекстовое совпадение">fts</span>';
  if (match === 'vector') return '<span class="badge k-kind k-cyan" title="векторное совпадение">вектор</span>';
  if (match === 'both') return '<span class="badge k-kind k-green" title="совпадение по тексту и по вектору">оба</span>';
  return '';
}

/* компактные оценки: bm25 для fts, cos для вектора, обе — через точку */
function scoreText(h) {
  const parts = [];
  if (typeof h.bm25 === 'number') parts.push('bm25 ' + h.bm25.toFixed(1));
  if (typeof h.cos === 'number') parts.push('cos ' + h.cos.toFixed(2));
  return parts.join(' · ');
}

/* попадание относится к выбранному проекту? Смотрим явное поле project,
   затем source_ref, затем title (у части источников ref не содержит имени,
   например «board:1», — имя остаётся только в title) */
function hitOfProject(h, sel) {
  const s = sel.toLowerCase();
  if (h.project && String(h.project).toLowerCase() === s) return true;
  if (String(h.source_ref || '').toLowerCase().includes(s)) return true;
  return String(h.title || '').toLowerCase().includes(s);
}

function selonlyState() {
  try { return localStorage.getItem('atlas.search.selonly') === '1'; } catch (_) { return false; }
}
function setSelonly(v) {
  try { localStorage.setItem('atlas.search.selonly', v ? '1' : '0'); } catch (_) {}
  if (ch.se && ch.se.tog) ch.se.tog.checked = v;
  const tb = panelRoots.search && panelRoots.search.querySelector('#selonly-tb');
  if (tb) tb.checked = v;
  applySearchFilter();
}

function applySearchFilter() {
  const only = selonlyState();
  let hits = searchHits;
  if (searchKindFilter) hits = hits.filter((h) => h.kind === searchKindFilter);
  if (only && state.selected) hits = hits.filter((h) => hitOfProject(h, state.selected));
  renderSearch(hits, searchQuery);
  if (ch.se && ch.se.info) {
    const hybrid = (searchQuery && searchMeta && searchMeta.vector_enabled &&
        settings['ui.show_hybrid_details'] !== false && searchMeta.model)
      ? ' · гибрид: ' + searchMeta.model + ' · ' + (searchMeta.took_ms != null ? searchMeta.took_ms + ' мс' : '?') : '';
    const kf = searchKindFilter ? ' · только ' + (KIND_LABELS[searchKindFilter] || searchKindFilter) : '';
    ch.se.info.textContent = !searchQuery ? 'введите запрос…'
      : 'найдено: ' + hits.length + (only && state.selected && hits.length !== searchHits.length ? ' (из ' + searchHits.length + ')' : '') + kf + hybrid;
  }
}

/* ---------- запрос к поиску из меню значка vault (мост atlas.search.request) ----
   выбрать проект → открыть панель «Поиск» (вкладка/окно) → прогнать запрос
   с фильтрами «только этот проект» и «только vault-заметки». Чужое окно
   получает запрос через localStorage-мост, как терминал. */
const SEARCH_REQ = 'atlas.search.request';
let searchKindFilter = null; /* например 'vault_note' — до ручного ввода */
function applySearchRequest(req) {
  searchKindFilter = (req && req.kind) || null;
  setSelonly(!!(req && req.selonly));
  const inp = $('#q');
  if (inp && req && req.q != null) inp.value = String(req.q);
  runSearch();
}
async function openSearchPanel() {
  let ui = null;
  try { ui = await api('/api/ui'); setUiCache(ui); } catch (_) {}
  const tabs = ui && Array.isArray(ui.tabs) ? ui.tabs : (uiCache && Array.isArray(uiCache.tabs) ? uiCache.tabs : []);
  const wins = ui && Array.isArray(ui.windows) ? ui.windows : (uiCache && Array.isArray(uiCache.windows) ? uiCache.windows : []);
  if (WIN === 'projects' && tabs.indexOf('search') !== -1 && BUILDERS.search) {
    activateTab('search');
    return;
  }
  if (wins.indexOf('search') === -1) {
    try { await api('/api/windows/restore', jsonOpts('POST', { win: 'search' })); }
    catch (e) { log('✗ окно «Поиск»: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/restore' : e.message)); }
    pollUi();
  }
}
async function searchProjectVault(name) {
  selectProject(name);
  await openSearchPanel();
  if (panelRoots.search) applySearchRequest({ q: name, selonly: true, kind: 'vault_note' });
  else {
    try { localStorage.setItem(SEARCH_REQ, JSON.stringify({ q: name, selonly: true, kind: 'vault_note' })); } catch (_) {}
  }
}

/* деградация: векторное крыло недоступно — dim-предупреждение, поиск работает */
function updateVecWarn() {
  const w = $('#vecwarn');
  if (!w) return;
  const err = searchMeta && searchMeta.vector_error;
  w.classList.toggle('hidden', !err);
  w.textContent = err ? 'вектор недоступен: ' + err : '';
}

async function runSearch() {
  const inp = $('#q');
  const q = (inp && inp.value || '').trim();
  searchQuery = q;
  if (!q) { searchHits = []; searchMeta = null; updateVecWarn(); applySearchFilter(); return; }
  try {
    const k = Math.min(200, Math.max(1, Number(settings['ui.search_k']) || 20));
    const d = await api('/api/search?q=' + encodeURIComponent(q) + '&k=' + k);
    searchHits = Array.isArray(d.hits) ? d.hits : [];
    searchMeta = d;
    updateVecWarn();
    applySearchFilter();
    hideErr('search');
    probeSet(true);
  } catch (e) {
    panelError('поиск', e, 'search');
    scheduleRetry(runSearch, 'search');
  }
}

function buildSearch(root) {
  panelRoots.search = root;
  const q = (s) => root.querySelector(s);
  root.innerHTML =
    '<div class="panel" style="grid-template-rows:auto auto minmax(0,1fr)">' +
      '<div class="toolbar">' + winControlsHtml() +
        '<span class="spacer"></span>' +
        '<label class="tog tb-fallback" title="показывать только результаты выбранного проекта">' +
          '<input type="checkbox" id="selonly-tb"> только выбранный проект</label>' +
      '</div>' +
      '<div class="search-top">' +
        '<form id="sform" class="sform out">' +
          '<input id="q" class="fld" placeholder="поиск… (Enter — искать, Esc — очистить)" autocomplete="off" spellcheck="false">' +
          '<button class="btn95" type="submit">Найти</button>' +
          '<span class="spacer"></span>' +
          '<button id="btn-orch" class="btn95" type="button" title="открыть панель «Оркестратор» (rpc-канал, не терминал)">Открыть оркестратор</button>' +
        '</form>' +
        '<div id="vecwarn" class="vecwarn hidden"></div>' +
      '</div>' +
      '<div class="scroll well grow" id="sres"></div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  const inp = q('#q');
  const tbtog = q('#selonly-tb');
  tbtog.checked = selonlyState();
  tbtog.addEventListener('change', () => setSelonly(tbtog.checked));
  let deb = null;
  inp.addEventListener('input', () => {
    searchKindFilter = null; /* ручной ввод сбрасывает фильтр рода из моста */
    clearTimeout(deb);
    deb = setTimeout(runSearch, 250);
  });
  q('#sform').addEventListener('submit', (e) => { e.preventDefault(); clearTimeout(deb); searchKindFilter = null; runSearch(); });
  q('#btn-orch').addEventListener('click', () => openOrchestratorPanel());
  /* мост из других окон: «поиск по заметкам vault проекта» */
  const consumeSearchRequest = () => {
    let s = null;
    try { s = localStorage.getItem(SEARCH_REQ); } catch (_) {}
    if (!s) return;
    try { localStorage.removeItem(SEARCH_REQ); } catch (_) {}
    try { applySearchRequest(JSON.parse(s)); } catch (_) {}
  };
  window.addEventListener('storage', (e) => { if (e.key === SEARCH_REQ && panelRoots.search) consumeSearchRequest(); });
  consumeSearchRequest(); /* запрос мог прийти, пока панель ещё не была построена */
  bindWinControls(root);
  inp.focus();
  chromeFillers.search = () => {
    ch.se = {};
    setChrome((mk) => {
      ch.se.info = mk('span', 'dim', 'введите запрос…');
      const lab = mk('label', 'tog');
      lab.title = 'показывать только результаты выбранного проекта';
      const cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.checked = selonlyState();
      cb.addEventListener('change', () => setSelonly(cb.checked));
      lab.appendChild(cb);
      lab.appendChild(document.createTextNode(' только выбранный проект'));
      ch.se.tog = cb;
    });
    applySearchFilter();
  };
  fillChrome();
}

export { applySearchFilter, buildSearch, runSearch, searchProjectVault };
