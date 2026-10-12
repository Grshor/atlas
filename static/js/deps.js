/* static/js/deps.js — панель «Зависимости».
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc } from './lib/dom.js';
import { api } from './lib/api.js';
import { bindWinControls, ch, chromeFillers, fillChrome, hideErr, panelError, panelRoots, probeSet, scheduleRetry, setChrome, state, winControlsHtml } from './core.js';

let depsCache = null;
/* =====================================================================
   панель «зависимости» (ASCII-граф) + происхождение связей
   =================================================================== */
/* модалка «откуда?»: происхождение связи (read-only) */
function originDialog(e) {
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out');
  dlg.appendChild(el('h3', null, 'откуда эта связь?'));
  const o = e && e.origin;
  if (!o || (!o.label && !o.ref && !o.raw)) {
    dlg.appendChild(el('div', 'dim', 'происхождение неизвестно'));
  } else {
    if (o.label) dlg.appendChild(el('div', null, '<b>' + esc(o.label) + '</b>'));
    if (o.ref) {
      const r = el('div', 'dlg-ref', esc(o.ref));
      r.title = 'щёлкните, чтобы выделить целиком';
      dlg.appendChild(r);
    }
    if (o.raw) dlg.appendChild(el('pre', 'dlg-raw', esc(o.raw)));
    if (o.source) dlg.appendChild(el('div', 'dim', 'тип: ' + (o.source === 'seed' ? 'seed-данные в коде' : 'файл связей')));
  }
  const btns = el('div', 'dbtns');
  const ok = el('button', 'btn95', 'закрыть');
  btns.appendChild(ok);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);
  ok.focus();
  const done = () => { ov.remove(); document.removeEventListener('keydown', key, true); };
  const key = (ev) => { if (ev.key === 'Escape') { ev.stopPropagation(); done(); } };
  document.addEventListener('keydown', key, true);
  ok.addEventListener('click', done);
  ov.addEventListener('mousedown', (ev) => { if (ev.target === ov) done(); });
}

function updateDepsChrome() {
  if (!ch.d) return;
  const d = depsCache || {};
  const edges = d.edges || [];
  const s = d.summary || {};
  const strict = s.strict != null ? s.strict : edges.filter((e) => e.kind === 'strict').length;
  const soft = s.soft != null ? s.soft : edges.filter((e) => e.kind !== 'strict').length;
  if (ch.d.count) ch.d.count.textContent = 'связей ' + edges.length + ' (строгих ' + strict + ' · мягких ' + soft + ')';
  const cyc = (s.cycles || []).length;
  if (ch.d.cyc) {
    ch.d.cyc.textContent = cyc ? '⟲ циклов: ' + cyc + '!' : '';
    ch.d.cyc.classList.toggle('cycle', !!cyc);
  }
}

function renderDeps(d) {
  const g = $('#graph');
  if (!g) return;
  g.innerHTML = '';
  const edges = (d && d.edges) || [];
  for (const e of edges) {
    const strict = e.kind === 'strict';
    const touched = !!state.selected && (e.from === state.selected || e.to === state.selected);
    const line = el('div', 'gl' + (touched ? ' sel-edge' : ''));
    line.innerHTML = esc(e.from) + ' ' +
      '<span class="arr-' + (strict ? 'strict' : 'soft') + '">' + (strict ? '──строгая──▶' : '┈┈мягкая┈┈▶') + '</span> ' +
      esc(e.to) + (e.note ? '<span class="note">  — ' + esc(e.note) + '</span>' : '');
    if (e.origin && e.origin.ref) line.title = e.origin.ref;
    const ob = el('button', 'mini dep-src', 'откуда?');
    ob.type = 'button';
    ob.title = 'показать происхождение связи';
    ob.addEventListener('click', (ev) => { ev.stopPropagation(); originDialog(e); });
    line.appendChild(ob);
    g.appendChild(line);
  }
  if (!edges.length) g.appendChild(el('div', 'dim', '— связей не найдено —'));
  const s = (d && d.summary) || {};
  const cycles = s.cycles || [];
  for (const c of cycles) {
    g.appendChild(el('div', 'cycle', '⟲ цикл: ' + esc(Array.isArray(c) ? c.join(' → ') : String(c))));
  }
  const sum = $('#dsum');
  if (sum) {
    sum.innerHTML = 'строгих ' + esc(s.strict != null ? s.strict : '?') + ' · мягких ' + esc(s.soft != null ? s.soft : '?') +
      (cycles.length ? ' · <span class="cycle" style="display:inline">циклов: ' + cycles.length + '!</span>' : '');
  }
  const warns = s.warnings || [];
  const dw = $('#dwarn');
  if (dw) {
    dw.textContent = warns.length ? '⚠ ' + warns.join(' · ') : '';
    dw.classList.toggle('hidden', !warns.length);
  }
  updateDepsChrome();
}

async function loadDeps() {
  try {
    const d = await api('/api/deps');
    depsCache = d;
    renderDeps(d);
    hideErr('deps');
    probeSet(true);
  } catch (e) {
    panelError('зависимости', e, 'deps');
    scheduleRetry(loadDeps, 'deps');
  }
}

function buildDeps(root) {
  panelRoots.deps = root;
  root.innerHTML =
    '<div class="panel">' +
      '<div class="deps-head">' +
        '<span><span class="arr-strict">──строгая──▶</span> строгая (блокирует)</span>' +
        '<span><span class="arr-soft">┈┈мягкая┈┈▶</span> мягкая</span>' +
        '<span class="cycle">⟲ — цикл</span>' +
        '<span class="spacer"></span>' +
        winControlsHtml() +
      '</div>' +
      '<div class="scroll well grow"><div class="graph" id="graph"></div></div>' +
      '<div class="dsum well" id="dsum"></div>' +
      '<div class="dwarn dim hidden" id="dwarn"></div>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  bindWinControls(root);
  loadDeps();
  chromeFillers.deps = () => {
    ch.d = {};
    setChrome((mk) => {
      ch.d.count = mk('span', 'dim');
      ch.d.cyc = mk('span', 'dim');
    });
    updateDepsChrome();
  };
  fillChrome();
}

export { buildDeps, depsCache, loadDeps, renderDeps };
