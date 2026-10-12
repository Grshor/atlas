/* static/js/terminal.js — панель «Терминал»: xterm.js, мультиессия по WS.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { BUILDERS, WIN, activateTab, activeTab, ch, chromeFillers, fillChrome, panelRoots, pollUi, setChrome, setUiCache, state, uiCache } from './core.js';
import { API, api, b64decode, b64encode, encUTF8, jsonOpts, wsBase } from './lib/api.js';
import { confirmDialog } from './lib/modal.js';
import { copyText, el, esc, log } from './lib/dom.js';
import { onTick } from './lib/ticker.js';
import { sttRegister } from './stt.js';

/* =====================================================================
   панель «Терминал» (?win=terminal): xterm.js 5.5.0, МУЛЬТИСЕССИЯ.
   GET /api/terminal/sessions → {"sessions":[{id,label,cmd,alive,pid}]} —
   только проектные сессии proj-*: оркестратор переехал в панель
   «Оркестратор» (один мозг). Список рисуем как есть, но сессию orch из него
   выкидываем: старый бэкенд всё ещё её отдаёт, а он 404-ит любую orch-адресацию —
   переключатель не должен её предлагать.
   WS /api/terminal/ws/{session_id} — те же кадры JSON+base64 (первый — resize);
   POST /api/terminal/{id}/kill (только proj-*), POST /api/terminal/{id}/restart
   и POST /api/terminal/{id}/open — открыть сессию в настоящем wezterm
   (вкладка в запущенном GUI или новое окно); встроенный xterm остаётся
   просмотром по умолчанию.
   Клавиатура: xterm.js 5.5 kitty-протокола не знает — слой ниже (kittyScan/
   kittyKeySeq) отслеживает push/pop/query `CSI … u` в выводе ребёнка и сам
   кодирует функциональные клавиши с модификаторами (Shift+Enter = CSI 13;2u),
   когда приложение включило флаг 1; остальное (DECCKM, bracketed paste,
   backspace \x7f) делает сам xterm.js.
   Пустой список — подсказка про кнопку W + «открыть herdr в wezterm», без
   падения и без «＋».
   =================================================================== */
const TERM_SCROLLBACK = 5000;
const TERM_REQ = 'atlas.terminal.request'; /* мост из других окон: id сессии */
const TERM_ORCH_LEGACY = 'orch'; /* старая сессия-оркестратор: не предлагаем вовсе */
/* Пер-сессионные объекты: {id, host, termEl, ov, xterm, fit, ro, ws, state,
   exitCode, everOpened}. Переключение чипа прячет один .term-host и показывает
   другой: xterm скрытой сессии продолжает принимать записи (буфер живёт), WS
   каждой сессии остаётся открытым, пока сессия жива (у сервера один зритель
   на сессию — N открытых сессий = N сокетов). Погибший WS (рестарт сервера)
   лениво переподключается при следующей активации сессии. term.reset() больше
   нет вовсе: буферы никогда не очищаются при переключении. */
const termObjs = new Map(); /* id → объект сессии */
let termSessions = [];      /* [{id,label,cmd,alive,pid}] */
let termSessionsOk = null;  /* null — неизвестно; true — список есть; false — 404 (старый бэкенд) */
let termActive = null;      /* id активной сессии */

function termWsUrl(id) {
  if (!id) return null;
  return wsBase() + '/api/terminal/ws/' + encodeURIComponent(id);
}
function termObj() { return termActive ? (termObjs.get(termActive) || null) : null; }
function termXterm() { const o = termObj(); return o ? o.xterm : null; }
function termPalette() {
  const cs = getComputedStyle(document.documentElement);
  const g = (k, fb) => (cs.getPropertyValue('--palette-' + k) || '').trim() || fb;
  return {
    background: g('darker-background', '#020c17'),
    foreground: g('foreground', '#f6dcac'),
    cursor: g('accent', '#faa968'),
    cursorAccent: g('darker-background', '#020c17'),
    selectionBackground: g('selection', '#134e5a'),
    black: g('darker-background', '#020c17'),
    brightBlack: g('muted', '#2a6b78'),
    white: g('light-foreground', '#a7c9c6'),
    brightWhite: g('bright-foreground', '#f6dcac'),
    red: g('red', '#f85525'),
    brightRed: g('red', '#f85525'),
    green: g('green', '#028391'),
    brightGreen: g('green', '#028391'),
    yellow: g('yellow', '#e97b3c'),
    brightYellow: g('yellow', '#e97b3c'),
    blue: g('blue', '#3f8f8a'),
    brightBlue: g('blue', '#3f8f8a'),
    magenta: g('muted', '#2a6b78'),
    brightMagenta: g('cyan', '#8cbfb8'),
    cyan: g('cyan', '#8cbfb8'),
    brightCyan: g('cyan', '#8cbfb8'),
  };
}
function termApplyTheme() {
  const theme = termPalette();
  for (const o of termObjs.values()) { try { o.xterm.options.theme = theme; } catch (_) {} }
}
function termCurrent() { return termSessions.find((s) => s.id === termActive) || null; }
function termFillChrome() {
  if (!ch.t || !ch.t.info) return;
  if (termSessionsOk === false) { ch.t.info.textContent = 'список сессий недоступен (бэкенд без /api/terminal/sessions)'; return; }
  const s = termCurrent();
  const o = termObj();
  const st = o ? { connecting: 'подключение…', open: 'открыта', exited: 'завершилась', busy: 'открыта в другом окне', error: 'нет связи' }[o.state] : null;
  if (s) ch.t.info.textContent = s.label + ' · ' + (s.pid != null ? 'PID ' + s.pid : 'PID —') + ' · ' + (s.alive ? 'жива' : 'мертва') + (st ? ' · ' + st : '');
  else ch.t.info.textContent = 'нет сессий';
}

/* список сессий: только proj-* и herdr (orch — не сессия терминала; выкидываем
   даже если старый бэкенд её ещё отдаёт); 404 — деградация с подсказкой, без WS.
   После списка — сверка объектов: исчезнувшие сессии (kill, рестарт сервера)
   освобождают свои xterm/WS/DOM; пропавший активный id замещается первой
   живой сессией. */
async function loadSessions() {
  try {
    const d = await api('/api/terminal/sessions');
    const list = Array.isArray(d && d.sessions) ? d.sessions : [];
    termSessions = list.filter((s) => s && s.id && String(s.id) !== TERM_ORCH_LEGACY).map((s) => ({
      id: String(s.id),
      label: s.label != null ? String(s.label) : String(s.id),
      cmd: Array.isArray(s.cmd) ? s.cmd : [],
      alive: !!s.alive,
      pid: s.pid != null ? s.pid : null,
    }));
    termSessionsOk = true;
  } catch (e) {
    if (String(e.message).indexOf('404') !== -1) { termSessionsOk = false; termSessions = []; }
    /* прочие ошибки — держим прежний список, не падаем */
  }
  if (termSessionsOk) {
    const ids = termSessions.map((s) => s.id);
    for (const id of Array.from(termObjs.keys())) if (!ids.includes(id)) termDispose(id);
    if (!ids.includes(termActive)) {
      termActive = ids[0] || null;
      /* видимый хост мог принадлежать исчезнувшей сессии — показать новую */
      if (panelRoots.terminal) termShowActive();
    }
  }
  renderTermStrip();
  termFillChrome();
}

/* чип сессии: клик — переключение (буфер целевой сессии НЕ трогается) */
function renderTermStrip() {
  const box = panelRoots.terminal && panelRoots.terminal.querySelector('#term-strip');
  if (!box) return;
  box.innerHTML = '';
  if (termSessionsOk !== true) {
    box.appendChild(el('span', 'term-hint', 'список сессий недоступен (бэкенд без /api/terminal/sessions)'));
    appendWzControls(box);
    return;
  }
  /* пустой список — честная подсказка + то же действие «открыть в wezterm»
     на herdr-клиенте (терминал живёт в wezterm, панель — пульт) */
  if (!termSessions.length) {
    box.appendChild(el('span', 'term-hint', 'сессии proj-* появляются при фокусе проекта кнопкой W'));
    const wz = el('button', 'term-add', '↗ herdr в wezterm');
    wz.type = 'button';
    wz.title = 'открыть herdr-клиент в wezterm: вкладка в запущенном GUI или новое окно';
    wz.addEventListener('click', () => termOpenWezterm('herdr'));
    box.appendChild(wz);
    appendWzControls(box);
    return;
  }
  for (const s of termSessions) {
    const isActive = s.id === termActive;
    const chip = el('span', 'term-chip ' + (s.alive ? 'alive' : 'dead') + (isActive ? ' active' : ''));
    chip.appendChild(el('span', 'tdot', '●'));
    chip.appendChild(el('span', 'tlabel', esc(s.label)));
    chip.title = s.label + ' · ' + (s.alive ? 'жива' : 'мертва') +
      (s.cmd && s.cmd.length ? ' · ' + s.cmd.join(' ') : '') +
      '\nклик — переключиться: буфер этой сессии сохраняется';
    chip.addEventListener('click', () => selectTermSession(s.id));
    const wz = el('span', 'tx', '↗');
    wz.title = 'открыть в wezterm: вкладка в запущенном GUI или новое окно';
    wz.addEventListener('click', (e) => { e.stopPropagation(); termOpenWezterm(s.id); });
    chip.appendChild(wz);
    if (s.id.indexOf('proj-') === 0) {
      const x = el('span', 'tx', '✕');
      x.title = 'остановить сессию «' + s.label + '»';
      x.addEventListener('click', (e) => { e.stopPropagation(); termKill(s.id); });
      chip.appendChild(x);
    }
    box.appendChild(chip);
  }
  const add = el('button', 'term-add', '＋');
  add.type = 'button';
  add.title = 'сессия проекта появится, когда сфокусировать проект кнопкой W (терминал atlas)';
  add.addEventListener('click', () => log('сессия проекта появится, когда сфокусировать проект кнопкой W (терминал atlas)'));
  box.appendChild(add);
  box.appendChild(el('span', 'term-hint', 'сессии proj-* — при фокусе проекта кнопкой W'));
  appendWzControls(box);
}

/* ---------- жизненный цикл объекта сессии ---------- */
function termEnsure(id) {
  let o = termObjs.get(id);
  if (o) return o;
  const wrap = panelRoots.terminal && panelRoots.terminal.querySelector('.term-wrap');
  if (!wrap || !window.Terminal) return null;
  /* хост создаётся СРАЗУ видимым: termShowActive вызывает нас только перед
     показом, а xterm.open() в display:none не измерит canvas;
     чужие хосты прячет следом сам termShowActive */
  const host = el('div', 'term-host');
  host.dataset.sid = id;
  const termEl = el('div', 'term-term');
  const ov = el('div', 'term-overlay hidden');
  host.append(termEl, ov);
  wrap.appendChild(host);
  const xterm = new window.Terminal({
    scrollback: TERM_SCROLLBACK,
    fontFamily: 'ui-monospace, "JetBrains Mono NL", "JetBrains Mono", "Cascadia Mono", Menlo, Consolas, "DejaVu Sans Mono", monospace',
    fontSize: 12.5,
    cursorBlink: true,
    theme: termPalette(),
  });
  let fit = null;
  if (window.FitAddon && window.FitAddon.FitAddon) {
    fit = new window.FitAddon.FitAddon();
    xterm.loadAddon(fit);
  }
  xterm.open(termEl);
  /* dec — ЕДИНЫЙ декодер на сессию: WS-кадры рвут байтовый поток где попало
     (бэкенд читает PTY кусками по 8 КиБ), мультибайтовый UTF-8 символ может
     закончиться в следующем кадре; новый TextDecoder на кадр такой хвост
     превращает в U+FFFD. dec.decode() по умолчанию «stream»: хвост
     расщеплённого символа держится до следующего вызова. */
  o = { id, host, termEl, ov, xterm, fit, ro: null, ws: null, state: 'init', exitCode: null, everOpened: false,
    dec: new TextDecoder('utf-8'), kpStack: [], kpTail: '' };
  xterm.attachCustomKeyEventHandler(termKeyHandler);
  xterm.onData((data) => termSendObj(o, { type: 'in', data: b64encode(encUTF8(data)) }));
  if (window.ResizeObserver) {
    o.ro = new ResizeObserver(() => termFitObj(o, true));
    o.ro.observe(host);
  }
  termObjs.set(id, o);
  return o;
}

/* kill/рестарт/исчезновение из списка: xterm, WS, observer и DOM освобождаются */
function termDispose(id) {
  const o = termObjs.get(id);
  if (!o) return;
  if (o.ws) { try { o.ws.onclose = null; o.ws.onmessage = null; o.ws.close(); } catch (_) {} }
  if (o.ro) { try { o.ro.disconnect(); } catch (_) {} }
  try { o.xterm.dispose(); } catch (_) {}
  if (o.host.parentNode) o.host.parentNode.removeChild(o.host);
  termObjs.delete(id);
}

function termHiddenNode(n) { return !n || n.offsetParent === null; }
function termFitObj(o, sendResize) {
  if (!o || !o.fit || termHiddenNode(o.host)) return;
  try { o.fit.fit(); } catch (_) {}
  if (sendResize) termSendResizeObj(o);
}
function termSendObj(o, frame) {
  if (o && o.ws && o.ws.readyState === 1) { try { o.ws.send(JSON.stringify(frame)); } catch (_) {} }
}
function termSendResizeObj(o) {
  if (!o || !o.xterm) return;
  termSendObj(o, { type: 'resize', cols: o.xterm.cols, rows: o.xterm.rows });
}

/* WS одной сессии: открывается при первой активации, держится открытым, пока
   сессия жива. Кадры адресуют СВОЙ объект — скрытые сессии продолжают писать
   в свои буферы, друг друга не затрагивая. */
function termConnectObj(o, force) {
  if (!o) return;
  if (o.ws && (o.ws.readyState === 0 || o.ws.readyState === 1) && !force) return;
  if (o.ws) { try { o.ws.onclose = null; o.ws.onmessage = null; o.ws.close(); } catch (_) {} o.ws = null; }
  const url = termWsUrl(o.id);
  if (!url) return;
  o.state = 'connecting';
  o.exitCode = null;
  /* свежая сессия = новый ребёнок: kitty-флаги и недокодированный хвост из
     прошлой жизни не переносятся */
  o.dec = new TextDecoder('utf-8');
  o.kpStack = [];
  o.kpTail = '';
  termOverlayObj(o);
  let ws;
  try { ws = new WebSocket(url); }
  catch (_) { o.state = 'error'; termOverlayObj(o); return; }
  o.ws = ws;
  ws.onopen = () => {
    o.state = 'open';
    o.everOpened = true;
    termOverlayObj(o);
    /* первый кадр от клиента — resize (требование протокола) */
    termFitObj(o, true);
  };
  ws.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch (_) { return; }
    if (m.type === 'out') {
      try { o.xterm.write(kittyScan(o, o.dec.decode(b64decode(m.data || '')))); } catch (_) {}
    } else if (m.type === 'exit') {
      /* сессия кончилась — ничего не придёт, выталкиваем хвост на экран */
      if (o.kpTail) { try { o.xterm.write(o.kpTail); } catch (_) {} o.kpTail = ''; }
      o.exitCode = m.code;
      o.state = 'exited';
      termOverlayObj(o);
    } else if (m.type === 'busy') {
      o.state = 'busy';
      termOverlayObj(o);
    }
  };
  ws.onclose = () => {
    o.ws = null;
    if (o.state === 'busy' || o.state === 'exited') { termOverlayObj(o); termFillChrome(); return; }
    /* сокет погиб (обычно рестарт сервера) — оверлей предлагает переподключить;
       само подключение произойдёт лениво при следующей активации сессии */
    o.state = 'error';
    termOverlayObj(o);
    termFillChrome();
  };
}

function termOverlayObj(o) {
  if (!o || !o.ov) return;
  let html = null;
  let show = true;
  if (o.state === 'exited') html = 'сессия завершена (code ' + (o.exitCode == null ? '?' : o.exitCode) + ') <button class="btn95" type="button">⟳ перезапустить</button>';
  else if (o.state === 'busy') html = 'терминал открыт в другом окне';
  else if (o.state === 'error') html = 'подключение потеряно <button class="btn95" type="button">⟳ переподключить</button>';
  else show = false;
  o.ov.classList.toggle('hidden', !show);
  if (html != null) o.ov.innerHTML = html;
  const btn = o.ov.querySelector('.btn95');
  if (btn) btn.addEventListener('click', () => termRestart(o.id));
  termFillChrome();
}

/* показать активную сессию: спрятать чужие хосты, вернуть свой, подогнать
   размер и лениво переподключить погибший/ещё не открытый WS */
function termShowActive() {
  const wrap = panelRoots.terminal && panelRoots.terminal.querySelector('.term-wrap');
  if (!wrap) return;
  const nosess = wrap.querySelector('#term-overlay');
  if (!termActive) {
    if (nosess && termSessionsOk === true && !termSessions.length) {
      nosess.textContent = 'нет сессий — сфокусируйте проект кнопкой W (терминал atlas)';
      nosess.classList.remove('hidden');
    }
    return;
  }
  if (nosess) nosess.classList.add('hidden');
  const o = termEnsure(termActive);
  if (!o) return;
  for (const n of wrap.querySelectorAll('.term-host')) n.classList.toggle('hidden', n !== o.host);
  termFitObj(o, true);
  const s = termSessions.find((x) => x.id === o.id);
  if (s && !s.alive && !o.everOpened && !o.ws) {
    o.state = 'exited';
    termOverlayObj(o);
  } else {
    if (!o.ws && (!s || s.alive)) termConnectObj(o, false);
    termOverlayObj(o);
  }
  try { o.xterm.focus(); } catch (_) {}
}

/* переключение сессии: показать целевой хост; буферы и открытые WS не трогаются */
function selectTermSession(id) {
  if (!id || !panelRoots.terminal) return;
  termActive = id;
  termShowActive();
  renderTermStrip();
  termFillChrome();
}

/* мост из других окон: atlas.terminal.request (id сессии) */
function applyTermRequest() {
  if (!panelRoots.terminal) return;
  let s = null;
  try { s = localStorage.getItem(TERM_REQ); } catch (_) {}
  if (!s) return;
  /* список мог быть получен до появления сессии (её создал фокус W) — обновим */
  if (termSessionsOk === true && !termSessions.some((x) => x.id === s)) {
    loadSessions().then(() => { if (termSessions.some((x) => x.id === s)) applyTermRequest(); });
    return;
  }
  if (s !== termActive) selectTermSession(s);
}
window.addEventListener('storage', (e) => { if (!e.key || e.key === TERM_REQ) applyTermRequest(); });

/* открыть панель «Терминал» на сессии — строгая последовательность с
   single-flight замком (двойной клик W даёт ОДИН restore):
   1) свежий GET /api/ui;
   2) панель в ui.tabs → активировать вкладку хоста;
   3) панель в ui.windows → отдельное окно уже есть, ничего не создаём;
   4) иначе POST /api/windows/restore; ok:false → одна повторная попытка
      через 1 с, затем честный отказ в журнале;
   5) мост session-request + чипы из свежего GET /api/terminal/sessions. */
let termOpenInflight = null;
async function termRestoreOnce() {
  try {
    const r = await api('/api/windows/restore', jsonOpts('POST', { win: 'terminal' }));
    if (r && r.ok) return true;
    log('✗ окно «Терминал»: ' + ((r && r.detail) || 'бэкенд ответил ok:false'));
    return false;
  } catch (e) {
    log('✗ окно «Терминал»: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без /api/windows/restore' : e.message));
    return false;
  }
}
async function openTerminalPanel(session) {
  if (termOpenInflight) return termOpenInflight;
  termOpenInflight = (async () => {
    try {
      if (session) { try { localStorage.setItem(TERM_REQ, String(session)); } catch (_) {} }
      let ui = null;
      try { ui = await api('/api/ui'); setUiCache(ui); } catch (_) { /* нет /api/ui — решаем по прежнему кэшу */ }
      const tabs = ui && Array.isArray(ui.tabs) ? ui.tabs : (uiCache && Array.isArray(uiCache.tabs) ? uiCache.tabs : []);
      const wins = ui && Array.isArray(ui.windows) ? ui.windows : (uiCache && Array.isArray(uiCache.windows) ? uiCache.windows : []);
      if (WIN === 'projects' && tabs.indexOf('terminal') !== -1 && BUILDERS.terminal) {
        activateTab('terminal');
      } else if (wins.indexOf('terminal') === -1) {
        let ok = await termRestoreOnce();
        if (!ok) {
          await new Promise((res) => setTimeout(res, 1000)); /* окно могло догонять — одна повторная попытка */
          ok = await termRestoreOnce();
        }
        if (!ok) { log('✗ не удалось открыть окно «Терминал» — см. журнал выше'); return; }
        pollUi();
      } /* else: отдельное окно уже открыто — создавать нечего */
      if (panelRoots.terminal) { await loadSessions(); applyTermRequest(); }
    } finally { termOpenInflight = null; }
  })();
  return termOpenInflight;
}

/* ↗ (чип сессии / пустой список): открыть сессию в ВЫДЕЛЕННОМ окне wezterm,
   принадлежащем atlas (не в личных окнах пользователя) — вкладка в живом
   atlas-экземпляре или запуск самого экземпляра, решает бэкенд. Встроенный
   xterm остаётся просмотром по умолчанию; текст ошибки бэкенда идёт в журнал
   как есть. */
async function termOpenWezterm(id) {
  const s = termSessions.find((x) => x.id === id);
  const lbl = s ? s.label : id;
  log('терминал: открываю «' + lbl + '» в wezterm…');
  try {
    /* fetch вместо api(): текст ошибки приходит в body {detail}, а api()
       оставляет от неё только «HTTP 400» */
    const r = await fetch(API + '/api/terminal/' + encodeURIComponent(id) + '/open', jsonOpts('POST', {}));
    const j = await r.json().catch(() => null);
    if (r.ok && j && j.ok) {
      log('✓ wezterm: «' + lbl + '» открыт ' + (j.mode === 'window' ? 'в новом окне' : 'вкладкой'));
    } else log('✗ wezterm: ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ wezterm: ' + e.message); }
}

async function termKill(id) {
  const s = termSessions.find((x) => x.id === id);
  const lbl = s ? s.label : id;
  if (!(await confirmDialog('остановить сессию «' + lbl + '»? процесс будет убит'))) { log('остановка отменена'); return; }
  log('терминал: остановка «' + lbl + '»…');
  try {
    await api('/api/terminal/' + encodeURIComponent(id) + '/kill', jsonOpts('POST', {}));
    log('✓ сессия остановлена: ' + lbl);
  } catch (e) { log('✗ остановить не удалось: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без kill' : e.message)); }
  await loadSessions(); /* сверка освободит объект и DOM исчезнувшей сессии */
}
/* ⟳ (хром/оверлей): перезапуск сессии (kill + поднять заново).
   Рестарт = свежая сессия: объект освобождается и создаётся лениво заново —
   чистый буфер вместо term.reset(). */
async function termRestart(id) {
  const target = id || termActive;
  if (!target) { log('терминал: нет активной сессии'); return; }
  const s = termSessions.find((x) => x.id === target);
  const lbl = s ? s.label : target;
  log('терминал: перезапуск «' + lbl + '»…');
  try {
    await api('/api/terminal/' + encodeURIComponent(target) + '/restart', jsonOpts('POST', {}));
  } catch (e) { log('✗ перезапуск: ' + (String(e.message).indexOf('404') !== -1 ? 'бэкенд без restart' : e.message)); }
  await loadSessions();
  if (termSessions.some((x) => x.id === target)) {
    termDispose(target);
    if (termActive === target) termShowActive();
  }
  renderTermStrip();
  termFillChrome();
}
function termHint(msg) { log('буфер обмена: ' + msg); }

/* ---------- док: atlas-wezterm как окно-панель (WEZTERM-EMBED, этап A) ----------

   «Док / Спрятать-Показать / Фокус» управляют ВЫДЕЛЕННЫМ окном wezterm
   (класс atlas-wezterm, создаётся «↗») через hyprctl на сервере: прямоугольник
   из пресета, special:atlas-dock для «спрятать», фокус + z-order. Кнопки
   живут в полосе сессий рядом с «↗»; текст ошибки бэкенда идёт в журнал
   как есть (тот же raw fetch + {detail}, что у termOpenWezterm). */

/* пресеты дока: клик по «Док» применяет показанный пресет и переключает на
   следующий (половина → четверть → поверх) */
const WZ_PRESETS = [['right-half', 'половина'], ['right-quarter', 'четверть'], ['overlay', 'поверх']];
let wzPresetIdx = 0;

/* fetch вместо api(): текст ошибки приходит в body {detail}, а api()
   оставляет от неё только «HTTP 4xx» */
function wzFetch(path, body) {
  return fetch(API + path, body != null ? jsonOpts('POST', body) : {});
}

async function termWeztermDock() {
  const [preset, label] = WZ_PRESETS[wzPresetIdx];
  log('терминал: док wezterm — ' + label + '…');
  try {
    const r = await wzFetch('/api/terminal/wezterm/dock', { preset });
    const j = await r.json().catch(() => null);
    if (r.ok && j && j.ok) {
      const rc = j.rect || {};
      log('✓ док: ' + (j.preset || label) + ' → ' + rc.x + ',' + rc.y + ' ' + rc.w + '×' + rc.h);
    } else log('✗ док: ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ док: ' + e.message); }
  wzPresetIdx = (wzPresetIdx + 1) % WZ_PRESETS.length;
  renderTermStrip(); /* кнопка показывает следующий пресет */
}

async function termWeztermToggleHidden() {
  try {
    const sr = await wzFetch('/api/terminal/wezterm/state');
    const s = await sr.json().catch(() => null);
    if (!sr.ok) { log('✗ спрятать/показать: HTTP ' + sr.status); return; }
    const hidden = !!(s && s.hidden);
    log('терминал: ' + (hidden ? 'показываю' : 'прячу') + ' wezterm…');
    const r = await wzFetch('/api/terminal/wezterm/' + (hidden ? 'show' : 'hide'), {});
    const j = await r.json().catch(() => null);
    if (r.ok && j && j.ok) log('✓ ' + (hidden ? 'показан' : 'спрятан (special:atlas-dock)'));
    else log('✗ ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ спрятать/показать: ' + e.message); }
}

async function termWeztermFocus() {
  log('терминал: фокус wezterm…');
  try {
    const r = await wzFetch('/api/terminal/wezterm/focus', {});
    const j = await r.json().catch(() => null);
    if (r.ok && j && j.ok) log('✓ фокус: окно atlas-wezterm поднято');
    else log('✗ фокус: ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ фокус: ' + e.message); }
}

/* ---------- хвост: get-text снапшот atlas-wezterm в панели (WEZTERM-EMBED §A4) ----------

   Кнопка «Хвост» разворачивает read-only <pre> с последними строками панели
   atlas-wezterm (GET /api/terminal/wezterm/tail). Опрос раз в 2 с живёт как
   остальные тикеры: один общий onTick, работа — только пока хвост открыт и
   панель видима (переключение вкладки/чипа и закрытие панели опрос
   останавливают, возврат возобновляет). Текст идёт строго в textContent —
   терминальный вывод никогда не попадает в innerHTML. Ввод не трогается:
   печатает только окно wezterm. */

let wzTailOpen = false;

function wzTailToggle() {
  wzTailOpen = !wzTailOpen;
  const box = panelRoots.terminal && panelRoots.terminal.querySelector('#wz-tail');
  if (box) box.classList.toggle('hidden', !wzTailOpen);
  if (wzTailOpen) wzTailFetch(); /* первый кадр не ждёт тика */
}

function wzTailTick() {
  if (!wzTailOpen || !panelRoots.terminal) return;
  if (!(WIN === 'terminal' || (WIN === 'projects' && activeTab === 'terminal'))) return;
  wzTailFetch();
}

async function wzTailFetch() {
  const root = panelRoots.terminal;
  const head = root && root.querySelector('#wz-tail-head');
  const pre = root && root.querySelector('#wz-tail-pre');
  if (!pre) return;
  try {
    const r = await wzFetch('/api/terminal/wezterm/tail?lines=40');
    const j = await r.json().catch(() => null);
    if (r.ok && j && j.ok) {
      if (head) {
        head.textContent = 'хвост atlas-wezterm · панель ' + j.pane + ' · ' +
          new Date().toLocaleTimeString() + ' · строк: ' + j.lines;
      }
      pre.textContent = String(j.text == null ? '' : j.text);
    } else log('✗ хвост: ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ хвост: ' + e.message); }
}

/* кнопки дока — в конец полосы сессий, при любом состоянии списка:
   окну wezterm можно управлять и без активных чипов */
function appendWzControls(box) {
  const dock = el('button', 'mini', 'Док: ' + WZ_PRESETS[wzPresetIdx][1]);
  dock.type = 'button';
  dock.title = 'прижать окно atlas-wezterm к вычисленному прямоугольнику; каждый клик — следующий пресет: половина → четверть → поверх';
  dock.addEventListener('click', termWeztermDock);
  box.appendChild(dock);
  const hide = el('button', 'mini', 'Спрятать/Показать');
  hide.type = 'button';
  hide.title = 'убрать окно atlas-wezterm в special:atlas-dock и вернуть (по состоянию окна)';
  hide.addEventListener('click', termWeztermToggleHidden);
  box.appendChild(hide);
  const f = el('button', 'mini', 'Фокус');
  f.type = 'button';
  f.title = 'поднять окно atlas-wezterm наверх и сфокусировать';
  f.addEventListener('click', termWeztermFocus);
  box.appendChild(f);
  const tail = el('button', 'mini', 'Хвост');
  tail.type = 'button';
  tail.title = 'хвост вывода atlas-wezterm в панели: снапшот get-text раз в 2 с; только чтение — ввод остаётся в окне wezterm';
  tail.addEventListener('click', wzTailToggle);
  box.appendChild(tail);
}
function termCopy() {
  const t = termXterm();
  if (!t) return;
  const sel = t.getSelection();
  if (!sel) { termHint('нечего копировать — выделите текст'); return; }
  copyText(sel, 'выделенный текст', 'буфер обмена недоступен в этом окне');
}
/* вставка ФАЙЛА из файл-менеджера: в WebKitGTK надёжный аромат буфера —
   `text/uri-list` (file://-URI, возможно несколько, percent-экранированные).
   Разбор чистый и тестируемый: '#' — комментарий, каждая строка — один URI;
   file:// срезается, %-экраны декодируются ПО СЕГМЕНТАМ пути ('+' в путях —
   плюс, а не пробел: decodeURIComponent, не query-правила); не-file строки
   остаются как есть; битый % — сегмент без декодирования. */
function uriListToPaths(s) {
  const out = [];
  for (const raw of String(s).split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    if (!/^file:\/\//i.test(line)) { out.push(line); continue; }
    const tail = line.slice('file://'.length);
    out.push(tail.split('/').map((seg) => {
      try { return decodeURIComponent(seg); } catch (_) { return seg; }
    }).join('/'));
  }
  return out;
}
/* пути → текст для сессии: через пробел, в кавычках только путь с пробелами
   или кавычками (внутренние " экранируются, это POSIX-вставка в шелл) */
function termPathsText(paths) {
  return paths.map((p) => (/[\s"]/.test(p) ? '"' + p.replace(/"/g, '\\"') + '"' : p)).join(' ');
}
/* пути файлов в активную сессию — тот же путь вставки, что у картинки */
function pasteFilePaths(paths) {
  if (!paths.length) return false;
  const ok = termInsertActive(termPathsText(paths) + ' ');
  log((ok ? '✓ вставлено: ' : '✗ не вставлено: ') + paths.length + ' путь(ей): ' + termPathsText(paths));
  return ok;
}
function termPaste() {
  const o = termObj();
  const send = (text) => { if (text) termSendObj(o, { type: 'in', data: b64encode(encUTF8(text)) }); };
  const sendText = () => {
    if (navigator.clipboard && navigator.clipboard.readText) {
      navigator.clipboard.readText().then(send).catch(() => termHint('вставка заблокирована — используйте среднюю кнопку мыши'));
    } else termHint('вставка недоступна в этом окне');
  };
  try {
    /* файлы/картинка в буфере → путь(и) в сессию; текста нет — текст как был */
    if (navigator.clipboard && navigator.clipboard.read) {
      navigator.clipboard.read().then(async (items) => {
        for (const it of items || []) {
          const types = it.types || [];
          if (types.indexOf('text/uri-list') !== -1) {
            try {
              const blob = await it.getType('text/uri-list');
              const paths = uriListToPaths(await blob.text());
              if (paths.length) { pasteFilePaths(paths); return; }
            } catch (_) { /*(uri-list не отдался — дальше по ароматам)*/ }
          }
          const imgType = types.find((t) => String(t).startsWith('image/'));
          if (imgType) {
            const blob = await it.getType(imgType);
            await pasteImageFile(blob, imgType);
            return;
          }
        }
        sendText();
      }).catch(sendText);
      return;
    }
    sendText();
  } catch (_) { termHint('вставка недоступна в этом окне'); }
}
/* изображение при вставке в терминал → файл в ~/.local/share/atlas/paste,
   в сессию идёт ПУТЬ (терминалу картинка не нужна, агент откроет файл) */
async function pasteImageFile(blob, type) {
  try {
    const r = await fetch('/api/clipboard/image', {
      method: 'POST',
      headers: { 'content-type': type || 'application/octet-stream' },
      body: blob,
    });
    const j = await r.json();
    if (j && j.ok && j.path) {
      termInsertActive(j.path + ' ');
      log('✓ изображение сохранено: ' + j.path);
    } else log('✗ изображение не сохранено: ' + ((j && j.detail) || ('HTTP ' + r.status)));
  } catch (e) { log('✗ изображение не сохранено: ' + e.message); }
}
/* native paste (Ctrl+V) в панели: ФАЙЛЫ (uri-list/files → пути в сессию),
   картинка (→ файл + путь), обычный текст — мимо, его вставляет сам xterm */
function handleTermPaste(e) {
  const dt = e.clipboardData;
  if (!dt) return;
  /* скопированные в файл-менеджере файлы: WebKitGTK кладёт text/uri-list
     (file://-URI); dt.files — запасной аромат (реальный путь не всегда есть) */
  let uris = '';
  try { uris = dt.getData ? dt.getData('text/uri-list') : ''; } catch (_) { /* аромат не отдался */ }
  const paths = uriListToPaths(uris || '');
  if (paths.length) { e.preventDefault(); pasteFilePaths(paths); return; }
  if (dt.files && dt.files.length) {
    const fps = [];
    for (const f of dt.files) { if (f && f.path) fps.push(String(f.path)); }
    if (fps.length) { e.preventDefault(); pasteFilePaths(fps); return; }
  }
  if (!dt.items) return;
  for (const it of dt.items) {
    if (it.kind === 'file' && it.type && it.type.startsWith('image/')) {
      const f = it.getAsFile();
      if (!f) continue;
      e.preventDefault();
      pasteImageFile(f, it.type);
      return;
    }
  }
}
/* ---------- kitty keyboard protocol, уровень 1 («disambiguate») ----------

   xterm.js 5.5 kitty-протокол не умеет вовсе. Приложения его включают сами:
   `CSI > flags u` (push), `CSI < n u` (pop), опрашивают `CSI ? u`. Когда флаг
   активен, ФУНКЦИОНАЛЬНЫЕ клавиши с модификаторами обязаны приходить
   CSI u-кодами из таблицы спецификации (Shift+Enter = `CSI 13;2u`,
   Ctrl+Enter = `CSI 13;5u`, Shift+Tab = `CSI 9;2u` и т.д.) — иначе шелл-приложение
   (nvim, fish, claude) не отличит Shift+Enter от Enter. Немодифицированные
   клавиши и DECCKM остаются у xterm.js — режимами ребёнка мы не управляем.
   Поддержан только бит 1; старшие биты (event types, alternate keys) не
   ломаются: их эффект мы просто не эмулируем, кодируем как для флага 1. */

/* активный флаг kitty (0 = протокол выключен приложением) */
function kittyFlags(o) {
  return o.kpStack && o.kpStack.length ? (o.kpStack[o.kpStack.length - 1] & 1) : 0;
}

/* ответ на запрос `CSI ? u` — какие флаги активны сейчас (0 без push'ей) */
function kittyReply(o, text) {
  termSendObj(o, { type: 'in', data: b64encode(encUTF8(text)) });
}

/* разбор потока вывода: услуги push/pop/query снимаются ДО отрисовки;
   `CSI ... u` может быть разрезан границей WS-кадра — возможное начало
   последовательности (ESC[…) держится до следующего кадра. Возвращает
   строку, которую можно писать в xterm. */
function kittyScan(o, s) {
  if (!o.kpStack) o.kpStack = [];
  const buf = o.kpTail + s;
  o.kpTail = '';
  const re = /\x1b\[([<>?])(\d*)u/g;
  let m, end = 0;
  while ((m = re.exec(buf))) {
    end = re.lastIndex;
    if (m[1] === '>') o.kpStack.push(parseInt(m[2] || '1', 10) || 1);
    else if (m[1] === '<') {
      const n = parseInt(m[2] || '1', 10) || 1;
      for (let i = 0; i < n && o.kpStack.length; i++) o.kpStack.pop();
    } else kittyReply(o, '\x1b[?' + kittyFlags(o) + 'u');
  }
  const tail = /\x1b(?:\[[<>?\d;]*)?$/.exec(buf.slice(end));
  if (tail && tail[0]) {
    o.kpTail = tail[0];
    return buf.slice(0, buf.length - tail[0].length);
  }
  return buf;
}

/* CSI u-код функциональной клавиши с модификаторами по таблице kitty
   (модификаторы = 1 + shift | alt<<1 | ctrl<<2); null = кодировать нечего —
   пусть клавишу кодирует xterm.js по-своему (legacy, DECCKM и пр.) */
function kittyKeySeq(e) {
  if (e.metaKey) return null;
  const mods = 1 + (e.shiftKey ? 1 : 0) + (e.altKey ? 2 : 0) + (e.ctrlKey ? 4 : 0);
  if (mods === 1) return null;
  switch (e.code) {
    case 'Enter': case 'NumpadEnter': return '\x1b[13;' + mods + 'u';
    case 'Tab': return '\x1b[9;' + mods + 'u';
    case 'Escape': return '\x1b[27;' + mods + 'u';
    case 'Backspace': return '\x1b[127;' + mods + 'u';
    case 'ArrowUp': return '\x1b[1;' + mods + 'A';
    case 'ArrowDown': return '\x1b[1;' + mods + 'B';
    case 'ArrowRight': return '\x1b[1;' + mods + 'C';
    case 'ArrowLeft': return '\x1b[1;' + mods + 'D';
    case 'Home': return '\x1b[1;' + mods + 'H';
    case 'End': return '\x1b[1;' + mods + 'F';
    case 'Insert': return '\x1b[2;' + mods + '~';
    case 'Delete': return '\x1b[3;' + mods + '~';
    case 'PageUp': return '\x1b[5;' + mods + '~';
    case 'PageDown': return '\x1b[6;' + mods + '~';
    case 'F1': return '\x1b[1;' + mods + 'P';
    case 'F2': return '\x1b[1;' + mods + 'Q';
    case 'F3': return '\x1b[13;' + mods + 'u';
    case 'F4': return '\x1b[1;' + mods + 'S';
    case 'F5': return '\x1b[15;' + mods + '~';
    case 'F6': return '\x1b[17;' + mods + '~';
    case 'F7': return '\x1b[18;' + mods + '~';
    case 'F8': return '\x1b[19;' + mods + '~';
    case 'F9': return '\x1b[20;' + mods + '~';
    case 'F10': return '\x1b[21;' + mods + '~';
    case 'F11': return '\x1b[23;' + mods + '~';
    case 'F12': return '\x1b[24;' + mods + '~';
  }
  return null;
}

/* Ctrl+Shift+C/V: перехватываем ДО xterm, чтобы не мешать обычному вводу;
   далее — kitty CSI u для функциональных клавиш, если приложение включило
   протокол; всё остальное — xterm.js */
function termKeyHandler(e) {
  if (e.type !== 'keydown') return true;
  if (e.ctrlKey && e.shiftKey && !e.altKey && e.code === 'KeyC') { termCopy(); return false; }
  if (e.ctrlKey && e.shiftKey && !e.altKey && e.code === 'KeyV') { termPaste(); return false; }
  const o = termObj();
  if (o && o.ws && o.ws.readyState === 1 && kittyFlags(o)) {
    const seq = kittyKeySeq(e);
    if (seq) { termSendObj(o, { type: 'in', data: b64encode(encUTF8(seq)) }); return false; }
  }
  return true;
}
function buildTerminal(root) {
  panelRoots.terminal = root;
  root.innerHTML =
    '<div class="panel term-panel">' +
      '<div class="term-strip" id="term-strip"></div>' +
      '<div class="term-wrap well grow">' +
        '<div class="term-overlay hidden" id="term-overlay"></div>' +
      '</div>' +
      '<div class="errbox hidden"></div>' +
      /* хвост atlas-wezterm — ПОСЛЕДНИЙ ребёнок: grid-строки .term-panel —
         `auto minmax(0,1fr) auto`, скрытый (display:none) блок выпадает из
         раскладки, видимый занимает неявную auto-строку снизу, 1fr остаётся
         у .term-wrap */
      '<div class="hidden" id="wz-tail"></div>' +
    '</div>';
  /* хвост atlas-wezterm: заголовок (панель + время забора) и <pre> с текстом;
     содержимое — только textContent, рамка и шрифт здесь, а не в css */
  const wzTail = root.querySelector('#wz-tail');
  const wzHead = document.createElement('div');
  wzHead.id = 'wz-tail-head';
  wzHead.style.cssText = 'padding:2px 6px;font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;';
  const wzPre = document.createElement('pre');
  wzPre.id = 'wz-tail-pre';
  wzPre.style.cssText = 'margin:0;padding:4px 6px;font-family:var(--font-mono);font-size:11px;' +
    'white-space:pre;overflow:auto;max-height:12em;border-top:1px solid var(--bevel-lo);';
  wzTail.style.cssText = 'margin:0 4px 4px;border:1px solid var(--bevel-lo);overflow:hidden;';
  wzTail.appendChild(wzHead);
  wzTail.appendChild(wzPre);
  chromeFillers.terminal = () => {
    ch.t = {};
    setChrome((mk) => {
      ch.t.info = mk('span', 'dim', 'подключение…');
      const mic = mk('button', 'mini stt-btn', '🎙');
      mic.type = 'button';
      sttRegister(mic, termInsertActive);
      const b = mk('button', 'btn95', '⟳ перезапустить');
      b.type = 'button';
      b.title = 'перезапустить активную сессию (kill + поднять заново)';
      b.addEventListener('click', () => termRestart());
    });
    termFillChrome();
  };
  fillChrome();
  /* вставка файлов/картинки (нативный Ctrl+V): путь(и) в сессию вместо пикселей */
  root.addEventListener('paste', handleTermPaste);
  if (!window.Terminal) {
    const ov = root.querySelector('#term-overlay');
    if (ov) { ov.classList.remove('hidden'); ov.textContent = 'xterm.js не загрузился (vendor/xterm.js)'; }
    termFillChrome();
    return;
  }
  window.addEventListener('resize', () => { const o = termObj(); if (o) termFitObj(o, true); });
  /* сессии — первыми: от них зависит, какой хост показать; затем мост из другого окна */
  loadSessions().then(() => {
    applyTermRequest();
    if (!termActive && termSessions.length) selectTermSession(termSessions[0].id);
    else termShowActive();
  });
  /* чипы не протухают: список сессий обновляется, пока панель видима */
  onTick('termsessions', 10000, termSessionsTick);
  /* хвост atlas-wezterm: тот же общий тикер, работа — только пока «Хвост»
     открыт и панель видима (сторож внутри wzTailTick) */
  onTick('wztail', 2000, wzTailTick);
}
/* опрос списка сессий: ТОЛЬКО пока панель «Терминал» видима
   (отдельное окно или активная вкладка хоста) */
function termSessionsTick() {
  if (!panelRoots.terminal) return;
  if (!(WIN === 'terminal' || (WIN === 'projects' && activeTab === 'terminal'))) return;
  loadSessions();
}
/* вставка голосового текста в АКТИВНУЮ сессию: paste-семантика (xterm.paste —
   скобочная вставка на стороне pty), БЕЗ Enter — отправляет сам пользователь */
function termInsertActive(text) {
  const o = termObj();
  if (!o || !o.xterm) { log('✗ терминал: нет активной сессии'); return false; }
  try { o.xterm.paste(text); return true; }
  catch (_) {
    try { o.xterm.write(text); return true; }
    catch (_2) { return false; }
  }
}
/* повторное открытие/обновление терминала (клавиша r): список + сверка */
async function refreshTerminal() {
  await loadSessions();
  if (!termActive && termSessions.length) selectTermSession(termSessions[0].id);
  else termShowActive();
}

export { buildTerminal, openTerminalPanel, refreshTerminal, termApplyTheme };
