/* static/js/lib/fmt.js — форматирование длительности (fmtSecs).
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
function fmtSecs(v) {
  if (v == null || !Number.isFinite(Number(v))) return '?';
  return (Math.round(Number(v) * 10) / 10) + 'с';
}

export { fmtSecs };
