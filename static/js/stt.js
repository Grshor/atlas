/* static/js/stt.js — голосовой ввод (STT): старт/стоп/опрос, кнопки-микрофоны, push-to-talk.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { $, insertAtCursor, log } from './lib/dom.js';
import { api, jsonOpts } from './lib/api.js';
import { fmtSecs } from './lib/fmt.js';

/* =====================================================================
   голосовой ввод (STT): POST /api/stt/start → {ok,id} (409 — уже пишут),
   POST /api/stt/stop → {ok,text,duration_s} (синхронный, может занять секунды —
   загрузка модели), GET /api/stt/status → {recording,transcribing}.
   Кнопка-микрофон живёт в строке ввода «Оркестратора» и в хроме «Терминала»;
   push-to-talk — удержание пробела в ПУСТОМ сфокусированном поле оркестратора
   (диалог открыт / поле не пусто → обычный пробел). Пока пишем, пробел
   перехватывается; на остановке текст вставляется: в оркестратор — в курсор,
   в терминал — paste в АКТИВНУЮ сессию, всегда БЕЗ Enter.
   Статус опрашивается раз в секунду, пока запись/распознавание идут, — после
   перезагрузки окна состояние подхватывается; 404 от /api/stt/* прячет всё.
   =================================================================== */
const STT_IV = 1000;
let sttAvailable = null;  /* null — неизвестно; true — есть; false — 404 (спрятать) */
let sttRecording = false;
let sttTranscribing = false;
let sttStartedAt = null;  /* мс начала записи — для «● Nс» */
let sttInsertFn = null;   /* куда вставлять: fn(text) → bool */
let sttPtt = false;       /* запись начата удержанием пробела */
let sttOwnStop = false;   /* наш stop в полёте — серверную истину не применяем */
let sttBtns = new Set();  /* живые кнопки-микрофоны (chrome перестраивается) */
let sttPollTimer = null;
let sttHintShown = false; /* подсказка про PTT — один раз за сессию */
let sttPttStopOnStart = false; /* пробел отпустили, пока start ещё в полёте */
let sttHoldTimer = 0;     /* таймер удержания пробела (250 мс до старта записи) */

function sttRegister(btn, insertFn) {
  btn.classList.add('stt-btn');
  /* первая зарегистрированная кнопка задаёт цель вставки ПО УМОЛЧАНИЮ:
     запись, подхваченная после перезагрузки (adopt из status), стартует без
     явной цели — остановка вставит в свою панель, а не «некуда вставить» */
  if (insertFn && !sttInsertFn) sttInsertFn = insertFn;
  btn.addEventListener('click', () => {
    if (sttRecording) sttStopAndInsert();
    else sttStart(insertFn || sttInsertFn, 'click');
  });
  sttBtns.add(btn);
  sttRender();
  /* дежурный статус только при первом знакомстве: запись могла идти до
     перезагрузки окна; на 404 кнопки скрываются вовсе */
  if (sttAvailable == null) sttPollOnce();
}
function sttOrchInsert(text) {
  if (!orchInputEl) return false;
  insertAtCursor(orchInputEl, text);
  return true;
}
/* одноразовая подсказка про push-to-talk: первый фокус пустого поля за сессию */
function sttShowHint() {
  if (sttHintShown || sttAvailable === false || !orchInputEl) return;
  sttHintShown = true;
  const h = document.getElementById('stt-hint');
  if (!h) return;
  h.classList.remove('hidden');
  const hide = () => h.classList.add('hidden');
  setTimeout(hide, 8000);
  orchInputEl.addEventListener('blur', hide, { once: true });
}
async function sttStart(insertFn, via) {
  if (sttAvailable === false || sttRecording || sttTranscribing) return false;
  try {
    const r = await api('/api/stt/start', jsonOpts('POST', {}));
    if (r && r.ok) {
      sttRecording = true;
      sttStartedAt = Date.now();
      if (insertFn) sttInsertFn = insertFn;
      sttPtt = via === 'ptt';
      sttStartPoll();
      sttRender();
      if (sttPtt && sttPttStopOnStart) { sttPttStopOnStart = false; sttStopAndInsert(); }
      return true;
    }
    log('✗ голосовой ввод: ' + ((r && r.detail) || 'не удалось начать запись'));
    return false;
  } catch (e) {
    const m = String(e.message);
    if (m.indexOf('409') !== -1) {
      /* пишет другое окно/панель — подхватываем общую запись */
      sttRecording = true;
      if (sttStartedAt == null) sttStartedAt = Date.now();
      if (insertFn) sttInsertFn = insertFn;
      sttPtt = via === 'ptt';
      sttStartPoll();
      sttRender();
      log('голосовой ввод: запись уже идёт — продолжаю её');
      if (sttPtt && sttPttStopOnStart) { sttPttStopOnStart = false; sttStopAndInsert(); }
      return true;
    }
    if (m.indexOf('404') !== -1) sttHide();
    else log('✗ голосовой ввод: ' + e.message);
    return false;
  }
}
async function sttStopAndInsert() {
  if (!sttRecording) return;
  sttRecording = false;
  sttTranscribing = true; /* остановка синхронная — может занять секунды */
  sttOwnStop = true;
  sttPtt = false;
  sttRender();
  try {
    const r = await api('/api/stt/stop', jsonOpts('POST', {}));
    sttTranscribing = false;
    sttStartedAt = null;
    if (!r || r.ok === false) {
      log('✗ голосовой ввод: ' + ((r && r.detail) || 'не удалось'));
      return;
    }
    /* хвостовые переводы строк срезаем: paste НЕ должен отправлять Enter */
    const text = String(r.text || '').replace(/[\r\n]+$/, '');
    if (!text) { log('✗ голосовой ввод: пустой текст — ничего не вставлено'); return; }
    const fn = sttInsertFn;
    if (!fn || !fn(text)) log('✗ голосовой ввод: некуда вставить');
    else log('✓ голосовой ввод: вставлено' + (r.duration_s != null ? ' (' + fmtSecs(r.duration_s) + ')' : ''));
  } catch (e) {
    sttTranscribing = false;
    sttStartedAt = null;
    const m = String(e.message);
    if (m.indexOf('404') !== -1) sttHide();
    else log('✗ голосовой ввод: ' + e.message);
  } finally {
    sttOwnStop = false;
    sttStopPoll();
    sttRender();
  }
}
function sttStartPoll() { if (!sttPollTimer) sttPollTimer = setInterval(() => { try { sttPollTick(); } catch (_) {} }, STT_IV); }
function sttStopPoll() { clearInterval(sttPollTimer); sttPollTimer = null; }
async function sttPollTick() {
  if (!sttRecording && !sttTranscribing) { sttStopPoll(); return; }
  try {
    const st = await api('/api/stt/status');
    if (sttAvailable === false) return;
    sttAvailable = true;
    if (!sttOwnStop) {
      /* своих запросов в полёте нет — серверная истина: запись могли
         остановить или начать в другом окне */
      sttRecording = !!st.recording;
      sttTranscribing = !!st.transcribing;
      if (sttRecording && sttStartedAt == null) sttStartedAt = Date.now();
      if (!sttRecording && !sttTranscribing) { sttStartedAt = null; sttStopPoll(); }
    }
    sttRender();
  } catch (e) {
    if (String(e.message).indexOf('404') !== -1) sttHide();
    /* сеть мигнула — опрос продолжается */
  }
}
async function sttPollOnce() {
  try {
    const st = await api('/api/stt/status');
    sttAvailable = true;
    if (st && st.recording) {
      sttRecording = true;
      if (sttStartedAt == null) sttStartedAt = Date.now();
      sttStartPoll();
    }
    sttRender();
  } catch (e) {
    if (String(e.message).indexOf('404') !== -1) sttHide();
  }
}
function sttHide() {
  sttAvailable = false;
  sttRecording = false;
  sttTranscribing = false;
  sttStartedAt = null;
  sttStopPoll();
  sttRender();
}
function sttRender() {
  for (const b of Array.from(sttBtns)) {
    if (!b.isConnected) { sttBtns.delete(b); continue; }
    b.classList.toggle('hidden', sttAvailable === false);
    if (sttAvailable === false) continue;
    b.classList.toggle('rec', sttRecording);
    if (sttTranscribing) {
      b.textContent = 'распознаю…';
      b.disabled = true;
      b.title = 'распознаю запись…';
    } else if (sttRecording) {
      const s = sttStartedAt ? Math.max(0, Math.round((Date.now() - sttStartedAt) / 1000)) : 0;
      b.textContent = '● ' + s + 'с';
      b.disabled = false;
      b.title = 'идёт запись — клик: остановить и вставить';
    } else {
      b.textContent = '🎙';
      b.disabled = false;
      b.title = 'голосовой ввод: клик — начать, повторный клик — остановить и вставить';
    }
  }
}

/* push-to-talk «Оркестратора»: удержание пробела в ПУСТОМ сфокусированном поле
   (переехало из buildOrchestrator без изменений — состояние PTT живёт здесь).
   onEnter — Enter без shift в поле ввода (отправка сообщения оркестратору). */
function sttAttachPtt(input, onEnter) {
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); onEnter(); return; }
    /* push-to-talk: удержание пробела в ПУСТОМ поле (без диалога) — запись;
       пока пишем, пробел перехватывается и не печатается */
    if (e.code !== 'Space' || sttAvailable === false) return;
    if (sttRecording || sttTranscribing) { e.preventDefault(); return; }
    if (e.repeat) return;
    if (input.value || document.querySelector('.overlay')) return;
    e.preventDefault(); /* удержание началось: символ не печатаем */
    sttPttStopOnStart = false;
    sttHoldTimer = setTimeout(() => {
      sttHoldTimer = 0;
      sttStart(sttOrchInsert, 'ptt');
    }, 250);
  });
  input.addEventListener('keyup', (e) => {
    if (e.code !== 'Space') return;
    if (sttHoldTimer) { clearTimeout(sttHoldTimer); sttHoldTimer = 0; return; } /* короткий тап — не запись */
    if (sttRecording && sttPtt) sttStopAndInsert();
    else if (sttTranscribing || sttPttStopOnStart) sttPttStopOnStart = true; /* start ещё в полёте */
  });
  input.addEventListener('blur', () => {
    if (sttHoldTimer) { clearTimeout(sttHoldTimer); sttHoldTimer = 0; }
  });
}

export { sttAttachPtt, sttOrchInsert, sttRegister, sttShowHint };
