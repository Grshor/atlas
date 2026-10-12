/* static/js/lib/api.js — бэкенд: базовый URL, fetch-обёртки, base64/UTF-8, ws-база, allowlist URL.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
/* ---------- бэкенд: тот же origin, если отдаёт сам бэкенд, иначе фиксированный порт.
   ВАЖНО: внутри webview Tauri location.protocol === 'tauri:', и база вида
   'tauri://127.0.0.1:8787' не работает — WebKitGTK роняет такой fetch на сетевом
   слое. Поэтому относительные запросы — только когда страница отдана самим
   бэкендом (http/https И порт 8787); во всех остальных случаях (tauri:, file:,
   другой порт) — явно на http://127.0.0.1:8787. */
const API = (() => {
  const web = location.protocol === 'http:' || location.protocol === 'https:';
  return (web && location.port === '8787') ? '' : 'http://127.0.0.1:8787';
})();
window.__ATLAS_API = API; /* для проверок, без console-шума */

async function api(path, opts) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 8000);
  try {
    const res = await fetch(API + path, Object.assign({ signal: ctl.signal }, opts));
    if (!res.ok) throw new Error('HTTP ' + res.status);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}
const jsonOpts = (method, body) => ({
  method,
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify(body),
});

/* ---------- утилиты новых панелей ----------
   base64↔байты и UTF-8: кадры терминала — base64 от UTF-8, поэтому кодируем
   именно байты, а не UTF-16-строку (иначе кириллица и эмодзи ломаются). */
const b64encode = (bytes) => {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s);
};
const b64decode = (b64) => {
  const bin = atob(b64 || '');
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
};
const encUTF8 = (s) => new TextEncoder().encode(s);
/* NB: декодирование байтов кадра — НЕ через новый TextDecoder на кадр, а
   единым декодером на сессию (static/js/terminal.js): WS-кадр режет поток
   байт где попало, расщеплённый UTF-8 хвост живёт в декодере между кадрами. */

/* ws(s):// базы API — из window.__ATLAS_API (пусто → тот же origin страницы) */
function wsBase() {
  const b = API || location.origin || 'http://127.0.0.1:8787';
  return b.replace(/^http/, 'ws');
}

/* allowlist панели «Просмотр»: только http://127.0.0.1:<port>/… и
   http://localhost:<port>/… — ни других хостов, ни https, ни путей без порта. */
function localOnlyUrl(raw) {
  const s = String(raw == null ? '' : raw).trim();
  if (!s) return { ok: false, reason: 'адрес пуст' };
  let u;
  try { u = new URL(s); } catch (_) { return { ok: false, reason: 'это не похоже на URL' }; }
  if (u.protocol !== 'http:') return { ok: false, reason: 'разрешён только http:// (не https и не другие схемы)' };
  if (u.hostname !== '127.0.0.1' && u.hostname !== 'localhost') return { ok: false, reason: 'только 127.0.0.1 или localhost' };
  const hadPort = /^http:\/\/(?:127\.0\.0\.1|localhost):\d+/.test(s);
  if (!u.port && !hadPort) return { ok: false, reason: 'укажите порт, например http://127.0.0.1:3000/' };
  return { ok: true, url: u.href };
}

export { API, api, b64decode, b64encode, encUTF8, jsonOpts, localOnlyUrl, wsBase };
