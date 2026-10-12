/* static/js/orchestrator.js — панель «Оркестратор»: чат поверх rpc-канала, подтверждения.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { API, api, jsonOpts } from './lib/api.js';
import { BUILDERS, WIN, activateTab, bindWinControls, ch, chromeFillers, panelRoots, pollUi, setChrome, setUiCache, state, uiCache, winControlsHtml } from './core.js';
import { el, esc, log } from './lib/dom.js';
import { onTick } from './lib/ticker.js';
import { sttAttachPtt, sttOrchInsert, sttRegister, sttShowHint } from './stt.js';

/* =====================================================================
   панель «Оркестратор» (?win=orchestrator): чат поверх rpc-канала.
   GET  /api/orch/history        → {events:[…]} — реплей при открытии;
   POST /api/orch {prompt}       → ТЕЛО ответа — SSE-поток канонических событий
                                   (fetch + ReadableStream, строки `data:`);
   GET  /api/orch/status         → {alive,pid,cmd,turn_active,pending:[…]};
   POST /api/orch/approve {id,allow}; POST /api/orch/cancel.
   События: turn_start{prompt} · text{delta} · thinking{delta} ·
   tool{name,detail,phase} · approval_request{id,tool,detail} ·
   approval_resolved{id,allow} · done{duration_s} · error{message}.
   Пока /api/orch/* отвечают 404 (старый бэкенд) — dim-строка и выключенный
   ввод, без падения и спама. Панель НИЧЕГО не отправляет сама при открытии.
   =================================================================== */
const ORCH_IV = 15000;          /* опрос /api/orch/status */
const ORCH_APPROVAL_SECS = 300; /* бэкенд авто-отклоняет подтверждение через 5 мин */
const ORCH_READY_MS = 8000;     /* сколько чип держит «готово · Nс» */

let orchAvailable = null;      /* null — неизвестно; true — канал есть; false — 404 */
let orchState = { alive: null, pid: null, cmd: null, turn_active: false, pending: [] };
let orchBusy = false;          /* ввод выключен: ход идёт */
let orchTurnStartedAt = null;  /* мс начала хода (локально/из статуса) */
let orchDoneFlash = null;      /* {text, until} — «готово · Nс» в чипе */
let orchScrollEl = null, orchListEl = null, orchApprEl = null, orchInputEl = null, orchSendEl = null, orchNewEl = null;
let orchCur = null;            /* текущий ход: {asstEl, asstText, thinkEl, thinkText, tools:Map} */
let orchStick = true;          /* автопрокрутка: держим нижний край, пока не отлистали */
let orchUnavailEl = null;      /* dim-строка «rpc-канал недоступен» */
const orchCards = new Map();   /* id → карточка подтверждения */

/* имя команды канала для чипа: «жив · omp · простой» */
function orchCmdName() {
  const c = orchState.cmd;
  const first = Array.isArray(c) ? c[0] : (typeof c === 'string' ? c.split(/\s+/)[0] : '');
  const base = String(first || 'omp').replace(/^.*\//, '');
  return base || 'omp';
}
function orchRunning() { return orchBusy || orchState.turn_active; }
function orchChipState() {
  if (orchAvailable === false) return { text: 'rpc-канал недоступен', cls: 'dim' };
  if (orchRunning()) {
    const secs = orchTurnStartedAt ? Math.max(0, Math.round((Date.now() - orchTurnStartedAt) / 1000)) : 0;
    return { text: 'работает · ' + secs + 'с', cls: 'work' };
  }
  if (orchDoneFlash && orchDoneFlash.until > Date.now()) return { text: orchDoneFlash.text, cls: 'ok' };
  if (orchState.alive) return { text: 'жив · ' + orchCmdName() + ' · простой', cls: 'ok' };
  return { text: 'не запущен — поднимется при первом запросе', cls: 'dim' };
}
function orchFillChrome() {
  if (!ch.o) return;
  const st = orchChipState();
  if (ch.o.status) { ch.o.status.textContent = st.text; ch.o.status.className = 'orch-st ' + st.cls; }
  if (ch.o.cancel) ch.o.cancel.classList.toggle('hidden', !orchRunning());
}
function fillChromeOrch() {
  ch.o = {};
  setChrome((mk) => {
    ch.o.status = mk('span', 'orch-st dim', '…');
    ch.o.cancel = mk('button', 'btn95 hidden', '■ прервать');
    ch.o.cancel.type = 'button';
    ch.o.cancel.id = 'btn-orch-cancel';
    ch.o.cancel.title = 'прервать ход';
    ch.o.cancel.addEventListener('click', orchCancel);
    const b = mk('button', 'btn95', '⟳ статус');
    b.type = 'button';
    b.title = 'перечитать состояние rpc-канала';
    b.addEventListener('click', orchPoll);
  });
  orchFillChrome();
}

/* ---------- автопрокрутка: держим нижний край, пока пользователь не отлистал ---------- */
function orchAutoScroll() {
  if (!orchScrollEl) return;
  if (orchStick) orchScrollEl.scrollTop = orchScrollEl.scrollHeight;
  else if (orchNewEl) orchNewEl.classList.remove('hidden');
}
function orchAppend(node) { if (orchListEl) { orchListEl.appendChild(node); orchAutoScroll(); } }
function orchNotice(text) { orchAppend(el('div', 'orch-notice dim', esc(text))); }
function orchErrLine(text) { const n = el('div', 'orch-err'); n.textContent = text; orchAppend(n); }

function orchUpdateInput() {
  const off = orchAvailable === false;
  const busy = orchRunning() || off;
  if (orchInputEl) {
    orchInputEl.disabled = busy;
    orchInputEl.placeholder = off ? 'rpc-канал недоступен (старый бэкенд)'
      : busy ? 'ход выполняется…' : 'сообщение оркестратору… (Enter — отправить, Shift+Enter — перенос)';
  }
  if (orchSendEl) orchSendEl.disabled = busy;
  orchFillChrome();
}

/* ---------- карточки подтверждения (одна на id; стек снизу, над вводом) ---------- */
function fmtCountdown(s) {
  const m = Math.floor(s / 60), ss = s % 60;
  return m + ':' + (ss < 10 ? '0' : '') + ss;
}
function orchRenderCountdown(card, now) {
  if (!card || card.state !== 'pending') return;
  const left = Math.max(0, Math.ceil((card.expiresAt - now) / 1000));
  if (card.timerEl) { card.timerEl.textContent = '⏳ ' + fmtCountdown(left); card.timerEl.title = 'бэкенд авто-отклонит запрос через 5 минут'; }
  if (left <= 0) orchResolveCard(card.id, false, { timeout: true });
}
function orchCard(rawId, tool, detail) {
  const id = String(rawId);
  let card = orchCards.get(id);
  if (card) return card;
  const wrap = el('div', 'orch-approval');
  const head = el('div', 'orch-approval-head');
  head.appendChild(el('span', 'orch-approval-tool', esc(tool != null ? tool : 'инструмент')));
  head.appendChild(el('span', 'spacer'));
  const timer = el('span', 'orch-approval-timer');
  head.appendChild(timer);
  wrap.appendChild(head);
  if (detail != null && String(detail) !== '') {
    const d = el('div', 'orch-approval-detail', esc(detail));
    d.title = String(detail);
    wrap.appendChild(d);
  }
  const btns = el('div', 'orch-approval-btns');
  const allow = el('button', 'btn95', 'Разрешить');
  allow.type = 'button';
  allow.title = 'разрешить вызов инструмента';
  const deny = el('button', 'btn95', 'Запретить');
  deny.type = 'button';
  deny.title = 'запретить вызов инструмента';
  btns.append(allow, deny);
  wrap.appendChild(btns);
  card = { id, el: wrap, state: 'pending', allow, deny, timerEl: timer, tool, detail, expiresAt: Date.now() + ORCH_APPROVAL_SECS * 1000 };
  allow.addEventListener('click', () => orchApprove(card, true));
  deny.addEventListener('click', () => orchApprove(card, false));
  orchCards.set(id, card);
  if (orchApprEl) { orchApprEl.appendChild(wrap); orchApprEl.classList.remove('hidden'); }
  orchRenderCountdown(card, Date.now());
  orchAutoScroll();
  return card;
}
function orchResolveCard(id, allow, opts) {
  const card = orchCards.get(id);
  if (!card || card.state !== 'pending') return;
  const timeout = !!(opts && opts.timeout);
  card.state = 'resolved';
  card.el.classList.add('resolved');
  card.allow.disabled = true;
  card.deny.disabled = true;
  const btns = card.el.querySelector('.orch-approval-btns');
  const mark = el('span', 'orch-approval-mark ' + (allow ? 'ok' : 'err'),
    timeout ? '✗ время вышло (авто-отказ)' : (allow ? '✓ разрешено' : '✗ запрещено'));
  if (btns) btns.replaceWith(mark); else card.el.appendChild(mark);
  if (card.timerEl) card.timerEl.textContent = timeout ? '5:00 истекли' : (allow ? 'разрешено' : 'запрещено');
}
function orchReenable(card) {
  if (card.state === 'pending') { card.allow.disabled = false; card.deny.disabled = false; }
}
async function orchApprove(card, allow) {
  card.allow.disabled = true;
  card.deny.disabled = true;
  log('оркестратор: ' + (allow ? 'разрешаю' : 'запрещаю') + ' — ' + (card.tool || card.id) + '…');
  try {
    /* бэкенд ждёт id строкой ({"id":"…"}): ключ карточки уже строка */
    const r = await api('/api/orch/approve', jsonOpts('POST', { id: card.id, allow: !!allow }));
    if (r && r.ok === false) { log('✗ ответ не принят: ' + (r.detail || 'причина не указана')); orchReenable(card); return; }
    /* обычно состояние пометит approval_resolved; метим сразу, чтобы не зависеть от него */
    orchResolveCard(card.id, allow);
  } catch (e) {
    log('✗ ответ не отправлен: ' + e.message);
    orchReenable(card);
  }
}

/* ---------- отрисовка событий (и живой поток, и реплей истории) ---------- */
function orchNewTurn() {
  orchCur = { asstEl: null, asstText: null, thinkEl: null, thinkText: '', tools: new Map() };
}
function orchEnsureTurn() { if (!orchCur) orchNewTurn(); return orchCur; }
function orchToggleThink(think) {
  if (!orchCur || !orchCur.thinkText) return;
  const next = think.nextElementSibling;
  if (next && next.classList.contains('orch-think-body')) {
    next.remove();
    think.classList.remove('open');
    think.textContent = 'думает…';
    return;
  }
  const body = el('pre', 'orch-think-body');
  body.textContent = orchCur.thinkText;
  think.after(body);
  think.classList.add('open');
  think.textContent = 'думает… (свернуть)';
  orchAutoScroll();
}
function orchToolEvent(ev) {
  const name = ev.name != null ? String(ev.name) : 'tool';
  const detail = ev.detail != null ? String(ev.detail) : '';
  const phase = ev.phase != null ? String(ev.phase) : 'start';
  const cur = orchEnsureTurn();
  const key = name + '\u0000' + detail;
  let arr = cur.tools.get(key);
  if (!arr) { arr = []; cur.tools.set(key, arr); }
  const isErr = phase === 'error' || phase === 'fail';
  let line = null;
  if (phase !== 'start' && phase !== 'begin') {
    for (let i = arr.length - 1; i >= 0; i--) if (arr[i].dataset.state === 'start') { line = arr[i]; break; }
  }
  if (!line) {
    line = el('div', 'orch-tool dim');
    line.innerHTML = '<span class="orch-tool-mark">' + (isErr ? '✗' : (phase === 'start' || phase === 'begin' ? '⠿' : '✓')) + '</span> ' +
      esc(name) + (detail ? ' <span class="orch-tool-detail">' + esc(detail) + '</span>' : '');
    line.title = name + (detail ? ' · ' + detail : '');
    line.dataset.state = (phase === 'start' || phase === 'begin') ? 'start' : (isErr ? 'error' : 'end');
    arr.push(line);
    orchAppend(line);
    return;
  }
  line.dataset.state = isErr ? 'error' : 'end';
  line.classList.toggle('orch-tool-err', isErr);
  const mark = line.querySelector('.orch-tool-mark');
  if (mark) mark.textContent = isErr ? '✗' : '✓';
  orchAutoScroll();
}
function orchApplyEvent(ev, replay) {
  if (!ev || typeof ev !== 'object') return;
  const t = ev.type || ev.kind; /* kind — старое поле, на случай смешанных кадров */
  switch (t) {
    case 'turn_start': {
      orchNewTurn();
      const prompt = ev.prompt != null ? String(ev.prompt) : '';
      const msg = el('div', 'orch-msg orch-user');
      msg.appendChild(el('div', 'orch-who', 'вы'));
      const b = el('div', 'orch-bubble');
      b.textContent = prompt;
      msg.appendChild(b);
      orchAppend(msg);
      if (!replay) { orchBusy = true; orchTurnStartedAt = Date.now(); orchDoneFlash = null; orchUpdateInput(); }
      break;
    }
    case 'text': {
      const cur = orchEnsureTurn();
      if (!cur.asstEl) {
        const msg = el('div', 'orch-msg orch-asst');
        msg.appendChild(el('div', 'orch-who', 'оркестратор'));
        cur.asstText = el('div', 'orch-bubble');
        msg.appendChild(cur.asstText);
        cur.asstEl = msg;
        orchAppend(msg);
      }
      cur.asstText.appendChild(document.createTextNode(ev.delta != null ? String(ev.delta) : ''));
      orchAutoScroll();
      break;
    }
    case 'thinking': {
      const cur = orchEnsureTurn();
      if (!cur.thinkEl) {
        const think = el('div', 'orch-think dim', 'думает…');
        think.tabIndex = 0;
        think.title = '';
        think.addEventListener('click', () => { if (orchCur && orchCur.thinkEl === think) orchToggleThink(think); });
        cur.thinkEl = think;
        cur.thinkText = '';
        orchAppend(think);
      }
      cur.thinkText += ev.delta != null ? String(ev.delta) : '';
      cur.thinkEl.title = cur.thinkText; /* развернуть — наведением (или кликом) */
      break;
    }
    case 'tool': orchToolEvent(ev); break;
    case 'approval_request': if (ev.id != null) orchCard(ev.id, ev.tool, ev.detail); break;
    case 'approval_resolved': if (ev.id != null) orchResolveCard(String(ev.id), !!ev.allow); break;
    case 'done': {
      const d = ev.duration_s != null && Number.isFinite(Number(ev.duration_s))
        ? (Math.round(Number(ev.duration_s) * 10) / 10) + 'с' : '';
      orchAppend(el('div', 'orch-done dim', '✓ готово' + (d ? ' · ' + d : '')));
      if (!replay) {
        orchBusy = false; orchState.turn_active = false; orchTurnStartedAt = null; orchCur = null;
        orchDoneFlash = { text: 'готово · ' + (d || '—'), until: Date.now() + ORCH_READY_MS };
        orchUpdateInput();
      }
      break;
    }
    case 'error': {
      orchErrLine('✗ ' + (ev.message != null ? String(ev.message) : 'ошибка'));
      if (!replay) { orchBusy = false; orchState.turn_active = false; orchTurnStartedAt = null; orchCur = null; orchUpdateInput(); }
      break;
    }
    default: break; /* неизвестный тип — молча игнорируем */
  }
}

/* ---------- загрузка: история → статус → опрос ---------- */
function orchSetAvailable(v) {
  if (orchAvailable === v) return;
  orchAvailable = v;
  if (v === false) {
    if (!orchUnavailEl) { orchUnavailEl = el('div', 'orch-notice dim', 'rpc-канал недоступен (старый бэкенд)'); orchAppend(orchUnavailEl); }
  } else if (orchUnavailEl) { orchUnavailEl.remove(); orchUnavailEl = null; }
  orchUpdateInput();
}
async function orchLoadHistory() {
  let d = null;
  try { d = await api('/api/orch/history'); }
  catch (e) {
    if (String(e.message).indexOf('404') !== -1) orchSetAvailable(false);
    return;
  }
  orchSetAvailable(true);
  const events = Array.isArray(d && d.events) ? d.events : [];
  if (!events.length) { orchNotice('история с прошлого запуска сервера недоступна'); return; }
  for (const ev of events) orchApplyEvent(ev, true);
  orchCur = null; /* реплей закончился — живой поток не должен вливаться в него */
}
function orchStatusStart(st) {
  for (const k of ['turn_started_at', 'started_at', 'since', 'turn_started_s']) {
    const v = Number(st && st[k]);
    if (Number.isFinite(v) && v > 0) return v > 1e12 ? v : v * 1000;
  }
  return 0;
}
function orchPendingExpiry(p) {
  for (const k of ['expires_at', 'expires']) {
    const v = Number(p && p[k]);
    if (Number.isFinite(v) && v > 0) return v > 1e12 ? v : v * 1000;
  }
  for (const k of ['created_at', 'ts', 'since']) {
    const v = Number(p && p[k]);
    if (Number.isFinite(v) && v > 0) return (v > 1e12 ? v : v * 1000) + ORCH_APPROVAL_SECS * 1000;
  }
  return 0;
}
function orchApplyStatus(st) {
  st = st && typeof st === 'object' ? st : {};
  orchState = {
    alive: !!st.alive,
    pid: st.pid != null ? st.pid : null,
    cmd: st.cmd != null ? st.cmd : null,
    turn_active: !!st.turn_active,
    pending: Array.isArray(st.pending) ? st.pending : [],
  };
  if (orchState.turn_active) {
    if (!orchTurnStartedAt) orchTurnStartedAt = orchStatusStart(st) || Date.now();
    orchBusy = true;
  } else {
    orchBusy = false; orchTurnStartedAt = null;
  }
  /* pending-подтверждения переживают перезагрузку: дорисовываем из статуса */
  for (const p of orchState.pending) {
    if (!p || p.id == null) continue;
    const card = orchCard(p.id, p.tool, p.detail);
    const exp = orchPendingExpiry(p);
    if (exp) card.expiresAt = exp;
    orchRenderCountdown(card, Date.now());
  }
  orchUpdateInput();
}
async function orchPoll() {
  let st = null;
  try { st = await api('/api/orch/status'); }
  catch (e) {
    if (String(e.message).indexOf('404') !== -1) orchSetAvailable(false);
    return;
  }
  orchSetAvailable(true);
  orchApplyStatus(st);
}
function orchTick() {
  const now = Date.now();
  if (orchDoneFlash && orchDoneFlash.until <= now) { orchDoneFlash = null; orchFillChrome(); }
  else if (orchRunning() || orchDoneFlash) orchFillChrome();
  for (const card of orchCards.values()) if (card.state === 'pending') orchRenderCountdown(card, now);
}
function refreshOrchestrator() { orchPoll(); }

/* ---------- SSE-поток ответа POST /api/orch ---------- */
function orch409Excerpt(raw) {
  const s = String(raw == null ? '' : raw);
  let v = null;
  try { v = JSON.parse(s); } catch (_) {}
  let p = '';
  if (v && typeof v === 'object') p = v.prompt || v.running_prompt || v.detail || v.error || '';
  else p = s;
  p = String(p || '').replace(/\s+/g, ' ').trim();
  return p.length > 80 ? p.slice(0, 80) + '…' : p;
}
function orchFeedLine(line) {
  if (!line) return false;
  if (line.charAt(0) === ':') return false;         /* SSE-комментарий */
  if (line.slice(0, 5) !== 'data:') return false;    /* event:/id:/retry: не нужны */
  const payload = line.slice(5).trim();
  if (!payload || payload === '[DONE]') return false;
  let ev = null;
  try { ev = JSON.parse(payload); } catch (_) { return false; }
  if (!ev || typeof ev !== 'object') return false;
  orchApplyEvent(ev, false);
  const t = ev.type || ev.kind;
  return t === 'done' || t === 'error';
}
async function orchReadStream(res, ctl) {
  const reader = res.body.getReader();
  const dec = new TextDecoder('utf-8', { fatal: false });
  let buf = '';
  let sawEnd = false;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let idx;
      while ((idx = buf.indexOf('\n')) !== -1) {
        const line = buf.slice(0, idx).replace(/\r$/, '');
        buf = buf.slice(idx + 1);
        if (orchFeedLine(line)) sawEnd = true;
      }
    }
    buf += dec.decode();
    if (buf.trim() && orchFeedLine(buf.replace(/\r$/, ''))) sawEnd = true;
  } catch (e) {
    if (!ctl || !ctl.signal.aborted) orchErrLine('✗ поток прерван: ' + (e && e.message ? e.message : 'ошибка'));
    sawEnd = true;
  }
  return sawEnd;
}
async function orchSend() {
  if (orchAvailable === false) { log('оркестратор: rpc-канал недоступен'); return; }
  if (orchRunning()) { log('оркестратор: ход уже выполняется'); return; }
  const text = ((orchInputEl && orchInputEl.value) || '').trim();
  if (!text) return;
  if (orchInputEl) orchInputEl.value = '';
  orchStick = true;
  if (orchNewEl) orchNewEl.classList.add('hidden');
  orchBusy = true; orchTurnStartedAt = Date.now(); orchDoneFlash = null;
  orchUpdateInput();
  const ctl = new AbortController();
  log('оркестратор: запрос отправлен');
  let res = null;
  try {
    res = await fetch(API + '/api/orch', Object.assign({ signal: ctl.signal }, jsonOpts('POST', { prompt: text })));
  } catch (e) {
    orchBusy = false; orchTurnStartedAt = null; orchUpdateInput();
    orchErrLine('✗ запрос не доставлен: ' + (e && e.message ? e.message : 'ошибка'));
    return;
  }
  if (res.status === 409) {
    let excerpt = '';
    try { excerpt = orch409Excerpt(await res.text()); } catch (_) {}
    /* бэкенд отдаёт {"detail":"ход уже идёт: <промпт>"} — показываем его как есть */
    orchErrLine('✗ ' + (excerpt || 'оркестратор занят другим ходом'));
    orchPoll(); /* статус подтвердит turn_active — ввод остаётся выключенным */
    return;
  }
  if (!res.ok) { orchBusy = false; orchTurnStartedAt = null; orchUpdateInput(); orchErrLine('✗ канал ответил ' + res.status); return; }
  if (!res.body) { orchBusy = false; orchTurnStartedAt = null; orchUpdateInput(); orchErrLine('✗ канал не отдал поток'); return; }
  const sawEnd = await orchReadStream(res, ctl);
  if (!sawEnd) orchPoll(); /* поток закрылся без done/error — спросим статус */
}
async function orchCancel() {
  if (ch.o && ch.o.cancel) ch.o.cancel.disabled = true;
  log('оркестратор: прерываю ход…');
  try {
    const r = await api('/api/orch/cancel', jsonOpts('POST', {}));
    if (r && r.ok === false) log('✗ прервать не удалось: ' + (r.detail || 'причина не указана'));
  } catch (e) { log('✗ прервать не удалось: ' + e.message); }
  if (ch.o && ch.o.cancel) ch.o.cancel.disabled = false;
  orchPoll();
}

/* открыть «Оркестратор»: вкладка хоста → активировать; уже отдельное окно →
   POST /api/windows/focus {win:"orchestrator"} (поднять существующее);
   закрыто → POST /api/windows/restore. 404 от focus (старый бэкенд) —
   деградация с честной строкой в журнале. */
async function openOrchestratorPanel() {
  let ui = null;
  try { ui = await api('/api/ui'); setUiCache(ui); } catch (_) { /* решаем по прежнему кэшу */ }
  const tabs = ui && Array.isArray(ui.tabs) ? ui.tabs : (uiCache && Array.isArray(uiCache.tabs) ? uiCache.tabs : []);
  const wins = ui && Array.isArray(ui.windows) ? ui.windows : (uiCache && Array.isArray(uiCache.windows) ? uiCache.windows : []);
  if (WIN === 'projects' && tabs.indexOf('orchestrator') !== -1 && BUILDERS.orchestrator) {
    activateTab('orchestrator');
    return;
  }
  if (wins.indexOf('orchestrator') !== -1) {
    log('фокус окна «Оркестратор»…');
    try {
      const r = await api('/api/windows/focus', jsonOpts('POST', { win: 'orchestrator' }));
      if (r && r.ok === false) log('✗ фокус «Оркестратор»: ' + (r.detail || 'бэкенд ответил ok:false'));
    } catch (e) {
      if (String(e.message).indexOf('404') !== -1) log('бэкенд без /api/windows/focus — окно «Оркестратор» уже открыто, поднимите его вручную');
      else log('✗ фокус «Оркестратор»: ' + e.message);
    }
    return;
  }
  log('открываю окно «Оркестратор»…');
  try { await api('/api/windows/restore', jsonOpts('POST', { win: 'orchestrator' })); }
  catch (e) { log('✗ окно «Оркестратор»: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/restore' : e.message)); }
  pollUi();
}

function buildOrchestrator(root) {
  panelRoots.orchestrator = root;
  root.innerHTML =
    '<div class="panel orch-panel">' +
      '<div class="toolbar">' + winControlsHtml() +
        '<span class="spacer"></span>' +
        '<span class="dim">rpc-канал оркестратора · подтверждения требуют ответа</span>' +
      '</div>' +
      '<div class="orch-chat scroll well grow" id="orch-chat">' +
        '<div class="orch-list" id="orch-list"></div>' +
        '<div class="orch-new hidden" id="orch-new" title="прокрутить вниз, к свежему">↓ новое</div>' +
      '</div>' +
      '<div class="orch-approvals hidden" id="orch-approvals"></div>' +
      '<div class="orch-input">' +
        '<textarea id="orch-in" class="orch-text" rows="2" placeholder="сообщение оркестратору… (Enter — отправить, Shift+Enter — перенос)" spellcheck="false" autocomplete="off"></textarea>' +
        '<button id="orch-stt" class="mini stt-btn" type="button" title="голосовой ввод: клик — начать, повторный клик — остановить и вставить">🎙</button>' +
        '<button id="orch-send" class="btn95" type="button" title="отправить (Enter)">отправить</button>' +
      '</div>' +
      '<div class="stt-hint dim hidden" id="stt-hint">пустое поле + удержание пробела — голосовой ввод</div>' +
    '</div>';
  orchScrollEl = root.querySelector('#orch-chat');
  orchListEl = root.querySelector('#orch-list');
  orchApprEl = root.querySelector('#orch-approvals');
  orchInputEl = root.querySelector('#orch-in');
  orchSendEl = root.querySelector('#orch-send');
  orchNewEl = root.querySelector('#orch-new');
  sttRegister(root.querySelector('#orch-stt'), sttOrchInsert);
  sttAttachPtt(orchInputEl, orchSend);
  orchInputEl.addEventListener('focus', sttShowHint);
  orchSendEl.addEventListener('click', orchSend);
  orchNewEl.addEventListener('click', () => {
    orchStick = true;
    orchNewEl.classList.add('hidden');
    if (orchScrollEl) orchScrollEl.scrollTop = orchScrollEl.scrollHeight;
  });
  orchScrollEl.addEventListener('scroll', () => {
    if (!orchScrollEl) return;
    orchStick = orchScrollEl.scrollTop + orchScrollEl.clientHeight >= orchScrollEl.scrollHeight - 12;
    if (orchStick && orchNewEl) orchNewEl.classList.add('hidden');
  });
  bindWinControls(root);
  chromeFillers.orchestrator = fillChromeOrch;
  fillChromeOrch();
  orchUpdateInput();
  /* история → статус → опрос. Сами ничего не отправляем. */
  orchLoadHistory().then(() => orchPoll());
  onTick('orchstatus', ORCH_IV, orchPoll);
  onTick('orch1s', 1000, orchTick);
}

export { buildOrchestrator, openOrchestratorPanel, refreshOrchestrator };
