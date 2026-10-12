/* static/js/lib/smooth.js — плавная прокрутка колесом с сохранением якоря (smoothWheel).
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */

/* ---------- smoothWheel: плавная прокрутка колесом ----------
   Щелчки дискретного колеса (USB/GTK: шаг ~40px либо deltaMode line/page)
   копятся в целевой scrollTop и доигрываются rAF-анимацией — прокрутка
   получается непрерывной, как в wezterm, а не ступенчатой. Непрерывный мелкий
   пиксельный поток (тачпад) проходит нативно и не трогается: у него есть своя
   плавность, а анимация поверх ощущается торможением.
   Правила:
   - дискретность: deltaMode !== 0, либо |deltaY| >= 40px; крупный бросок вскоре
     после тачпад-потока (мелкие дельты чаще 40мс) считается инерцией тачпада и
     тоже проходит нативно;
   - цель анимации продлевается каждым новым щелчком и зажимается диапазоном;
   - на границе диапазона в покое колесо не перехватывается — нативная цепочка
     прокрутки на наружный контейнер работает как раньше;
   - ЛЮБОЙ чужой scroll (ползунок, PageUp/PageDown/Home/End, скриптовый
     scrollTo) гасит анимацию и ресинкит цель; свои записи отличаем по
     lastWritten: событие scroll приходит асинхронно, на следующем кадре
     рендеринга, поэтому флага вокруг присваивания недостаточно;
   - prefers-reduced-motion: reduce — перехвата нет вообще, нативный прыжок.
   Попапы mkSelect не участвуют: их колесо гасится в самом mkSelect, а прокрутка
   всё равно закрывает список (прежнее поведение). */
const SMOOTH_SEL = '.tabulator-tableholder, .scroll, .set-scroll, .cards, .done-list';
const SMOOTH_K = 0.3;          /* доля остатка, закрываемая за кадр */
const WHEEL_TICK_PX = 40;      /* «квант» щелчка колеса (GTK/USB) */
const WHEEL_STREAM_MS = 40;    /* события чаще — непрерывный поток (тачпад) */
const WHEEL_STREAM_HOLD = 300; /* сколько мс после потока помнить «это тачпад» */
const smoothMotionQ = matchMedia('(prefers-reduced-motion: reduce)');
let smoothEnabled = !smoothMotionQ.matches;
const smoothStates = new WeakMap();
const smoothLive = new Set();
let wheelLastT = 0, wheelLastAbs = 0, wheelPadUntil = 0;

try {
  smoothMotionQ.addEventListener('change', (e) => {
    smoothEnabled = !e.matches;
    if (!smoothEnabled) for (const n of Array.from(smoothLive)) smoothCancel(n);
  });
} catch (_) { /* старый движок без change у MediaQueryList — режим просто не переключится */ }

function smoothCancel(n) {
  const st = smoothStates.get(n);
  if (!st || !st.raf) return;
  cancelAnimationFrame(st.raf);
  st.raf = 0;
  st.target = n.scrollTop;
  smoothLive.delete(n);
}

function smoothStep(n) {
  const st = smoothStates.get(n);
  if (!st || !st.raf) return;
  const max = n.scrollHeight - n.clientHeight;
  if (st.target > max) st.target = max;
  const rest = st.target - n.scrollTop;
  if (Math.abs(rest) < 0.5) {
    st.raf = 0;
    st.lastWritten = st.target;
    n.scrollTop = st.target;
    smoothLive.delete(n);
    return;
  }
  const cur = n.scrollTop + rest * SMOOTH_K;
  st.lastWritten = cur;
  n.scrollTop = cur;
  st.raf = requestAnimationFrame(() => smoothStep(n));
}

/* дискретное ли колесо: см. блок-комментарий выше */
function wheelIsDiscrete(e) {
  if (e.deltaMode !== 0) return true;
  const abs = Math.abs(e.deltaY), now = performance.now();
  const gap = now - wheelLastT, prevAbs = wheelLastAbs;
  wheelLastT = now; wheelLastAbs = abs;
  if (abs < WHEEL_TICK_PX) {
    if (gap < WHEEL_STREAM_MS && prevAbs < WHEEL_TICK_PX) wheelPadUntil = now + WHEEL_STREAM_HOLD;
    return false;
  }
  return now >= wheelPadUntil;
}

/* самый внутренний реально прокручиваемый контейнер под курсором; отвечает
   только за свои зоны — чужой внутренний скроллер (лог задач, текст оркестратора)
   и цепочку наружу не трогаем */
function smoothWheelTarget(t) {
  for (let n = t instanceof Element ? t : null; n && n !== document.body; n = n.parentElement) {
    if (n.scrollHeight > n.clientHeight + 1) {
      const ov = getComputedStyle(n).overflowY;
      if (ov === 'auto' || ov === 'scroll') return n.matches(SMOOTH_SEL) ? n : null;
    }
  }
  return null;
}

document.addEventListener('wheel', (e) => {
  if (!smoothEnabled || e.defaultPrevented) return;
  if (Math.abs(e.deltaY) < Math.abs(e.deltaX)) return; /* горизонталь — нативно */
  if (!wheelIsDiscrete(e)) return;
  const n = smoothWheelTarget(e.target);
  if (!n) return;
  let st = smoothStates.get(n);
  if (!st) smoothStates.set(n, st = { target: n.scrollTop, raf: 0, lastWritten: n.scrollTop });
  if (!st.raf) st.target = n.scrollTop;
  const max = n.scrollHeight - n.clientHeight;
  let d = e.deltaY;
  if (e.deltaMode === 1) d *= WHEEL_TICK_PX;
  else if (e.deltaMode >= 2) d *= Math.max(1, n.clientHeight);
  const toward = d > 0 ? st.target < max : st.target > 0;
  if (!toward && !st.raf) return; /* граница в покое — пусть работает цепочка */
  e.preventDefault();
  st.target = Math.max(0, Math.min(max, st.target + d));
  if (!st.raf) { smoothLive.add(n); st.raf = requestAnimationFrame(() => smoothStep(n)); }
}, { passive: false });

/* чужие прокрутки гасят анимацию; scroll не всплывает — ловим на capture */
document.addEventListener('scroll', (e) => {
  const n = e.target;
  if (!(n instanceof Element)) return;
  const st = smoothStates.get(n);
  if (!st) return;
  if (!st.raf) { st.target = n.scrollTop; return; }
  if (Math.abs(n.scrollTop - st.lastWritten) >= 1) smoothCancel(n); /* не наша запись */
}, true);

export { smoothCancel };
