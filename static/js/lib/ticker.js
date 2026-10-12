/* static/js/lib/ticker.js — ручные тикеры панелей: onTick + реестр обработчиков.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
const tickTimers = {}; /* интервалы onTick */
const TICK_LOADERS = {};
function onTick(key, ms, fn) {
  TICK_LOADERS[key] = fn;
  clearInterval(tickTimers[key]);
  tickTimers[key] = setInterval(() => { try { fn(); } catch (_) {} }, ms);
}

export { TICK_LOADERS, onTick };
