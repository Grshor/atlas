/* static/js/board.js — панель «Доска» (2×2) + делегация дел: чипы, лог, ревью.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, el, esc, log } from './lib/dom.js';
import { TICK_LOADERS, onTick } from './lib/ticker.js';
import { api, jsonOpts } from './lib/api.js';
import { askDialog, confirmDialog } from './lib/modal.js';
import { bindWinControls, ch, chromeFillers, curRefreshIv, hideErr, panelError, panelRoots, probeSet, scheduleRetry, setChrome, state, winControlsHtml } from './core.js';
import { fmtSecs } from './lib/fmt.js';
import { mkMenu, mkSelect, mselCloseOpen } from './lib/menu.js';
import { selectProject } from './projects.js';

let boardCache = null;
/* =====================================================================
   панель «доска» (Эйзенхеймер 2×2) + перетаскиваемый разделитель рядов
   =================================================================== */
const QUADS = [
  ['q1', 'срочно · важно'],
  ['q2', 'несрочно · важно'],
  ['q3', 'срочно · неважно'],
  ['q4', 'несрочно · неважно'],
];

/* разделитель рядов доски: 20–80%, хранится в atlas.board.split */
function loadBoardSplit() {
  let v = null;
  try { v = parseInt(localStorage.getItem('atlas.board.split'), 10); } catch (_) {}
  if (!Number.isFinite(v)) v = 50;
  return Math.min(80, Math.max(20, v));
}
function bindBoardSplit(qgrid, split) {
  split.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    split.classList.add('drag');
    document.body.classList.add('rowsizing');
    const move = (ev) => {
      const r = qgrid.getBoundingClientRect();
      if (!r.height) return;
      const pct = Math.min(80, Math.max(20, ((ev.clientY - r.top) / r.height) * 100));
      qgrid.style.gridTemplateRows = pct.toFixed(1) + 'fr 6px ' + (100 - pct).toFixed(1) + 'fr';
      split.dataset.pct = String(Math.round(pct));
    };
    const up = () => {
      split.classList.remove('drag');
      document.body.classList.remove('rowsizing');
      document.removeEventListener('mousemove', move);
      document.removeEventListener('mouseup', up);
      const pct = Math.min(80, Math.max(20, parseInt(split.dataset.pct || '50', 10) || 50));
      try { localStorage.setItem('atlas.board.split', String(pct)); } catch (_) {}
    };
    document.addEventListener('mousemove', move);
    document.addEventListener('mouseup', up);
  });
}

/* активные (не выполненные) дела квадранта и общий список выполненных */
function boardActive(list) { return (list || []).filter((it) => !it.done); }
function boardDone(data) {
  const d = data || {};
  const out = [];
  for (const [qk] of QUADS) for (const it of (d[qk] || [])) if (it.done) out.push(it);
  if (Array.isArray(d.done)) for (const it of d.done) if (!out.some((x) => x.id === it.id)) out.push(it);
  return out;
}

function updateBoardChrome() {
  if (!ch.b || !ch.b.count) return;
  const d = boardCache || {};
  const act = QUADS.map(([qk]) => boardActive(d[qk]).length);
  const parts = QUADS.map(([qk], i) => qk + ' ' + act[i]);
  const dn = boardDone(d).length;
  ch.b.count.textContent = 'дел ' + act.reduce((a, v) => a + v, 0) + ' (' + parts.join(' · ') + ')' +
    (dn ? ' · выполнено: ' + dn : '');
}

/* ---------- делегация дела (доска): чип состояния + кнопка ▶ ----------
   Контракт бэкенда: у активного дела ключ `delegation` = null либо
   {state:'queued'|'running'|'review'|'needs-feedback'|'failed', target, ref,
   run_id, started, ended, exit}. Старый бэкенд ключа не отдаёт вовсе — тогда
   ни чипа, ни ▶, и в журнал ничего не пишем. Ревью/лог — модалки поверх доски;
   опрос лога
   (GET …/run раз в секунду) и тиканье секунд на чипе «идёт» живут на ОДНОМ
   общем 1-с интервале, чтобы не плодить таймеры. Автообновление доски, пока
   что-то бежит, ускоряется до 10 с — тем же единственным тиком доски. */
const DELEG_STATES = {
  queued:  { mark: '◻', label: 'ждёт',     cls: 'deleg-queued' },
  running: { mark: '⏳', label: 'идёт',     cls: 'deleg-running' },
  review:  { mark: '👁', label: 'на ревью', cls: 'deleg-review' },
  'needs-feedback': { mark: '?', label: 'есть вопросы', cls: 'deleg-feedback' },
  failed:  { mark: '✗', label: 'провал',    cls: 'deleg-failed' },
};
const DELEG_TARGETS = [
  { value: 'orchestrator', label: 'оркестратору', hint: 'omp-оркестратор', needsProject: false },
  { value: 'worktree', label: 'воркеру в worktree', hint: 'worktree проекта', needsProject: true },
  { value: 'dir', label: 'воркеру в каталоге проекта', hint: 'каталог проекта', needsProject: true },
];
const DELEG_FAST_MS = 10000; /* доска: пока есть бегущая делегация — опрос чаще */
let boardTickMs = 0;         /* текущий интервал тика доски (не перевзводим зря) */
let delegTickTimer = null;   /* 1-с интервал: живые секунды на чипе «идёт» */

/* живая доска: SSE /api/events. Событие — намёк «данные поменялись», не
   payload: доска перечитывается с debounce 300 мс, открытый ревью-диалог
   своего дела добирает evidence сразу. База — та же, что у api() (webview
   Tauri живёт не на origin страницы). Старый бэкенд без эндпоинта → ES
   закрывается после первой ошибки, опросный тик остаётся как был. */
let esTimer = null;
let reviewLive = null; /* {id, refresh} открытого ревью-диалога */
function scheduleBoardRefresh() {
  clearTimeout(esTimer);
  esTimer = setTimeout(() => { esTimer = null; loadBoard(); }, 300);
}
function initBoardEvents() {
  if (typeof EventSource === 'undefined') return;
  const base = window.__ATLAS_API || '';
  const es = new EventSource(base + '/api/events');
  let alive = false;
  es.addEventListener('message', (e) => {
    alive = true;
    let m = null;
    try { m = JSON.parse(e.data); } catch (_) { return; }
    if (m.type === 'assets') { assetsUpdated(); return; } /* статики коснулись — чип, перезагрузка только вручную */
    if (reviewLive && reviewLive.id === m.id && (m.type === 'run' || m.type === 'gate')) reviewLive.refresh();
    scheduleBoardRefresh();
  });
  es.addEventListener('error', () => {
    /* 404 старого бэкенда или обрыв: живой ES переподключается сам, мёртвый
       (404) закрываем — доска остаётся на обычном опросе, без спама ретраев */
    if (!alive) es.close();
  });
}

/* ---------- горячая замена статики: бэкенд смотрит на static/ на диске ----
   Изменение ловится SSE-событием {type:"assets"}. Автоперезагрузки НЕТ: в
   поля пишут — сюрприз-перезагрузка теряет набранное (черновики переживают
   reload только у фидбек-полей). Вместо неё — чип: R/щелчок → location.reload().
   Чип вешается на body (не в панель): вкладки переключаются, слушатель SSE
   живёт один на страницу. */
function assetsUpdated() {
  log('интерфейс обновлён — R или клик по плашке — перезагрузка');
  let chip = document.querySelector('.assets-chip');
  if (chip) return; /* уже висит — не плодим */
  chip = el('button', 'assets-chip', 'интерфейс обновлён — нажми R (или клик) для перезагрузки');
  chip.type = 'button';
  chip.title = 'статика панели изменилась на диске';
  chip.addEventListener('click', () => location.reload());
  document.body.appendChild(chip);
}
/* R (или р/к — русская раскладка) при висящем чипе перезагружает страницу,
   а не обновляет панель: capture-фаза перехватывает раньше общего
   обработчика app.js. В полях ввода и под оверлеями — как в app.js, не мешаем. */
document.addEventListener('keydown', (e) => {
  if (!document.querySelector('.assets-chip')) return;
  if (/^(INPUT|SELECT|TEXTAREA)$/.test(document.activeElement && document.activeElement.tagName || '')) return;
  if (document.querySelector('.overlay')) return;
  if (/^[rрк]$/i.test(e.key)) {
    e.preventDefault();
    e.stopImmediatePropagation();
    location.reload();
  }
}, true);

const targetLabel = (v) => { const t = DELEG_TARGETS.find((x) => x.value === v); return t ? t.label : String(v || '?'); };
const delegBusy = (dg) => !!(dg && (dg.state === 'queued' || dg.state === 'running'));
function delegSecs(started) {
  const s = Number(started);
  if (!Number.isFinite(s) || s <= 0) return '';
  return Math.max(0, Math.round(Date.now() / 1000 - s)) + 'с';
}
function delegStateText(st) {
  const d = DELEG_STATES[st];
  return d ? (d.mark + ' ' + d.label) : String(st || '?');
}
function boardHasRunning() {
  const d = boardCache || {};
  const scan = (arr) => (arr || []).some((it) => it && it.delegation && it.delegation.state === 'running');
  for (const [qk] of QUADS) if (scan(d[qk])) return true;
  return scan(d.done);
}
/* один тик доски: 10 с, пока что-то бежит; иначе обычный ui.autorefresh_sec */
function armBoardTick(force) {
  const want = boardHasRunning() ? DELEG_FAST_MS : curRefreshIv;
  if (!force && want === boardTickMs && TICK_LOADERS.board) return;
  boardTickMs = want;
  onTick('board', want, loadBoard);
}
function delegTick() {
  const chips = document.querySelectorAll('.chip.deleg-running[data-started]');
  for (const c of chips) {
    const s = c.querySelector('.deleg-secs');
    if (s) s.textContent = delegSecs(c.dataset.started);
  }
  if (!chips.length) delegTickStop();
}
function delegTickStart() { if (!delegTickTimer) delegTickTimer = setInterval(delegTick, 1000); }
function delegTickStop() { if (delegTickTimer) { clearInterval(delegTickTimer); delegTickTimer = null; } }
function delegEnsureTick() {
  if (document.querySelector('.chip.deleg-running[data-started]')) delegTickStart();
}

/* чип состояния на карточке: ◻ ждёт · ⏳ идёт (+живые секунды) · 👁 на ревью · ✗ провал */
function delegChip(it) {
  const dg = it.delegation;
  const st = dg && dg.state ? String(dg.state) : '';
  const meta = DELEG_STATES[st] || { mark: '•', label: st || '?', cls: '' };
  const chip = el('span', 'chip deleg ' + meta.cls);
  chip.appendChild(document.createTextNode(meta.mark + ' ' + meta.label));
  if (st === 'running') {
    chip.dataset.started = String((dg && dg.started) || '');
    chip.append(document.createTextNode(' '), el('span', 'deleg-secs', delegSecs(dg.started)));
  }
  chip.title = 'делегация: ' + targetLabel(dg && dg.target) +
    (dg && dg.ref ? ' · ' + dg.ref : '') + (dg && dg.run_id != null ? ' · run ' + dg.run_id : '') +
    (st === 'running' ? ' — клик откроет живой журнал' : '');
  if (st === 'running') {
    chip.addEventListener('click', (e) => { e.stopPropagation(); openDelegLog(it); });
  } else if (st === 'review' || st === 'needs-feedback') {
    chip.addEventListener('click', (e) => { e.stopPropagation(); openDelegReview(it); });
  } else if (st === 'failed') {
    chip.addEventListener('click', (e) => { e.stopPropagation(); openDelegLog(it); });
    /* хвост последних строк лога — в tooltip, лениво (один раз) */
    chip.dataset.tail = '';
    chip.addEventListener('mouseenter', () => { if (!chip.dataset.tail) delegLoadTail(it, chip); });
  }
  return chip;
}
async function delegLoadTail(it, chip) {
  chip.dataset.tail = '1';
  try {
    const r = await api('/api/board/item/' + it.id + '/run');
    const lines = Array.isArray(r && r.lines) ? r.lines : [];
    let t = 'провал делегации';
    if (lines.length) t += ':\n' + lines.slice(-8).join('\n');
    if (r && r.exit != null) t += '\nexit ' + r.exit + ' · ' + fmtSecs(r.duration_s);
    chip.title = t;
  } catch (_) { /* хвост недоступен — оставляем прежний tooltip */ }
}
/* кнопка действий: ▶ (меню целей) — либо «⏳ посмотреть лог», пока делегация идёт */
function delegButton(it) {
  const busy = delegBusy(it.delegation);
  const b = el('button', 'mini deleg-go', busy ? '⏳' : '▶');
  b.type = 'button';
  b.title = busy ? '⏳ живой журнал' : 'делегировать задачу';
  b.addEventListener('click', (e) => {
    e.stopPropagation();
    if (busy) openDelegLog(it); else delegOpenTargets(it, b);
  });
  return b;
}
/* пункты меню целей: worktree/каталог требуют проект; пока делегация идёт — всё выключено */
function delegMenuItem(it) {
  const busy = delegBusy(it.delegation);
  return DELEG_TARGETS.map((t) => {
    const need = t.needsProject && !it.project;
    return {
      label: t.label,
      hint: need ? 'нужен проект' : (busy ? 'идёт делегация' : t.hint),
      disabled: need || busy,
      action: () => delegPreview(it, t.value),
    };
  });
}
function delegOpenTargets(it, btn) {
  const r = btn.getBoundingClientRect();
  mkMenu(delegMenuItem(it), Math.round(r.left), Math.round(r.bottom + 2));
}

/* черновики фидбека провальных карточек: id дела → {open, text, caret, focus}.
   Доска перерисовывается ЦЕЛИКОМ по любому SSE-событию (debounce 300 мс) и по
   тику (10 с, пока что-то бежит) — открытая textarea «Возобновить» живёт в
   старом DOM и без этого буфера исчезает вместе с набранным текстом на
   следующем же событии. Запись заводится при рендере строки провала, правится
   по input/раскрытию, удаляется при отправке (reject) либо когда дело больше
   не в failed (pruneDelegFbDrafts на каждом renderBoard). */
const delegFbDrafts = new Map();
function pruneDelegFbDrafts(data) {
  const live = new Set();
  for (const [qk] of QUADS) {
    for (const it of boardActive(data[qk])) {
      if (it && it.delegation && it.delegation.state === 'failed') live.add(it.id);
    }
  }
  for (const id of [...delegFbDrafts.keys()]) {
    if (!live.has(id)) delegFbDrafts.delete(id);
  }
}

/* провал → восстановление на карточке: «Возобновить» (главная) раскрывает
   фидбек — reject вернёт дело в queued, следующий preview покажет сессию
   провального хода (resume=…) и «отправить» продолжит её; «Повторить» —
   свежий прогон по сохранённой цели (без фидбека сессия новая). Журнал и
   причина провала — как раньше, по клику на чип ✗. */
function delegFailedRow(it) {
  const row = el('div', 'deleg-failed-row');
  const rw = el('div', 'drow deleg-rej hidden');
  rw.appendChild(el('label', null, 'фидбек воркеру'));
  const ta = el('textarea', 'fld');
  ta.rows = 2;
  ta.spellcheck = false;
  ta.placeholder = 'что поправить / продолжай';
  rw.appendChild(ta);
  const rsend = el('button', 'mini deleg-rej-send', 'вернуть с фидбеком');
  rsend.type = 'button';
  rw.appendChild(rsend);
  row.appendChild(rw);
  /* реинкарнация черновика: было открыто и/или набрано — восстанавливаем
     (значение, каретка, фокус) вместо «схлопнуто и пусто» после перерисовки */
  const draft = delegFbDrafts.get(it.id) || { open: false, text: '', caret: 0, focus: false };
  delegFbDrafts.set(it.id, draft);
  if (draft.open) {
    rw.classList.remove('hidden');
    ta.value = draft.text;
  }
  ta.addEventListener('input', () => {
    draft.text = ta.value;
    draft.caret = ta.selectionStart;
  });
  ta.addEventListener('focus', () => { draft.focus = true; });
  /* Chrome шлёт blur ДО отвязки узла (isConnected ещё true), поэтому решаем
     отложенно: если к следующему тику поле выкинуто из DOM — это перерисовка
     доски, фокус «пережил» её и будет восстановлен; поле в документе — ушёл сам */
  ta.addEventListener('blur', () => {
    setTimeout(() => { if (ta.isConnected) draft.focus = false; }, 0);
  });
  const resume = el('button', 'mini deleg-resume', 'Возобновить');
  resume.type = 'button';
  resume.title = 'вернуть с фидбеком — воркер продолжит свою сессию';
  resume.addEventListener('click', () => {
    rw.classList.toggle('hidden');
    draft.open = !rw.classList.contains('hidden');
    if (draft.open) {
      ta.value = draft.text; /* поле могли пересобрать в свёрнутом виде — текст черновика вернём */
      ta.focus();
      draft.caret = ta.selectionStart;
    }
  });
  row.appendChild(resume);
  const target = it.delegation && it.delegation.target;
  if (target) {
    const retry = el('button', 'mini deleg-retry', 'Повторить');
    retry.type = 'button';
    retry.title = 'свежая делегация (' + targetLabel(target) + ') — без фидбека, новая сессия';
    retry.addEventListener('click', () => delegPreview(it, target));
    row.appendChild(retry);
  }
  /* фокус пережил перерисовку (DOM заменён под курсором, blur не успел
     сработать) — возвращаем его в новую textarea и ставим каретку на место;
     queueMicrotask: к этому моменту карточка уже прицеплена к документу */
  if (draft.focus) {
    queueMicrotask(() => {
      ta.focus();
      try { ta.setSelectionRange(draft.caret, draft.caret); } catch (_) {}
    });
  }
  rsend.addEventListener('click', async () => {
    const fb = ta.value.trim();
    if (!fb) { ta.focus(); return; } /* фидбек обязателен — хоть «продолжай» */
    rsend.disabled = true;
    try {
      await api('/api/board/item/' + it.id + '/reject', jsonOpts('POST', { feedback: fb }));
      delegFbDrafts.delete(it.id); /* отправлено — черновик больше не нужен */
      log('✓ возвращено с фидбеком: ' + (it.title || it.id));
      loadBoard();
      const t = (it.delegation && it.delegation.target) || null;
      if (t) delegPreview(it, t); /* явное подтверждение запуска: preview покажет resume=сессию */
    } catch (e) {
      rsend.disabled = false;
      log('✗ вернуть не удалось: ' + e.message);
      probeSet(false);
    }
  });
  return row;
}

/* подтверждение: собираем ТЗ (preview) и показываем read-only — отправка только по кнопке */
async function delegPreview(it, target) {
  let r;
  try {
    r = await api('/api/board/item/' + it.id + '/delegate/preview', jsonOpts('POST', { target }));
  } catch (e) {
    log('✗ не удалось собрать ТЗ: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без delegate/preview' : e.message));
    probeSet(false);
    return;
  }
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out');
  dlg.appendChild(el('h3', null, 'ТЗ для делегирования'));
  dlg.appendChild(el('div', 'dim', esc(it.title || it.id) + ' → ' + esc(targetLabel(target))));
  if (r && r.cwd) {
    const c = el('div', 'deleg-cwd', esc(String(r.cwd)));
    c.title = String(r.cwd);
    dlg.appendChild(c);
  }
  const pre = el('pre', 'deleg-brief', esc(String((r && r.brief) || '')));
  dlg.appendChild(pre);
  const btns = el('div', 'dbtns');
  const send = el('button', 'btn95', 'отправить');
  const cancel = el('button', 'btn95', 'отмена');
  btns.append(send, cancel);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);
  send.focus();
  const done = () => { ov.remove(); document.removeEventListener('keydown', key, true); };
  const key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(); } };
  document.addEventListener('keydown', key, true);
  cancel.addEventListener('click', done);
  ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(); });
  send.addEventListener('click', async () => {
    send.disabled = true; cancel.disabled = true;
    try {
      const res = await api('/api/board/item/' + it.id + '/delegate', jsonOpts('POST', { target }));
      it.delegation = {
        state: (res && res.state) || 'queued',
        target,
        ref: (res && res.ref) || null,
        run_id: res && res.run_id != null ? res.run_id : null,
        started: Date.now() / 1000, ended: null, exit: null,
      };
      log('✓ делегировано (' + targetLabel(target) + '): ' + (it.title || it.id));
      done();
      renderBoard(boardCache || {}); /* оптимистичный чип — сразу, не ждём опроса */
      armBoardTick();
      loadBoard();
    } catch (e) {
      send.disabled = false; cancel.disabled = false;
      const busy = String(e.message).indexOf('409') !== -1;
      log(busy ? '✗ делегация уже идёт: ' + (it.title || it.id) : '✗ делегировать не удалось: ' + e.message);
      if (!busy) probeSet(false);
    }
  });
}

/* предполагаемая причина провала: честная эвристика по последней строке хода
   (воркер умер молча — финального блока контракта нет). Узнаёт наблюдения
   поллера («наблюдатель: тишина N с…» — молчание и живой процесс — это
   контекст, не приговор), ошибку вызова, обрыв посреди инструмента и обрыв
   посреди текста; честно помечена как догадка. Сырой хвост остаётся рядом. */
function delegCauseBox(lines, r) {
  const box = el('div', 'deleg-cause');
  const cut = (s, n) => (s.length > n ? s.slice(0, n) + '…' : s);
  const last = [...(lines || [])].reverse().find((l) => l && l.trim());
  let why;
  if (last && last.startsWith('наблюдатель: тишина')) {
    why = 'на момент провала воркер молчал, процесс был жив (' + last +
      ') — возможно, долгий thinking; таймеры ход не убивают, отмена — вручную';
  } else if (r && r.exit != null && Number(r.exit) !== 0) {
    why = 'воркер завершился с кодом ' + r.exit + (last ? '; последняя строка хода: ' + last : '');
  } else if (!last) {
    why = 'воркер не успел ничего вывести';
  } else if (last.startsWith('! ')) {
    why = 'последняя строка хода — ошибка вызова: ' + last.slice(2);
  } else if (last.startsWith('· ')) {
    why = 'воркер прерван посреди работы; последнее действие: ' + last.slice(2);
  } else {
    why = 'воркер прерван во время ответа; последнее в ходе: ' + last;
  }
  box.appendChild(el('div', 'dl-stat', 'предполагаемая причина (эвристика по последней строке хода — не факт):'));
  box.appendChild(el('div', 'deleg-err', cut(why, 300)));
  return box;
}

/* живой журнал делегации (read-only): GET …/run раз в 2 с, ровно пока диалог
   открыт; финал — exit N · время и опрос стоп. Провал ведёт с предполагаемой
   причиной, сырой хвост — свёрнут; автопрокрутка только когда читатель и так
   внизу (якорь скролла, как у оркестратора). */
function openDelegLog(it) {
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out');
  dlg.appendChild(el('h3', null, 'журнал делегации — ' + (it.title || it.id)));
  const sub = el('div', 'dim');
  dlg.appendChild(sub);
  const out = el('div', 'tasks-out deleg-log');
  dlg.appendChild(out);
  const btns = el('div', 'dbtns');
  const cancel = el('button', 'btn95', 'отменить');
  const rev = el('button', 'btn95', 'к ревью');
  const close = el('button', 'btn95', 'закрыть');
  btns.append(cancel, rev, close);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);

  let state = (it.delegation && it.delegation.state) || 'running';
  const render = (r) => {
    state = (r && r.state) ? String(r.state) : state;
    const lines = r && Array.isArray(r.lines) ? r.lines.filter((l) => typeof l === 'string') : [];
    const stick = out.scrollTop + out.clientHeight >= out.scrollHeight - 12;
    out.textContent = '';
    if (state === 'failed' && !(r && typeof r.final === 'string' && r.final.trim())) {
      out.appendChild(delegCauseBox(lines, r));
      const det = el('details', 'deleg-tail');
      det.appendChild(el('summary', null, 'сырой хвост хода (' + lines.length + ' стр.)'));
      det.appendChild(el('pre', 'deleg-brief', lines.join('\n')));
      out.appendChild(det);
    } else {
      out.textContent = lines.length ? lines.join('\n') + '\n' : (state === 'queued' ? 'в очереди…\n' : '');
    }
    if (r && r.exit != null) {
      out.appendChild(el('span', 'exit ' + (Number(r.exit) === 0 ? 'ok' : 'err'),
        'exit ' + r.exit + ' · ' + fmtSecs(r.duration_s)));
    }
    if (stick) out.scrollTop = out.scrollHeight;
    sub.textContent = 'состояние: ' + delegStateText(state) +
      (r && r.started ? ' · ' + delegSecs(r.started) : '') +
      (state === 'running' || state === 'queued' ? ' · обновление 2 с' : '');
    cancel.classList.toggle('hidden', !(state === 'queued' || state === 'running'));
    rev.classList.toggle('hidden', state !== 'review');
  };
  let timer = null;
  const stopPoll = () => { if (timer) { clearInterval(timer); timer = null; } };
  const poll = async () => {
    let r = null;
    try { r = await api('/api/board/item/' + it.id + '/run'); }
    catch (e) { sub.textContent = '✗ журнал недоступен: ' + e.message; return; }
    render(r);
    if (!r || (r.state !== 'running' && r.state !== 'queued')) {
      stopPoll(); /* финал — опрос прекращаем, диалог оставляем открытым */
      loadBoard();
    }
  };
  const done = () => { stopPoll(); ov.remove(); document.removeEventListener('keydown', key, true); };
  const key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(); } };
  document.addEventListener('keydown', key, true);
  close.addEventListener('click', done);
  ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(); });
  cancel.addEventListener('click', async () => {
    if (!(await confirmDialog('отменить делегацию «' + (it.title || it.id) + '»?'))) return;
    try {
      await api('/api/board/item/' + it.id + '/delegate/cancel', jsonOpts('POST', {}));
      log('✓ делегация отменена: ' + (it.title || it.id));
      poll();
      loadBoard();
    } catch (e) { log('✗ отмена не удалась: ' + e.message); probeSet(false); }
  });
  rev.addEventListener('click', () => { done(); openDelegReview(it); });
  poll();
  timer = setInterval(poll, 2000); /* поллинг живёт ровно пока диалог открыт */
}

/* ревью делегации: stat-строка + diff (+, −, заголовки) + принять/вернуть */
function dlClass(line) {
  if (line.startsWith('@@')) return 'dl-hunk';
  if (/^(diff |index |new file|deleted file|similarity |rename |old mode|new mode|--- |\+\+\+ )/.test(line)) return 'dl-file';
  if (line.charAt(0) === '+') return 'dl-add';
  if (line.charAt(0) === '-') return 'dl-del';
  return 'dl-ctx';
}
/* строка итога воркера: заголовки контракта подсвечены */
function dlFinalClass(line) {
  return /^(СДЕЛАНО|ИЗМЕНЕНО|ПРОВЕРЕНО|ВОПРОСЫ|VERDICT)\b/.test(line.trim()) ? 'dl-hdr' : 'dl-ctx';
}
async function openDelegReview(it) {
  let d = null, err = null, lg = null;
  try { d = await api('/api/board/item/' + it.id + '/diff'); } catch (e) { err = e; }
  try { lg = await api('/api/board/item/' + it.id + '/run'); } catch (_) {}
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out');
  dlg.appendChild(el('h3', null, 'ревью делегации — ' + (it.title || it.id)));
  const box = el('div', 'deleg-diff');
  /* живая evidence-секция: итог воркера, команды, второе мнение, файлы,
     ход — перерисовывается по run-событию, пока диалог открыт (SSE). */
  const evBox = el('div', 'deleg-evidence');
  box.appendChild(evBox);
  const renderEvidence = (lgNow) => {
    evBox.textContent = '';
    /* итог воркера (СДЕЛАНО/ИЗМЕНЕНО/ПРОВЕРЕНО/ВОПРОСЫ) — синтез ревью: живёт
       выше diff, читается первым; вопросы без ответа → «вернуть с фидбеком». */
    const finalText = lgNow && typeof lgNow.final === 'string' ? lgNow.final : '';
    if (finalText.trim()) {
      evBox.appendChild(el('div', 'dl-stat', 'итог воркера:'));
      for (const line of finalText.split('\n')) {
        if (line.trim() === '') continue;
        evBox.appendChild(el('div', 'dl-line ' + dlFinalClass(line), esc(line)));
      }
      evBox.appendChild(el('div', 'dl-line dl-hdr', '—'));
    }
    /* proof-of-work: сколько команд воркер гонял и сколько упало (rpc-прогоны;
       процессные цели и старые прогоны ключа не отдают — строку не рисуем). */
    const cmds = lgNow && lgNow.cmds && typeof lgNow.cmds.count === 'number' ? lgNow.cmds : null;
    if (cmds) {
      evBox.appendChild(el('div', 'dl-stat', 'команд: ' + cmds.count + ' (падений: ' + (cmds.fails || 0) + ')'));
    }
    /* машинная верификация (гейт приёмки): воркер перечислил $-команды —
       гейт прогнал их сам. ok=false — красная строка, фидбек предзаполнен. */
    const gate = lgNow && lgNow.gate && typeof lgNow.gate.ran === 'number' ? lgNow.gate : null;
    if (lgNow && typeof lgNow.attempts === 'number' && lgNow.attempts > 0) {
      evBox.appendChild(el('div', 'dl-stat', 'автопилот: попытка ' + lgNow.attempts + ' из 2'));
    }
    if (gate && gate.ran > 0) {
      evBox.appendChild(el('div', 'dl-stat ' + (gate.ok ? 'gate-ok' : 'gate-fail'),
        'верификация гейта: ' + (gate.ok ? 'прошла' : 'УПАЛА') + ' (' + gate.ran + ' ком.)'));
      for (const s of (gate.results || []).slice(0, 6)) {
        evBox.appendChild(el('div', 'dl-line ' + (s.ok ? 'dl-ctx' : 'dl-del'),
          (s.ok ? '✓ ' : '✗ ') + (s.command || '')));
      }
      evBox.appendChild(el('div', 'dl-line dl-hdr', '—'));
    }
    /* второе мнение: холодный ревьюер (если включён в конфиге) смотрит diff
       свежей сессией и отвечает VERDICT: accept|block. Правом вето не обладает:
       accept/reject остаются за владельцем. null = ревьюер выключен/ещё думает. */
    const rv = lgNow && typeof lgNow.review === 'string' ? lgNow.review.trim() : '';
    if (rv) {
      evBox.appendChild(el('div', 'dl-stat', 'второе мнение (свежеконтекстный ревьюер):'));
      for (const line of rv.split('\n')) {
        if (line.trim() === '') continue;
        evBox.appendChild(el('div', 'dl-line ' + dlFinalClass(line), esc(line)));
      }
      evBox.appendChild(el('div', 'dl-line dl-hdr', '—'));
    } else if (lgNow && lgNow.state === 'review') {
      evBox.appendChild(el('div', 'dl-stat deleg-review-wait', 'второе мнение: ревьюер думает…'));
    }
    /* досье прогона файлами (data_dir/runs/<item>-r<run>/): клик открывает
       каталог в файловом менеджере; null = ещё не писалось / старый прогон. */
    if (lgNow && typeof lgNow.artifacts_dir === 'string' && lgNow.artifacts_dir) {
      const adir = lgNow.artifacts_dir;
      const alink = el('a', 'dl-stat deleg-artifacts', 'файлы прогона: ' + adir);
      alink.href = '#';
      alink.addEventListener('click', (e) => {
        e.preventDefault();
        api('/api/open', jsonOpts('POST', { path: adir })).catch((err2) => log('✗ не удалось открыть: ' + err2.message));
      });
      evBox.appendChild(alink);
    }
    /* ход воркера: его текст и ВОПРОСЫ живут здесь, а не в diff. Автономный
       работник спрашивает строками хода; ответ — «вернуть с фидбеком». */
    const runLines = lgNow && Array.isArray(lgNow.lines) ? lgNow.lines.filter((l) => typeof l === 'string') : [];
    if (runLines.length) {
      evBox.appendChild(el('div', 'dl-stat', 'ход воркера' + (lgNow && lgNow.state === 'running' ? ' (ещё идёт)' : '') + ' — его вопросы здесь, ответ — «вернуть с фидбеком»:'));
      for (const l of runLines.slice(-80)) evBox.appendChild(el('div', 'dl-line dl-ctx', esc(l)));
      evBox.appendChild(el('div', 'dl-line dl-hdr', '— diff проекта —'));
    }
  };
  renderEvidence(lg);
  if (err) {
    box.appendChild(el('div', 'deleg-err', '✗ не удалось получить diff: ' + esc(err.message)));
  } else {
    /* коммиты воркера (diff считается от стартового HEAD — коммиты видны) */
    const commits = String((d && d.commits) || '').trim();
    if (commits) {
      box.appendChild(el('div', 'dl-stat', 'коммиты воркера от старта делегации:'));
      for (const line of commits.split('\n')) {
        if (line.trim() === '') continue;
        box.appendChild(el('div', 'dl-line dl-ctx', esc(line)));
      }
      box.appendChild(el('div', 'dl-line dl-hdr', '— diff проекта —'));
    }
    if (d && d.stale) {
      box.appendChild(el('div', 'dl-line dl-trunc', '… стартовый HEAD не записан (старое делегирование) — diff от текущего HEAD, закоммиченное может быть не видно'));
    }
    box.appendChild(el('div', 'dl-stat', esc(String((d && d.stat) || '') || 'изменений нет')));
    const diff = String((d && d.diff) || '');
    for (const line of diff.split('\n')) box.appendChild(el('div', 'dl-line ' + dlClass(line), esc(line)));
    if (d && d.truncated) box.appendChild(el('div', 'dl-line dl-trunc', '… diff обрезан'));
  }
  dlg.appendChild(box);
  const msg = el('div', 'deleg-err hidden');
  dlg.appendChild(msg);
  /* возврат с фидбеком: textarea появляется по кнопке, отправка — явной кнопкой;
     при needs-feedback она открыта сразу — это единственный канал ответа */
  const rw = el('div', 'drow deleg-rej hidden');
  rw.appendChild(el('label', null, 'фидбек воркеру'));
  const ta = el('textarea', 'fld');
  ta.rows = 3;
  ta.spellcheck = false;
  ta.placeholder = 'что не так с результатом…';
  rw.appendChild(ta);
  const rsend = el('button', 'mini deleg-rej-send', 'отправить фидбек');
  rsend.type = 'button';
  rw.appendChild(rsend);
  dlg.appendChild(rw);
  const btns = el('div', 'dbtns');
  const merge = el('button', 'btn95', 'принять и смержить');
  const plain = el('button', 'btn95', 'принять без мержа');
  const rej = el('button', 'btn95', 'вернуть с фидбеком');
  const close = el('button', 'btn95', 'закрыть');
  btns.append(merge, plain, rej, close);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);
  close.focus();
  const answerFirst = lg && lg.state === 'needs-feedback';
  if (answerFirst) { rw.classList.remove('hidden'); ta.focus(); }

  /* live-регистрация: run-события (SSE) для этого дела перерисуют evidence
     без переоткрытия диалога — вердикт ревьюера появляется сам. gate-событие
     тоже добирает: верификация завершается позже settle. */
  const refreshEvidence = async () => {
    try {
      const lgNow = await api('/api/board/item/' + it.id + '/run');
      renderEvidence(lgNow);
      const g = lgNow && lgNow.gate;
      if (g && g.ran > 0 && g.ok === false && ta && !ta.value.trim()) {
        ta.value = 'гейт приёмки упал:\n' + (g.results || [])
          .map((s) => (s.ok ? '✓ ' : '✗ ') + (s.command || '') + (s.ok ? '' : '\n  ' + String(s.tail || '').trim().slice(0, 300)))
          .join('\n');
        rw.classList.remove('hidden');
      }
    } catch (_) { /* сеть мигнула — старая секция честно остаётся */ }
  };
  reviewLive = { id: it.id, refresh: refreshEvidence };

  const done = () => { ov.remove(); document.removeEventListener('keydown', key, true); if (reviewLive && reviewLive.id === it.id) reviewLive = null; };
  const key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(); } };
  document.addEventListener('keydown', key, true);
  close.addEventListener('click', done);
  ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(); });
  const showErr = (t) => { msg.textContent = t; msg.classList.remove('hidden'); };
  const accept = async (m) => {
    merge.disabled = plain.disabled = true;
    msg.classList.add('hidden');
    try {
      const r = await api('/api/board/item/' + it.id + '/accept', jsonOpts('POST', { merge: !!m }));
      if (r && r.ok === false) { showErr('✗ ' + ((r && r.detail) || 'не удалось принять')); merge.disabled = plain.disabled = false; return; }
      log((m ? '✓ принято и смержено: ' : '✓ принято: ') + (it.title || it.id));
      done();
      loadBoard();
    } catch (e) {
      showErr('✗ ' + e.message);
      merge.disabled = plain.disabled = false;
    }
  };
  merge.addEventListener('click', () => accept(true));
  plain.addEventListener('click', () => accept(false));
  rej.addEventListener('click', () => { rw.classList.remove('hidden'); ta.focus(); });
  rsend.addEventListener('click', async () => {
    const fb = ta.value.trim();
    rsend.disabled = true;
    msg.classList.add('hidden');
    try {
      await api('/api/board/item/' + it.id + '/reject', jsonOpts('POST', { feedback: fb }));
      log('✓ возвращено с фидбеком: ' + (it.title || it.id));
      done();
      loadBoard();
    } catch (e) { rsend.disabled = false; showErr('✗ ' + e.message); }
  });
}

/* одна карточка доски: активная (✓ сделать, перенос квадранта) или выполненная
   (↺ вернуть). Выполненные — приглушены и зачёркнуты. */
const STEPS_CAP = 20;
function stepsDoneCount(steps) { return (steps || []).filter((s) => s && s.done).length; }
/* замена набора шагов целиком (PUT): один шаг переключён, остальные как были.
   Переключение сразу и в локальном наборе (it.steps): быстрый второй тумблер
   и открытый следом диалог читают уже новый набор, а не снапшот на момент
   рендера — иначе второй PUT молча затирает первый переключённый шаг. */
async function toggleStep(it, step) {
  step.done = !step.done;
  const steps = (it.steps || []).map((s) => ({ text: s.text, done: !!s.done }));
  try {
    await api('/api/board/item/' + it.id + '/steps', jsonOpts('PUT', { steps }));
    log((step.done ? '↺ шаг снят: ' : '✓ шаг сделан: ') + (step.text || ''));
    loadBoard();
  } catch (e) {
    step.done = !step.done; /* не сохранилось — возвращаем чекбокс как был */
    const c404 = String(e.message).indexOf('404') !== -1;
    log('✗ шаг не сохранён: ' + (c404 ? 'бэкенд без PUT steps' : e.message));
    loadBoard();
  }
}
function boardCard(it, done) {
  const selCard = !!it.project && it.project === state.selected;
  const card = el('article', 'card out' + (selCard ? ' card-sel' : '') + (done ? ' card-done' : ''));
  /* id-чип: уведомления ссылаются на «дело #N» — без него владелец не сопоставит их с карточкой */
  card.title = 'дело #' + it.id + ': ' + (it.title || '(без названия)') +
    (selCard ? ' · проект выбран: ' + it.project : '');
  const title = el('div', 'card-title', esc(it.title || '(без названия)'));
  title.appendChild(el('span', 'chip item-id', '#' + it.id));
  card.appendChild(title);
  /* чип делегации — на активных карточках, когда бэкенд отдал ключ delegation */
  if (!done && it.delegation !== undefined && it.delegation) title.appendChild(delegChip(it));
  /* чеклист: рендерим только у активных карточек и только когда ключ steps
     пришёл с бэкенда (старый бэкенд — никаких чекбоксов) */
  const hasSteps = !done && Array.isArray(it.steps);
  if (hasSteps && it.steps.length) {
    title.appendChild(el('span', 'chip steps-count',
      stepsDoneCount(it.steps) + '/' + it.steps.length));
    const list = el('div', 'steps');
    for (const s of it.steps) {
      const row = el('label', 'step' + (s.done ? ' done' : ''));
      const cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.checked = !!s.done;
      cb.addEventListener('change', () => toggleStep(it, s));
      cb.addEventListener('click', (e) => e.stopPropagation());
      row.appendChild(cb);
      row.appendChild(el('span', 'step-text', esc(s.text || '')));
      row.addEventListener('click', (e) => e.stopPropagation()); /* не диалог описания */
      list.appendChild(row);
    }
    card.appendChild(list);
  }
  /* описание дела: превью в 3 строки (целиком — в tooltip), клик — диалог;
     выполненные остаются лёгкими, без описания и файлов */
  if (!done) {
    const desc = String(it.description == null ? '' : it.description).trim();
    if (desc) {
      const prev = el('div', 'card-desc', esc(desc));
      prev.title = 'описание (клик — диалог):\n' + desc;
      prev.addEventListener('click', () => editBoardItem(it));
      card.appendChild(prev);
    }
    /* провал → восстановление: «Возобновить» с фидбеком / «Повторить» */
    if (it.delegation && it.delegation.state === 'failed') card.appendChild(delegFailedRow(it));
  }
  const meta = el('div', 'card-meta');
  if (it.project) {
    const chip = el('span', 'chip proj click', esc(it.project));
    chip.title = 'выбрать проект «' + it.project + '»';
    chip.addEventListener('click', () => selectProject(it.project));
    meta.appendChild(chip);
  }
  for (const d of (it.deps || [])) {
    const strict = d.kind === 'strict';
    meta.appendChild(el('span', 'chip dep-' + (strict ? 'strict' : 'soft'),
      (strict ? '▲ блок: ' : '◇ ') + esc(d.to_project)));
  }
  /* файлы дела — хвост пути, целиком в tooltip */
  if (!done) for (const f of (it.files || [])) {
    const p = String((f && f.path) || '');
    if (!p) continue;
    const fc = el('span', 'chip file', esc(p.split('/').pop() || p));
    fc.title = 'файл дела: ' + p + (f && f.label ? ' — ' + f.label : '');
    meta.appendChild(fc);
  }
  meta.appendChild(el('span', 'push'));
  if (it.project) {
    const go = el('button', 'mini', '→ проект');
    go.type = 'button';
    go.title = 'запомнить выбор: ' + it.project;
    go.addEventListener('click', () => selectProject(it.project));
    meta.appendChild(go);
  }
  if (done) {
    const back = el('button', 'mini', '↺ вернуть');
    back.type = 'button';
    back.title = 'вернуть дело в квадрант (done: false)';
    back.addEventListener('click', () => setBoardDone(it, false));
    meta.appendChild(back);
  } else {
    /* ▶ делегировать / ⏳ посмотреть лог — только когда ключ delegation есть */
    if (it.delegation !== undefined) meta.appendChild(delegButton(it));
    const ed = el('button', 'mini', '✎');
    ed.type = 'button';
    ed.title = 'описание и файлы дела';
    ed.addEventListener('click', () => editBoardItem(it));
    meta.appendChild(ed);
    const ok = el('button', 'mini card-ok', '✓');
    ok.type = 'button';
    ok.title = 'выполнено';
    ok.addEventListener('click', () => setBoardDone(it, true));
    meta.appendChild(ok);
    meta.appendChild(mkSelect({
      title: 'перенести в другой квадрант',
      options: QUADS.map(([qk, ql]) => ({ value: qk, label: qk + ' · ' + ql })),
      value: it.quadrant,
      onchange: async (v) => {
        try {
          await api('/api/board/item/' + it.id, jsonOpts('PATCH', { quadrant: v }));
          loadBoard();
        } catch (e) { log('✗ не удалось перенести дело: ' + e.message); probeSet(false); }
      },
    }));
  }
  card.appendChild(meta);
  return card;
}

async function setBoardDone(it, done) {
  log((done ? 'выполняю' : 'возвращаю') + ' дело: ' + (it.title || it.id) + '…');
  try {
    await api('/api/board/item/' + it.id, jsonOpts('PATCH', { done: !!done }));
    log(done ? '✓ выполнено: ' + (it.title || it.id) : '↺ возвращено: ' + (it.title || it.id));
    loadBoard();
  } catch (e) { log('✗ не удалось изменить дело: ' + e.message); probeSet(false); }
}

/* диалог дела: описание (textarea, Ctrl+Enter — сохранить) + файлы (список с ✕
   и «+ файл» с @-подсказкой). Подсказка ищет по файлам проекта дела
   (GET /api/project/{name}/files?q=…) с задержкой 250 мс; список выглядит и
   ведёт себя как попап mkSelect (тот же прижим к вьюпорту, стрелки/Enter/Esc).
   На старом бэкенде у элемента нет ключей description/files — секции гаснут
   с пометкой «недоступно на текущем бэкенде», остальное работает. */
function editBoardItem(it) {
  const hasDesc = it && it.description !== undefined;
  const hasFiles = it && it.files !== undefined;
  const origFiles = hasFiles ? (it.files || []).map((f) => String((f && f.path) || '')).filter(Boolean) : [];
  const files = origFiles.slice();
  const ov = el('div', 'overlay');
  const dlg = el('div', 'dlg out');
  dlg.appendChild(el('h3', null, 'дело — ' + (it.title || '(без названия)')));
  dlg.appendChild(el('div', 'dim', 'квадрант ' + (it.quadrant || '?') +
    (it.project ? ' · проект ' + it.project : ' · без проекта')));

  /* --- описание --- */
  const drow = el('div', 'drow');
  drow.appendChild(el('label', null, 'описание'));
  const ta = el('textarea', 'fld');
  ta.rows = 5;
  ta.spellcheck = false;
  ta.placeholder = 'что делать: контекст, шаги, ссылки…';
  if (hasDesc) ta.value = String(it.description == null ? '' : it.description);
  else { ta.disabled = true; drow.classList.add('off'); }
  drow.appendChild(ta);
  if (!hasDesc) drow.appendChild(el('div', 'dim', 'недоступно на текущем бэкенде'));
  dlg.appendChild(drow);

  /* --- файлы --- */
  const arow = el('div', 'drow');
  arow.appendChild(el('label', null, 'файлы'));
  const list = el('div', 'att-list');
  const renderList = () => {
    list.innerHTML = '';
    if (!files.length) list.appendChild(el('div', 'dim', 'файлов нет'));
    for (const p of files) {
      const r = el('div', 'att-row');
      const nm = el('span', 'att-name', esc(p.split('/').pop() || p));
      nm.title = p;
      const x = el('button', 'mini', '✕');
      x.type = 'button';
      x.title = 'убрать файл: ' + p;
      x.addEventListener('click', () => {
        const i = files.indexOf(p);
        if (i >= 0) files.splice(i, 1);
        renderList();
      });
      r.append(nm, x);
      list.appendChild(r);
    }
  };
  renderList();
  arow.appendChild(list);
  const addWrap = el('div', 'att-add');
  const inp = el('input', 'fld');
  inp.type = 'text';
  inp.spellcheck = false;
  inp.autocomplete = 'off';
  inp.placeholder = '+ файл — часть имени файла или путь после @';
  inp.title = 'подсказка ищет по файлам проекта дела; Enter добавляет путь в список';
  addWrap.appendChild(inp);
  arow.appendChild(addWrap);
  if (!hasFiles) { arow.classList.add('off'); inp.disabled = true; }
  else if (!it.project) { inp.disabled = true; inp.placeholder = '+ файл — у дела нет проекта (подсказке нужен проект)'; }
  if (!hasFiles) arow.appendChild(el('div', 'dim', 'недоступно на текущем бэкенде'));
  dlg.appendChild(arow);

  /* --- шаги (чеклист): секция только когда бэкенд отдал ключ steps;
     старый бэкенд — секции нет вовсе. Сохранение — PUT полным набором. --- */
  const hasSteps = it && it.steps !== undefined;
  const origSteps = hasSteps ? (it.steps || []).map((s) => ({ text: String((s && s.text) || ''), done: !!(s && s.done) })) : null;
  const steps = hasSteps ? origSteps.map((s) => ({ text: s.text, done: s.done })) : null;
  let stepsDirty = false; /* пользователь уже правил шаги — серверную правку не подставляем */
  if (hasSteps) {
    const strow = el('div', 'drow');
    strow.appendChild(el('label', null, 'шаги (чеклист)'));
    const slist = el('div', 'att-list');
    const renderSteps = () => {
      slist.innerHTML = '';
      if (!steps.length) slist.appendChild(el('div', 'dim', 'шагов нет'));
      for (const s of steps) {
        const r = el('div', 'step-row');
        const cb = document.createElement('input');
        cb.type = 'checkbox';
        cb.checked = s.done;
        cb.title = 'шаг сделан';
        cb.addEventListener('change', () => { s.done = cb.checked; stepsDirty = true; });
        const sinp = el('input', 'fld');
        sinp.type = 'text';
        sinp.spellcheck = false;
        sinp.autocomplete = 'off';
        sinp.value = s.text;
        sinp.placeholder = 'текст шага';
        sinp.addEventListener('input', () => { s.text = sinp.value; stepsDirty = true; });
        const x = el('button', 'mini', '✕');
        x.type = 'button';
        x.title = 'убрать шаг';
        x.addEventListener('click', () => {
          const i = steps.indexOf(s);
          if (i >= 0) steps.splice(i, 1);
          stepsDirty = true;
          renderSteps();
        });
        r.append(cb, sinp, x);
        slist.appendChild(r);
      }
      const add = el('button', 'mini step-add', '+ шаг');
      add.type = 'button';
      add.disabled = steps.length >= STEPS_CAP;
      add.title = steps.length >= STEPS_CAP ? 'не больше 20 шагов' : 'добавить пустой шаг';
      add.addEventListener('click', () => {
        if (steps.length < STEPS_CAP) { steps.push({ text: '', done: false }); stepsDirty = true; renderSteps(); }
      });
      slist.appendChild(add);
      strow.appendChild(slist);
    };
    renderSteps();
    dlg.appendChild(strow);
    /* буфер шагов собран из карточки, которая могла устареть (тумблер перед
       открытием диалога, правка из второго окна): перечитываем доску и, пока
       пользователь ещё ничего не правил, тихо подменяем буфер на серверный
       набор — иначе «Сохранить» PUT-ом затрёт чужую/свежую правку целиком */
    api('/api/board').then((b) => {
      if (stepsDirty || !ov.isConnected) return;
      const pools = [b && b.q1, b && b.q2, b && b.q3, b && b.q4, b && b.done];
      const fresh = pools.flat(Infinity).find((x) => x && x.id === it.id);
      if (!fresh || !Array.isArray(fresh.steps)) return;
      const incoming = fresh.steps.map((s) => ({ text: String((s && s.text) || ''), done: !!(s && s.done) }));
      if (JSON.stringify(incoming) === JSON.stringify(steps)) return;
      steps.length = 0;
      for (const s of incoming) steps.push(s);
      origSteps.length = 0;
      for (const s of incoming) origSteps.push({ text: s.text, done: s.done });
      renderSteps();
    }).catch(() => {}); /* нет доски — работаем с тем, что было на карточке */
  }

  /* --- @-подсказка: попап в стиле mkSelect --- */
  let pop = null, items = [], active = -1, seq = 0, tmr = 0;
  const closePop = () => {
    clearTimeout(tmr);
    if (pop && pop.parentNode) pop.parentNode.removeChild(pop);
    pop = null; items = []; active = -1;
  };
  const clampPop = () => {
    if (!pop) return;
    const vw = document.documentElement.clientWidth, vh = document.documentElement.clientHeight;
    const r = inp.getBoundingClientRect();
    pop.style.minWidth = Math.round(r.width) + 'px';
    pop.style.maxWidth = Math.max(80, vw - 8) + 'px';
    const w = pop.offsetWidth, h = pop.offsetHeight;
    const left = Math.max(4, Math.min(r.left, vw - w - 4));
    const below = r.bottom + 2;
    const top = (below + h > vh - 4 && r.top - h - 2 >= 4) ? r.top - h - 2 : Math.max(4, Math.min(below, vh - h - 4));
    pop.style.left = Math.round(left) + 'px';
    pop.style.top = Math.round(top) + 'px';
  };
  const setActive = (i) => {
    if (!items.length) return;
    active = (i + items.length) % items.length;
    items.forEach((o, k) => o.el.classList.toggle('active', k === active));
    try { items[active].el.scrollIntoView({ block: 'nearest' }); } catch (_) {}
  };
  const fill = (paths) => {
    if (!pop) return;
    const lst = pop.firstChild;
    lst.innerHTML = '';
    items = []; active = -1;
    if (paths === null) { lst.appendChild(el('div', 'msel-opt dim', 'недоступно на текущем бэкенде')); return; }
    if (!paths.length) { lst.appendChild(el('div', 'msel-opt dim', 'ничего не найдено')); return; }
    for (const p of paths) {
      const o = el('div', 'msel-opt', esc(p));
      o.title = p;
      o.addEventListener('mousedown', (e) => e.preventDefault()); /* фокус остаётся в поле */
      o.addEventListener('click', () => insertPath(p));
      lst.appendChild(o);
      items.push({ el: o, p });
    }
    setActive(0);
    clampPop();
  };
  const query = () => {
    const v = inp.value;
    const at = v.lastIndexOf('@');
    const qStr = (at >= 0 ? v.slice(at + 1) : v).trim();
    const my = ++seq;
    api('/api/project/' + encodeURIComponent(it.project) + '/files?q=' + encodeURIComponent(qStr))
      .then((r) => { if (my === seq && pop) fill(r && Array.isArray(r.files) ? r.files.map(String).slice(0, 20) : []); })
      .catch(() => { if (my === seq && pop) fill(null); });
  };
  const openPop = () => {
    if (pop || inp.disabled) return;
    pop = el('div', 'msel-pop att-pop');
    pop.appendChild(el('div', 'msel-list'));
    pop.firstChild.appendChild(el('div', 'msel-opt dim', 'поиск…'));
    document.body.appendChild(pop);
    clampPop();
  };
  const insertPath = (p) => {
    closePop();
    if (!p) return;
    if (files.includes(p)) { inp.value = ''; return; }
    if (files.length >= 20) { log('✗ у дела не больше 20 файлов'); return; }
    files.push(p);
    renderList();
    inp.value = '';
  };
  inp.addEventListener('input', () => {
    clearTimeout(tmr);
    if (!pop) { if (!inp.value.trim()) return; openPop(); }
    tmr = setTimeout(() => { clampPop(); query(); }, 250); /* дебаунс запроса */
  });
  inp.addEventListener('keydown', (e) => {
    if (pop) {
      if (e.key === 'ArrowDown') { e.preventDefault(); setActive(active + 1); return; }
      if (e.key === 'ArrowUp') { e.preventDefault(); setActive(active - 1); return; }
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closePop(); return; } /* первый Esc — только подсказка */
      if (e.key === 'Enter') {
        e.preventDefault(); e.stopPropagation(); /* не отправлять диалог */
        if (active >= 0 && items[active]) insertPath(items[active].p);
        return;
      }
    } else if (e.key === 'Enter') {
      /* без подсказки: ручной ввод пути (хвост после @, если он есть) */
      e.preventDefault(); e.stopPropagation();
      const v = inp.value.trim();
      if (!v) return;
      const at = v.lastIndexOf('@');
      insertPath((at >= 0 ? v.slice(at + 1) : v).trim());
    }
  });

  /* --- кнопки/клавиши диалога --- */
  const btns = el('div', 'dbtns');
  const ok = el('button', 'btn95', 'Сохранить');
  const cancel = el('button', 'btn95', 'Отмена');
  btns.append(ok, cancel);
  dlg.appendChild(btns);
  ov.appendChild(dlg);
  document.body.appendChild(ov);
  (hasDesc ? ta : ok).focus();
  const done = () => { ov.remove(); document.removeEventListener('keydown', key, true); closePop(); };
  const key = (e) => {
    if (e.key === 'Escape') {
      if (pop) return; /* первый Esc закрывает только подсказку (поле само) */
      e.stopPropagation(); done();
    } else if (e.key === 'Enter' && e.ctrlKey) {
      const t = e.target;
      if (t === ta || t === inp) { e.preventDefault(); e.stopPropagation(); submit(); }
    } else if (e.key === 'Enter' && e.target && e.target.tagName !== 'TEXTAREA' &&
               e.target !== inp && e.target.tagName !== 'BUTTON') {
      /* кнопки получают Enter как нативный клик (ОК/Отмена сами) */
      e.stopPropagation(); submit();
    }
  };
  const submit = async () => {
    ok.disabled = true; cancel.disabled = true;
    try {
      if (hasDesc) {
        const d = ta.value.trim();
        await api('/api/board/item/' + it.id, jsonOpts('PATCH', { description: d || null }));
      }
      if (hasFiles && JSON.stringify(files) !== JSON.stringify(origFiles)) {
        await api('/api/board/item/' + it.id + '/files', jsonOpts('PUT', { paths: files }));
      }
      if (hasSteps) {
        const newSteps = steps.filter((s) => s.text.trim()).map((s) => ({ text: s.text.trim(), done: !!s.done }));
        if (JSON.stringify(newSteps) !== JSON.stringify(origSteps)) {
          await api('/api/board/item/' + it.id + '/steps', jsonOpts('PUT', { steps: newSteps }));
        }
      }
      log('✓ дело обновлено: ' + (it.title || it.id));
      done();
      loadBoard();
    } catch (e) {
      ok.disabled = false; cancel.disabled = false;
      log('✗ не удалось сохранить дело: ' + e.message); probeSet(false);
    }
  };
  document.addEventListener('keydown', key, true);
  ok.addEventListener('click', submit);
  cancel.addEventListener('click', done);
  ov.addEventListener('mousedown', (e) => { if (e.target === ov) done(); });
}

function renderBoardDone(items) {
  const block = $('#board-done');
  if (!block) return;
  const list = block.querySelector('#board-done-list');
  const sum = block.querySelector('#board-done-sum');
  const n = items.length;
  const num = block.querySelector('#board-done-n');
  if (num) num.textContent = String(n);
  if (sum) sum.textContent = 'выполнено: ' + n;
  block.classList.toggle('hidden', !n);
  if (list) {
    list.innerHTML = '';
    const sorted = items.slice().sort((a, b) => (b.id || 0) - (a.id || 0));
    for (const it of sorted) list.appendChild(boardCard(it, true));
  }
}

function renderBoard(data) {
  mselCloseOpen(); /* открытый список на карточке снимаем вместе со старыми карточками */
  pruneDelegFbDrafts(data); /* дело вышло из failed → черновик фидбека долой */
  for (const [q] of QUADS) {
    const box = $('#cards-' + q);
    if (!box) continue;
    box.innerHTML = '';
    const items = boardActive(data[q]).slice();
    /* дела выбранного проекта — первыми в своём квадранте */
    items.sort((a, b) => ((b.project === state.selected) ? 1 : 0) - ((a.project === state.selected) ? 1 : 0));
    for (const it of items) box.appendChild(boardCard(it, false));
    if (!box.children.length) box.appendChild(el('div', 'dim', 'пусто'));
    const cnt = $('#qcount-' + q);
    if (cnt) cnt.textContent = String(items.length);
  }
  renderBoardDone(boardDone(data));
  updateBoardChrome();
  armBoardTick();    /* 10 с, пока что-то бежит; иначе обычный интервал */
  delegEnsureTick(); /* живые секунды на чипах «идёт» */
}

/* монотонный номер запроса доски: ответ, пришедший после более свежего
   запроса (медленный GET, застрявший за PUT), молча выбрасывается — иначе
   запоздалый снапшот перерисовывает доску назад и чекбоксы «отскакивают» */
let boardFetchSeq = 0;

async function loadBoard() {
  const my = ++boardFetchSeq;
  try {
    const d = await api('/api/board');
    if (my !== boardFetchSeq) return; /* устарел: свежий запрос уже рендерит */
    boardCache = d && typeof d === 'object' ? d : {};
    renderBoard(boardCache);
    hideErr('board');
    probeSet(true);
  } catch (e) {
    if (my !== boardFetchSeq) return; /* ошибку обработает более свежий запрос */
    panelError('доска', e, 'board');
    scheduleRetry(loadBoard, 'board');
  }
}

function buildBoard(root) {
  panelRoots.board = root;
  const q = (s) => root.querySelector(s);
  const pct = loadBoardSplit();
  let grid = '<div class="quad-grid" id="qgrid" style="grid-template-rows:' + pct + 'fr 6px ' + (100 - pct) + 'fr">';
  for (const [qk, label] of QUADS) {
    grid += '<section class="quad ' + qk + ' out">' +
      '<header><span class="mark"></span><span class="ttl">' + qk + ' · ' + label + '</span>' +
      '<span class="qcount" id="qcount-' + qk + '" title="активных дел">0</span>' +
      '<button class="mini" type="button" data-add="' + qk + '" title="новое дело в ' + qk + '">+ дело</button></header>' +
      '<div class="cards in" id="cards-' + qk + '"></div></section>';
    if (qk === 'q2') grid += '<div class="board-split" id="bsplit" title="потянуть — высота рядов доски (20–80%)"></div>';
  }
  grid += '</div>';
  root.innerHTML =
    '<div class="panel" style="grid-template-rows:auto minmax(0,1fr) auto auto">' +
      '<div class="toolbar">' + winControlsHtml() +
        '<span class="spacer"></span>' +
        '<span class="dim">дела выбранного проекта — первыми в квадранте</span>' +
      '</div>' +
      grid +
      '<details class="done-block hidden" id="board-done">' +
        '<summary>выполненные (<span id="board-done-n">0</span>)</summary>' +
        '<div class="done-sum dim" id="board-done-sum">выполнено: 0</div>' +
        '<div class="done-list" id="board-done-list"></div>' +
      '</details>' +
      '<div class="errbox hidden"></div>' +
    '</div>';
  bindWinControls(root);
  bindBoardSplit(q('#qgrid'), q('#bsplit'));
  root.addEventListener('click', async (e) => {
    const qa = e.target && e.target.dataset && e.target.dataset.add;
    if (!qa) return;
    const vals = await askDialog('новое дело — ' + qa, [
      { label: 'название' },
      { label: 'проект (пусто — без проекта)' },
      { label: 'описание (необязательно)', multiline: true },
    ]);
    if (!vals) return;
    if (!vals[0]) { log('✗ дело без названия не сохранено'); return; }
    try {
      await api('/api/board/item', jsonOpts('POST', {
        title: vals[0], quadrant: qa, project: vals[1] || null,
        description: (vals[2] || '').trim() || null, /* файлы прикрепляются после создания, в диалоге дела */
      }));
      log('дело добавлено в ' + qa);
      loadBoard();
    } catch (err) { log('✗ не удалось добавить дело: ' + err.message); probeSet(false); }
  });
  loadBoard();
  armBoardTick(true);
  initBoardEvents();
  chromeFillers.board = () => {
    ch.b = {};
    setChrome((mk) => { ch.b.count = mk('span', 'dim'); });
    updateBoardChrome();
  };
  fillChromeBoard();
}
function fillChromeBoard() { if (chromeFillers.board) chromeFillers.board(); }

export { armBoardTick, boardCache, buildBoard, loadBoard, renderBoard };
