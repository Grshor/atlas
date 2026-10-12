/* static/js/lib/dom.js — dom-утилиты: $/esc/el, журнал #log, буфер обмена, вставка в курсор.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */

/* ---------- dom-утилиты ---------- */
const $ = (s, r) => (r || document).querySelector(s);
const esc = (v) => String(v == null ? '' : v).replace(/[&<>"']/g,
  (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
function el(tag, cls, html) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (html != null) n.innerHTML = html;
  return n;
}

/* ---------- статус-строка окна: #chrome (панель) + #log + #probe ---------- */
function log(msg) {
  const n = $('#log');
  if (!n) return;
  n.textContent = msg;
  n.title = msg; /* строка обрезается многоточием — полный текст в подсказке */
}
/* копирование в буфер: Clipboard API, при отказе — скрытое поле + execCommand.
   Промптов нет: успех/неуспех — только строкой в журнале. */
function copyText(text, what, failMsg) {
  if (!text) { log('✗ нечего копировать'); return; }
  const ok = () => log('✓ скопировано: ' + (what || 'текст'));
  const fallback = () => {
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.setAttribute('readonly', '');
      ta.style.position = 'fixed';
      ta.style.left = '-9999px';
      document.body.appendChild(ta);
      ta.select();
      const done = document.execCommand('copy');
      ta.remove();
      if (done) ok();
      else log('✗ ' + (failMsg || 'буфер обмена недоступен в этом окне'));
    } catch (_) { log('✗ ' + (failMsg || 'буфер обмена недоступен в этом окне')); }
  };
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).then(ok, fallback);
    else fallback();
  } catch (_) { fallback(); }
}
/* вставка текста в позицию курсора поля (textarea/input) с возвратом фокуса */
function insertAtCursor(ta, text) {
  const s = ta.selectionStart == null ? ta.value.length : ta.selectionStart;
  const epos = ta.selectionEnd == null ? s : ta.selectionEnd;
  ta.value = ta.value.slice(0, s) + text + ta.value.slice(epos);
  const pos = s + text.length;
  try { ta.setSelectionRange(pos, pos); } catch (_) {}
  ta.focus();
}

export { $, copyText, el, esc, insertAtCursor, log };
