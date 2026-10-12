/* static/js/view.js — панель «Просмотр»: iframe с allowlist 127.0.0.1.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { api, jsonOpts, localOnlyUrl } from './lib/api.js';
import { bindWinControls, ch, chromeFillers, fillChrome, panelRoots, pollUi, setChrome, uiCache, winControlsHtml } from './core.js';
import { log } from './lib/dom.js';

/* =====================================================================
   панель «Просмотр» (?win=view): iframe локального веб-UI + адресная строка.
   Адрес приходит и от строки, и от моста atlas.view.request (кнопка 🌐 в проектах).
   =================================================================== */
const VIEW_LAST = 'atlas.view.last';
const VIEW_REQ = 'atlas.view.request';
let viewFrame = null; /* <iframe> */
let viewInput = null; /* <input> адреса */
let viewEmptyEl = null; /* подсказка поверх пустого iframe (about:blank бел) */

function viewIsOpen() {
  if (!uiCache) return null;
  const w = Array.isArray(uiCache.windows) ? uiCache.windows : [];
  const t = Array.isArray(uiCache.tabs) ? uiCache.tabs : [];
  return w.indexOf('view') !== -1 || t.indexOf('view') !== -1;
}
function viewNavigate(url) {
  if (!viewFrame) return;
  viewFrame.src = url;
  if (viewInput) viewInput.value = url;
  if (viewEmptyEl) viewEmptyEl.classList.add('hidden');
  try { localStorage.setItem(VIEW_LAST, url); } catch (_) {}
  updateViewChrome();
}
function viewErr(reason) {
  const err = panelRoots.view && panelRoots.view.querySelector('#view-err');
  if (!err) return;
  err.textContent = 'разрешены только локальные адреса — ' + reason;
  err.classList.remove('hidden');
}
function viewClearErr() {
  const err = panelRoots.view && panelRoots.view.querySelector('#view-err');
  if (err) err.classList.add('hidden');
}
function viewTry(raw) {
  const v = localOnlyUrl(raw);
  if (!v.ok) { viewErr(v.reason); return false; }
  viewClearErr();
  viewNavigate(v.url);
  return true;
}
/* общий вход для моста: и своё окно, и панель во вкладке хоста */
function viewHandleRequest(url) {
  const v = localOnlyUrl(url);
  if (v.ok) { viewClearErr(); viewNavigate(v.url); }
}
/* открыть URL в «Просмотре» из другого окна: пишем мост, при необходимости
   поднимаем окно панели (POST /api/windows/restore), затем применяем локально. */
async function openInView(url) {
  const v = localOnlyUrl(url);
  if (!v.ok) { log('✗ URL не локальный: ' + v.reason); return; }
  try { localStorage.setItem(VIEW_REQ, v.url); } catch (_) {}
  const open = viewIsOpen();
  if (!panelRoots.view && open !== true) {
    log('открываю окно «Просмотр»…');
    try { await api('/api/windows/restore', jsonOpts('POST', { win: 'view' })); }
    catch (e) { log('✗ окно «Просмотр»: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/restore' : e.message)); }
    pollUi();
  }
  if (panelRoots.view) viewHandleRequest(v.url);
  log('просмотр: ' + v.url);
}
function updateViewChrome() {
  if (!ch.v || !ch.v.cur) return;
  ch.v.cur.textContent = viewInput && viewInput.value ? ('адрес: ' + viewInput.value) : 'адрес не задан';
}
function buildView(root) {
  panelRoots.view = root;
  root.innerHTML =
    '<div class="panel view-panel">' +
      '<div class="toolbar">' + winControlsHtml() +
        '<span class="spacer"></span>' +
        '<span class="dim">только локальные веб-UI: http://127.0.0.1|localhost:порт</span>' +
      '</div>' +
      '<div class="view-top">' +
        '<form class="view-bar out" id="view-form">' +
          '<input id="view-url" class="fld" placeholder="http://127.0.0.1:3000/" autocomplete="off" spellcheck="false">' +
          '<button class="btn95" type="submit">открыть</button>' +
          '<button id="view-reload" class="btn95" type="button" title="перезагрузить текущий адрес">⟳ перезагрузить</button>' +
        '</form>' +
        '<div id="view-err" class="view-err hidden"></div>' +
      '</div>' +
      '<div class="view-holder well grow">' +
        '<iframe id="view-frame" class="view-frame" referrerpolicy="no-referrer"></iframe>' +
        '<div id="view-empty" class="view-empty">адрес не задан — введите локальный URL вида http://127.0.0.1:3000/</div>' +
      '</div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  viewFrame = root.querySelector('#view-frame');
  viewInput = root.querySelector('#view-url');
  viewEmptyEl = root.querySelector('#view-empty');
  root.querySelector('#view-form').addEventListener('submit', (e) => { e.preventDefault(); viewTry(viewInput.value); });
  root.querySelector('#view-reload').addEventListener('click', () => { if (viewFrame && viewFrame.src) viewFrame.src = viewFrame.src; });
  bindWinControls(root);
  /* стартовый адрес: явный запрос (из другого окна) важнее последнего */
  let start = '';
  try { start = localStorage.getItem(VIEW_REQ) || localStorage.getItem(VIEW_LAST) || ''; } catch (_) {}
  if (start) { const v = localOnlyUrl(start); if (v.ok) viewNavigate(v.url); else viewInput.value = start; }
  chromeFillers.view = () => {
    ch.v = {};
    setChrome((mk) => {
      ch.v.cur = mk('span', 'dim');
      const b = mk('button', 'btn95', '⟳ перезагрузить');
      b.type = 'button';
      b.addEventListener('click', () => { if (viewFrame && viewFrame.src) viewFrame.src = viewFrame.src; });
    });
    updateViewChrome();
  };
  fillChrome();
}

export { buildView, openInView, viewFrame };
