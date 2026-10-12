/* atlas — фронтенд: ОДНА панель на реальное окно (окном управляет WM/Hyprland,
   внутри — Win95-хром). Панель выбирается параметром ?win=…; по умолчанию — проекты.
   Главное окно («проекты») — хост: свёрнутые окна живут в нём вкладками (GET /api/ui).
   Vanilla JS, без фреймворков и внешних URL. Тема синхронизируется с /api/theme,
   настройки — с /api/settings (недоступно — значения по умолчанию, без спама ошибками). */

import { $, log } from './js/lib/dom.js';
import { BUILDERS, TITLES, WIN, activePanel, applyDerivedColors, armNotifications, fetchTheme, loadNotifications, loadSettings, pollUi, probe, refreshSettingsSilent, themeStreamStart, timers } from './js/core.js';
import { buildHost, loadProjects, projCache, renderProjects } from './js/projects.js';
import { loadBoard } from './js/board.js';
import { loadDeps } from './js/deps.js';
import { loadHerdr, renderHerdr } from './js/herdr.js';
import { loadSources } from './js/sources.js';
import { onTick } from './js/lib/ticker.js';
import { refreshOrchestrator } from './js/orchestrator.js';
import { refreshTerminal } from './js/terminal.js';
import { runSearch } from './js/search.js';
import { viewFrame } from './js/view.js';

/* Точка входа. Сама логика живёт в модулях static/js/ (домены: core, projects,
   board, deps, search, sources, setview, herdr, view, terminal, stt,
   orchestrator + lib/*). Здесь — только сборка/загрузка панели и глобальные
   обработчики окна. Разрезка монолита — FRAGILE.md §4. */

/* ---------- переходный window-шим ----------
   Инлайновых атрибутов (onclick=…) ни в index.html, ни в генерируемом DOM нет —
   exposing нечего, вся проводка идёт через addEventListener внутри модулей.
   Единственное существующее window-поле — __ATLAS_API (проверки), оно
   выставляется в js/lib/api.js. Если инлайновые обработчики появятся
   (например, после конвертации строк HTML) — добавлять их exposure СЮДА,
   по одному импорту и присваиванию, пока они не будут переведены
   на addEventListener и шим не удалят. */
try {
  if (WIN === 'projects') buildHost($('#app'));
  else BUILDERS[WIN]($('#app'));
} catch (e) {
  log('ошибка отрисовки: ' + e.message);
}
applyDerivedColors(); /* литералы вместо color-mix() — до первого :hover по строкам */
fetchTheme();
themeStreamStart();
loadSettings();
probe();
/* GET /api/ui — во всех окнах: хосту для вкладок, настройкам для «Окон»,
   остальным — ради честного состояния «⌄ свернуть»/«⚙ настройки» */
pollUi();
onTick('ui', 5000, pollUi);
/* уведомления: раз в min(autorefresh_sec,15)с; скрыты, пока эндпоинта нет */
armNotifications();
clearInterval(timers.theme);
timers.theme = setInterval(() => { fetchTheme(); refreshSettingsSilent(); }, 30000);
clearInterval(timers.probe);
timers.probe = setInterval(probe, 60000);
document.addEventListener('visibilitychange', () => { if (!document.hidden) { fetchTheme(); probe(); refreshSettingsSilent(); pollUi(); loadNotifications(); } });

/* клавиатура: «/» — фокус в фильтр/поиск, «r» — обновить панель, Escape — очистить */
const LOADERS = {
  projects: loadProjects,
  board: loadBoard,
  deps: loadDeps,
  search: runSearch,
  sources: loadSources,
  herdr: loadHerdr,
  view: () => { if (viewFrame && viewFrame.src) viewFrame.src = viewFrame.src; },
  terminal: refreshTerminal,
  orchestrator: refreshOrchestrator,
};
function refreshPanel() {
  const p = activePanel();
  log('обновление: ' + (TITLES[p] || p));
  const fn = LOADERS[p];
  if (fn) fn();
}
/* первый ВИДИМый ввод (в хосте скрытые вкладки не должны ловить фокус) */
function visibleInput() {
  for (const n of [$('#q'), $('#filter'), $('#herdr-filter'), $('#view-url')]) if (n && n.offsetParent) return n;
  return null;
}
document.addEventListener('keydown', (e) => {
  const typing = /^(INPUT|SELECT|TEXTAREA)$/.test(document.activeElement && document.activeElement.tagName || '');
  const overlay = !!document.querySelector('.overlay');
  if (e.key === '/' && !typing) {
    const target = visibleInput();
    if (target) { e.preventDefault(); target.focus(); if (target.select) target.select(); }
  } else if ((e.key === 'r' || e.key === 'р') && !typing && !overlay) {
    e.preventDefault();
    refreshPanel();
  } else if (e.key === 'Escape' && !overlay) {
    const target = visibleInput();
    if (target && target.value) {
      target.value = '';
      if (target.id === 'q') runSearch();
      else if (target.id === 'herdr-filter') renderHerdr();
      else renderProjects(projCache);
    }
  }
});

window.addEventListener('unhandledrejection', (e) => {
  e.preventDefault();
  log('фоновая ошибка: ' + (e.reason && e.reason.message ? e.reason.message : 'неизвестная'));
});
window.addEventListener('error', (e) => {
  e.preventDefault();
  log('ошибка: ' + (e.message || 'неизвестная'));
});
