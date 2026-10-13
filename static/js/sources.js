/* static/js/sources.js — панель «Источники» (herdr/omp/git/vault/zen/memory).
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc, log } from './lib/dom.js';
import { api, jsonOpts } from './lib/api.js';
import { bindWinControls, ch, chromeFillers, curRefreshIv, fmtAge, hideErr, lastSyncSec, panelError, panelRoots, probeSet, scheduleRetry, setChrome, winControlsHtml } from './core.js';
import { onTick } from './lib/ticker.js';

let srcCache = [];
/* =====================================================================
   панель «источники» (кнопка синка живёт в статус-строке)
   =================================================================== */
const SYNC_LABELS = { herdr: 'рабочих столов', omp: 'сессий', git: 'репозиториев', vault: 'заметок', zen: 'пространств', memory: 'фактов' };

function updateSyncAge() {
  if (ch.s && ch.s.age) ch.s.age.textContent = 'последний синк: ' + fmtAge(lastSyncSec);
}

function renderSources(list) {
  const tb = $('#stbl tbody');
  if (!tb) return;
  tb.innerHTML = '';
  for (const s of list) {
    const tr = el('tr');
    tr.append(
      el('td', null, '<b>' + esc(s.name) + '</b>'),
      el('td', 'num', s.count != null ? String(s.count) : '—'),
      el('td', null, '<span class="' + (s.ok ? 'ok-dot' : 'err-dot') + '">●</span> ' + (s.ok ? 'ок' : 'ошибка')),
      el('td', 'num dim', fmtAge(s.last_sync)),
    );
    tb.appendChild(tr);
  }
}

async function loadSources() {
  try {
    const d = await api('/api/sources');
    srcCache = Array.isArray(d.sources) ? d.sources : [];
    renderSources(srcCache);
    hideErr('sources');
    probeSet(true);
  } catch (e) {
    panelError('источники', e, 'sources');
    scheduleRetry(loadSources, 'sources');
  }
}

async function syncSources() {
  const btns = document.querySelectorAll('[data-act="sync"]');
  btns.forEach((b) => { b.disabled = true; });
  const logBox = $('#synclog');
  if (logBox) logBox.innerHTML = '<div class="dim">синхронизация…</div>';
  log('синхронизация источников…');
  try {
    const r = await api('/api/sync', jsonOpts('POST', {}));
    if (logBox) {
      logBox.innerHTML = '';
      for (const key of Object.keys(r)) {
        if (key === 'ok' || key === 'duration') continue;
        const v = r[key] || {};
        const parts = Object.keys(v).filter((k) => k !== 'ok').map((k) => SYNC_LABELS[k] || k + ': ' + v[k]);
        logBox.appendChild(el('div', null,
          '<span class="' + (v.ok ? 'ok-dot' : 'err-dot') + '">●</span> ' + esc(key) +
          ' — ' + (v.ok ? 'ок' : 'ошибка') + (parts.length ? ' (' + esc(parts.join(', ')) + ')' : '')));
      }
      logBox.appendChild(el('div', 'dim', 'готово за ' + (r.duration != null ? r.duration : '?') + 'с'));
    }
    log('синхронизация готова' + (r.duration != null ? ' за ' + r.duration + 'с' : ''));
    probeSet(true);
    loadSources();
  } catch (e) {
    if (logBox) logBox.innerHTML = '<div class="err-dot">✗ нет связи с бэкендом: ' + esc(e.message) + '</div>';
    log('✗ синхронизация не удалась: ' + e.message);
    probeSet(false);
  } finally {
    btns.forEach((b) => { b.disabled = false; });
  }
}

function buildSources(root) {
  panelRoots.sources = root;
  const q = (s) => root.querySelector(s);
  root.innerHTML =
    '<div class="panel src-panel">' +
      '<div class="toolbar">' +
        '<button id="btn-sync-src" class="btn95 ic tb-fallback" type="button" data-act="sync" title="синхронизировать все источники" aria-label="синхронизировать источники">⇄</button>' +
        '<span class="dim">записи по источникам; синк обновляет индекс</span>' +
        '<span class="spacer"></span>' +
        winControlsHtml() +
      '</div>' +
      '<div class="scroll well grow">' +
        '<table class="tbl" id="stbl"><thead><tr>' +
          '<th>источник</th><th>записей</th><th>статус</th><th>синк</th>' +
        '</tr></thead><tbody></tbody></table>' +
      '</div>' +
      '<div class="synclog in" id="synclog"><div class="dim">журнал синхронизации пуст</div></div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  q('#btn-sync-src').addEventListener('click', syncSources);
  bindWinControls(root);
  loadSources();
  onTick('sources', curRefreshIv, loadSources);
  chromeFillers.sources = () => {
    ch.s = {};
    setChrome((mk) => {
      const b = mk('button', 'btn95 ic', '⇄');
      b.type = 'button';
      b.dataset.act = 'sync';
      b.title = 'синхронизировать все источники';
      b.setAttribute('aria-label', 'синхронизировать источники');
      b.addEventListener('click', syncSources);
      ch.s.age = mk('span', 'dim');
    });
    updateSyncAge();
  };
  fillChromeSources();
}
function fillChromeSources() { if (chromeFillers.sources) chromeFillers.sources(); }

export { buildSources, loadSources, renderSources, srcCache, updateSyncAge };
