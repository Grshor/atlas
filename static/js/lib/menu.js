/* static/js/lib/menu.js — свои выпадашка/контекстное меню (mkSelect/mkMenu) вместо нативных.
   Вырезано из static/app.js без изменений поведения (разрезка монолита, FRAGILE.md §4). */
import { el, esc, log } from './dom.js';

/* ---------- mkSelect: своя выпадашка вместо нативного select ----------
   Нативный select рисует попап средствами ОС — светлым и мимо Win95/omarchy-хрома,
   поэтому в UI нативных select больше нет: список рисует сам компонент (кнопка в
   стиле поля + свой listbox), значения и обработчики те же. Позиция —
   position:fixed от прямоугольника кнопки с прижатием к вьюпорту (карточки доски
   живут в скроллящемся контейнере), поэтому открытый список закрывается на
   scroll/resize/клик вне. Экземпляров много (по одному на карточку доски) —
   сам список создаётся при открытии и снимается при закрытии. */
let mselSeq = 0;
let mselOpen = null; /* { btn, close } — открыт ровно один список */

function mselCloseOpen() { if (mselOpen) mselOpen.close(); }

function mkSelect(cfg) {
  const c = cfg || {};
  const opts = Array.isArray(c.options) ? c.options.slice() : [];
  const btn = el('button', 'msel');
  btn.type = 'button';
  if (c.id) btn.id = c.id;
  if (c.title) btn.title = c.title;
  if (c.disabled) btn.disabled = true;
  btn.setAttribute('aria-haspopup', 'listbox');
  btn.setAttribute('aria-expanded', 'false');
  const lab = el('span', 'msel-label');
  btn.append(lab, el('span', 'msel-caret', '▼'));

  let value = c.value;
  let st = null;          /* открытое состояние: { pop, items, setActive, pick, activeIndex, onDoc } */
  let taBuf = '', taAt = 0;

  const find = (v) => opts.find((o) => String(o.value) === String(v)) || null;
  const labelOf = (v) => {
    const o = find(v);
    return o ? (o.label != null ? String(o.label) : String(o.value)) : (v == null ? '—' : String(v));
  };
  const render = () => { lab.textContent = labelOf(value); btn.dataset.value = value == null ? '' : String(value); };
  render();

  function close() {
    if (!st) return;
    document.removeEventListener('mousedown', st.onDoc, true);
    window.removeEventListener('scroll', close, true);
    window.removeEventListener('resize', close);
    if (st.pop.parentNode) st.pop.parentNode.removeChild(st.pop);
    st = null;
    btn.setAttribute('aria-expanded', 'false');
    btn.removeAttribute('aria-activedescendant');
    btn.classList.remove('open');
    if (mselOpen && mselOpen.btn === btn) mselOpen = null;
    /* фокус возвращаем только если он потерялся (клик по пустому месту);
       если пользователь ушёл в другое поле — не отбираем */
    setTimeout(() => {
      if (!btn.isConnected || btn.disabled) return;
      const a = document.activeElement;
      if (!a || a === document.body) { try { btn.focus(); } catch (_) {} }
    }, 0);
  }

  function open() {
    if (st || btn.disabled) return;
    if (mselOpen) mselOpen.close(); /* открыт ровно один список */
    const pop = el('div', 'msel-pop');
    pop.setAttribute('role', 'listbox');
    const list = el('div', 'msel-list');
    pop.appendChild(list);
    const items = opts.map((o) => {
      const sel = String(o.value) === String(value);
      const it = el('div', 'msel-opt' + (sel ? ' sel' : ''));
      it.id = 'msel-' + (++mselSeq);
      it.setAttribute('role', 'option');
      it.setAttribute('aria-selected', sel ? 'true' : 'false');
      it.textContent = o.label != null ? String(o.label) : String(o.value);
      list.appendChild(it);
      return it;
    });
    document.body.appendChild(pop);

    /* прижатие к вьюпорту: сначала вниз от кнопки, при нехватке места — вверх */
    const vw = document.documentElement.clientWidth, vh = document.documentElement.clientHeight;
    const r = btn.getBoundingClientRect();
    pop.style.maxWidth = Math.max(80, vw - 8) + 'px';
    pop.style.minWidth = Math.round(r.width) + 'px';
    const w = pop.offsetWidth, h = pop.offsetHeight;
    const left = Math.max(4, Math.min(r.left, vw - w - 4));
    const below = r.bottom + 2;
    const top = (below + h > vh - 4 && r.top - h - 2 >= 4) ? r.top - h - 2 : Math.max(4, Math.min(below, vh - h - 4));
    pop.style.left = Math.round(left) + 'px';
    pop.style.top = Math.round(top) + 'px';

    let active = Math.max(0, opts.findIndex((o) => String(o.value) === String(value)));
    const setActive = (i) => {
      if (!items.length) return;
      active = (i + items.length) % items.length;
      items.forEach((it, k) => it.classList.toggle('active', k === active));
      btn.setAttribute('aria-activedescendant', items[active].id);
      try { items[active].scrollIntoView({ block: 'nearest' }); } catch (_) {}
    };
    const pick = (i) => {
      const o = opts[i];
      if (!o) return;
      const connected = btn.isConnected; /* панель могла перерисоваться под открытым списком */
      close();
      if (!connected) return;
      if (String(o.value) === String(value)) { render(); return; }
      value = o.value;
      render();
      if (typeof c.onchange === 'function') c.onchange(value, o);
    };
    const onDoc = (e) => { if (st && !st.pop.contains(e.target) && !btn.contains(e.target)) close(); };
    /* activeIndex — живой: стрелки двигают его, Enter берёт именно текущий */
    st = { pop, items, setActive, pick, onDoc, get activeIndex() { return active; } };
    setActive(active);

    items.forEach((it, i) => {
      it.addEventListener('mousedown', (e) => e.preventDefault()); /* фокус остаётся на кнопке */
      it.addEventListener('click', () => pick(i));
    });
    pop.addEventListener('wheel', (e) => e.stopPropagation(), { passive: true }); /* прокручивается только список */
    document.addEventListener('mousedown', onDoc, true);
    window.addEventListener('scroll', close, true);
    window.addEventListener('resize', close);
    btn.setAttribute('aria-expanded', 'true');
    btn.classList.add('open');
    mselOpen = { btn, close };
  }

  btn.addEventListener('keydown', (e) => {
    if (!st) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ' || e.key === 'Spacebar') {
        e.preventDefault();
        open();
      }
      return;
    }
    switch (e.key) {
      case 'ArrowDown': e.preventDefault(); st.setActive(st.activeIndex + 1); break;
      case 'ArrowUp': e.preventDefault(); st.setActive(st.activeIndex - 1); break;
      case 'Home': e.preventDefault(); st.setActive(0); break;
      case 'End': e.preventDefault(); st.setActive(st.items.length - 1); break;
      case 'Enter': case ' ': case 'Spacebar':
        e.preventDefault(); e.stopPropagation(); st.pick(st.activeIndex); break;
      case 'Escape':
        e.preventDefault(); e.stopPropagation(); close(); break; /* не даём Esc утечь в глобальный обработчик */
      case 'Tab': close(); break;
      default:
        /* набор с клавиатуры (type-ahead), как у нативного списка */
        if (typeof e.key === 'string' && e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey && st.items.length) {
          const now = Date.now();
          taBuf = (now - taAt > 800) ? e.key : taBuf + e.key;
          taAt = now;
          const hit = opts.findIndex((o) => String(o.label != null ? o.label : o.value).toLowerCase().indexOf(taBuf.toLowerCase()) === 0);
          if (hit >= 0) { e.preventDefault(); st.setActive(hit); }
        }
    }
  });
  btn.addEventListener('click', () => { if (st) close(); else open(); });
  return btn;
}

/* ---------- mkMenu: контекстное меню (ПКМ) в том же виде, что попап mkSelect ----
   Тот же тёмный фасочный попап (msel-pop), прижатие к вьюпорту, закрытие по
   Esc/клику вне/скроллу/resize, стрелки + Enter, hover-подсветка. Пункты:
   {label, action, danger?, disabled?, hint?} и разделители {sep:true}.
   hint — приглушённая подпись справа (и tooltip); открыт ровно один попап
   вместе со списками mkSelect (общий счётчик mselOpen). */
function mkMenu(items, x, y) {
  mselCloseOpen();
  const pop = el('div', 'msel-pop ctxmenu');
  pop.tabIndex = -1;
  pop.setAttribute('role', 'menu');
  const list = el('div', 'ctxmenu-list');
  pop.appendChild(list);
  const enabled = [];
  for (const it of (Array.isArray(items) ? items : [])) {
    if (!it) continue;
    if (it.sep) { list.appendChild(el('div', 'ctxmenu-sep')); continue; }
    const row = el('div', 'ctxmenu-item' + (it.disabled ? ' disabled' : '') + (it.danger ? ' danger' : ''));
    row.setAttribute('role', 'menuitem');
    if (it.hint) row.title = String(it.hint);
    row.appendChild(el('span', 'ctxmenu-label', esc(it.label)));
    if (it.hint) row.appendChild(el('span', 'ctxmenu-hint', esc(it.hint)));
    list.appendChild(row);
    if (it.disabled) continue;
    const en = { row, it };
    row.addEventListener('mouseenter', () => setActive(enabled.indexOf(en)));
    row.addEventListener('click', () => {
      close();
      if (typeof it.action === 'function') { try { it.action(); } catch (e) { log('✗ ' + e.message); } }
    });
    enabled.push(en);
  }
  document.body.appendChild(pop);

  /* прижатие к вьюпорту: от точки (x,y), при нехватке места — влево/вверх */
  const vw = document.documentElement.clientWidth, vh = document.documentElement.clientHeight;
  const w = pop.offsetWidth, h = pop.offsetHeight;
  const left = Math.max(4, Math.min(x, vw - w - 4));
  const top = Math.max(4, Math.min(y, vh - h - 4));
  pop.style.left = Math.round(left) + 'px';
  pop.style.top = Math.round(top) + 'px';

  let active = -1;
  const setActive = (i) => {
    if (!enabled.length) return;
    active = (i + enabled.length) % enabled.length;
    enabled.forEach((en, k) => en.row.classList.toggle('active', k === active));
    try { enabled[active].row.scrollIntoView({ block: 'nearest' }); } catch (_) {}
  };
  const onDoc = (e) => { if (!pop.contains(e.target)) close(); };
  function close() {
    if (!pop.parentNode) return;
    document.removeEventListener('mousedown', onDoc, true);
    document.removeEventListener('keydown', onKey, true);
    window.removeEventListener('scroll', close, true);
    window.removeEventListener('resize', close);
    if (pop.parentNode) pop.parentNode.removeChild(pop);
    if (mselOpen && mselOpen.close === close) mselOpen = null;
  }
  function onKey(e) {
    if (!pop.parentNode) return;
    switch (e.key) {
      case 'ArrowDown': e.preventDefault(); e.stopPropagation(); setActive(active + 1); break;
      case 'ArrowUp': e.preventDefault(); e.stopPropagation(); setActive(active - 1); break;
      case 'Home': e.preventDefault(); e.stopPropagation(); setActive(0); break;
      case 'End': e.preventDefault(); e.stopPropagation(); setActive(enabled.length - 1); break;
      case 'Enter': case ' ': case 'Spacebar': {
        e.preventDefault(); e.stopPropagation();
        const en = enabled[active];
        if (en) { close(); if (typeof en.it.action === 'function') { try { en.it.action(); } catch (err) { log('✗ ' + err.message); } } }
        break;
      }
      case 'Escape': e.preventDefault(); e.stopPropagation(); close(); break; /* Esc не утекает в глобальный обработчик */
      case 'Tab': close(); break;
      default: break;
    }
  }
  document.addEventListener('mousedown', onDoc, true);
  document.addEventListener('keydown', onKey, true);
  window.addEventListener('scroll', close, true);
  window.addEventListener('resize', close);
  pop.addEventListener('contextmenu', (e) => e.preventDefault()); /* внутри меню нативного меню нет */
  mselOpen = { close };
  pop.focus(); /* фокус на попапе — стрелки/Enter работают сразу */
  return { close, pop };
}

export { mkMenu, mkSelect, mselCloseOpen };
