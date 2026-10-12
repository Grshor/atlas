# WEZTERM-EMBED: заэмбедить wezterm в atlas — вердикты и поэтапный дизайн

*Исследование 2026-10-10. Бокс: Arch, Hyprland 0.56.2, wezterm 20260716-195552-76b606ec, Wayland (wayland-1), atlas = Tauri v2 + WebKitGTK. Всё эмпирическое ниже проверено на этой машине: спавнились/убивались **пробные** экземпляры wezterm класса `atlas-probe` (Wayland и XWayland), окно владельца не тронуто; `hyprctl dispatch/keyword` не запускался вовсе (гвард) — все диспатчеры ниже спеканы по wiki и валидированы по `hyprctl version`, их живую примерку делает владелец. Прототипы лежат в `/tmp/wz-probe*.lua`, окна и сокеты убраны.*

---

## 0. Жёсткое ограничение, с которого начинаемся

**На Wayland приложение не может забрать чужое окно внутрь своего.** Нет аналога `XReparentWindow`: `xdg_toplevel` не имеет кросс-клиентского родителя, sub-surfaces — только внутри своего клиента. WebKitGTK не умеет хостить нативные дочерние окна (XEmbed/NPAPI-плагины мертвы), Tauri/wry такого API не дают. Значит, «заэмбедить wezterm в webview» в буквальном смысле **невозможно не из-за wezterm, а из-за протокола** — это надо было доказать, а не предполагать, и ниже каждый тупик проверен.

«Наш хром» на Wayland — это честная сделка: **геометрия + раздевание окна + пульт**. Окно wezterm остаётся отдельным toplevel, но atlas кладёт его пиксель-в-пиксель куда нужно, снимает с него титлбар/бордер/тень и управляет показом/скрытием/фокусом. Визуально это «терминал atlas», по архитектуре — настоящий терминал (FRAGILE §1).

---

## 1. Таблица вердиктов

| # | Кандидат | Вердикт | Ключевое доказательство | Труд |
|---|---|---|---|---|
| A1 | wezterm mux как поток данных для нашего рендерера (xterm.js) | **тупик** | `wezterm cli --help`: нет ни одной стриминговой читалки; `get-text` — one-shot. `cli proxy` (rpc-пайп) в актуальных доках **не документирован** (в `docs/cli/cli/` файла proxy.md нет); mux-протокол «experimental», внутренний | L |
| A2 | `wezterm-mux-server` + домены: рендер в чужое GTK-окно | **тупик** | `wezterm-gui` всегда создаёт свой toplevel; единственный рендерер mux-клиента — сам wezterm. Эмпирически: mux-server держит pid-лок, singleton на юзера, наружу отдаёт только mux-протокол | S |
| A3 | XWayland + `XReparentWindow` в окно atlas | **тупик, доказан** | Окно atlas — Wayland-поверхность GTK, репарентить в неё нечего и некем; wezterm — не GTK (winit), GtkPlug-ить нельзя; эмпирика: wezterm под XWayland (`xwayland: true` в `hyprctl clients -j`) — отдельный toplevel, и даже хинты позиции/размера Hyprland игнорирует, пока окно тайлится | S |
| A4 | `get-text` снапшоты как read-only хвост в панели | **жизнеспособно** | Поллинг `get-text --pane-id N` по per-instance сокету: 34–54 мс на вызов (~32 мс среднее на 10 вызовов), скроллбэк читается (`--start-line -10` вернул уехавшие строки), свежесть = интервал поллинга | S |
| B1 | Собственное размещение wezterm (config/Lua) | **виable с оговорками: размер — да, позиция — нет (Wayland)** | `window_position` — **не существует** (WARN «not a valid Config field»); Lua `window:set_position()` с 2022 существует, но «has no effect on Wayland» — подтверждено живьём: вызов прошёл, окно легло в тайл [1727,1495]; `set_inner_size(600,400)` принят, но на тайловом окне WM затёр (итог 1701×1373) | S |
| B2 | Hyprland-контроль геометрии (док) | **жизнеспособно, основной путь** | `hyprctl clients -j` отдаёт `at`/`size`/`xwayland`/`floating` для всех окон, включая сами окна atlas; `movewindowpixel`/`resizewindowpixel` с `exact` и селектором `class:^(atlas-wezterm)$` — синтаксис сверен с wiki; floating обязателен → windowrule | M |
| B3 | Скрыть/показать | **жизнеспособно: special workspace** | Минимайза отпадает: **диспатчера minimize/unminimize в Hyprland нет**; `movetoworkspacesilent special:…` / `togglespecialworkspace` — есть; у atlas в `windows.rs` минимайзы нет и близко (там close+tab собственных панелей) | S |
| B4 | Фокус и z-order у дока | **viabe, есть нюансы** | `focuswindow` уже используется (`api_windows_focus`, прецедент); `alterzorder top` — но «нельзя увести floating за tiled»; floating всегда над тайлом — для дока это то, что нужно | S |
| B5 | Синк геометрии при ресайзе панели | **viabe, оценки внутри** | GTK на Wayland **не знает своей глобальной позиции** — но `hyprctl clients -j` знает (`at`); события движения — `.socket2.sock` (есть на боксе) или поллинг 1–2 с | M |

Сводно: **эмбед — нет; плотно управляемое окно wezterm в нашем хроме — да, и это недалеко от текущего кода** (`term.rs` уже держит выделенный экземпляр `--workspace atlas --class atlas-wezterm` с per-instance сокетом — это фундамент всего нижеследующего).

---

## 2. Тупики: доказательства

### A1. Mux как источник данных для xterm.js

`wezterm cli --help` (полный список, this build): `list, list-clients, proxy, tlscreds, move-pane-to-new-tab, split-pane, spawn, send-text, get-text, activate-pane(-direction), get-pane-direction, kill-pane, adjust-pane-size, activate-tab, set-tab-title, set-window-title, rename-workspace, zoom-pane`. Читает панель **только** `get-text` — one-shot снапшот (`--start-line/--end-line` адресуют скроллбэк, минус — назад от экрана). Стриминга нет. `cli proxy` («start rpc proxy pipe») — passthrough внутреннего mux-протокола (protobuf), который мызterm сам же и потребляет; в актуальной документации страницы proxy **нет** (в `docs/cli/cli/` — activate-pane…zoom-pane, proxy отсутствует), CLI-заголовок сам называет это «experimental mux server». Писать поверх недокументированного протокола кодек + эмулятор вывода = повторить wezterm-gui без гарантий стабильности между версиями. **Не наш путь.**

### A2. wezterm-mux-server / домены

`wezterm-mux-server` — отдельный бинарь, singleton на пользователя (эмпирически: второй инстанс падает с `unable to lock pid file … Resource temporarily unavailable`). Его клиенты — wezterm-gui/wezterm cli; рендерить в чужую Wayland/GTK-поверхность он не умеет по определению (см. §0). Сценарий «atlas рисует сам, а PTY живёт в mux» упирается в A1 — данных наружу нет. **Тупик.**

### A3. XWayland + XReparentWindow

Три независимых замка: (1) окно atlas — Wayland-поверхность, у X-сервера её нет, репарентить не во что; (2) WebKitGTK/Tauri не хостят нативные дети (XEmbed удалён вместе с NPAPI), а wezterm — winit-приложение, не GTK, т.е. и GtkSocket/GtkPlug (GTK3, X11-only, в GTK4 выпилены) ему не светят; (3) эмпирика: `env -u WAYLAND_DISPLAY wezterm …` реально поднял XWayland-клиент (`xwayland: true`) — но Hyprland его **затайлил**: запрошенные Lua `set_position(1400,200)`/`set_inner_size(600,400)` проигнорированы (итог `at [1727,1495] size [1701,1373]`). Плавающий XWayland-прецедент виден на живом боксе (окно Steam, float True, держит свою геометрию [4655,-754] 700×330), т.е. и в X11-режиме всё решает WM-флоат, как и для Wayland-окон. XWayland даёт только DPI-риск (fractional scaling, разные координаты). **Тупик, и гонять wezterm через X11 бессмысленно.**

### A4. get-text — хвост в панели (работает)

Замер (пробный экземпляр `--workspace atlas-probe`, адресация через `WEZTERM_UNIX_SOCKET`):

```sh
wezterm cli get-text --pane-id 0                # снапшот экрана, 34–54 мс
wezterm cli get-text --pane-id 0 --start-line -10   # уехавшие в скроллбэк строки — вернулись
```

10 вызовов подряд — 323 мс суммарно (~32 мс/вызов). Свежесть = период поллинга + один вызов; при 1–2 с это незаметно для «наблюдательного хвоста». Ограничения честно: это снапшот экрана эмулятора (фуллскрин-приложения между поллами выглядят застывшими), цветов нет без `--escapes`, и это **read-only** (ввод остаётся в окне wezterm — для превью в панели это ровно то, что нужно). Адресация панели-владельца — через `WEZTERM_UNIX_SOCKET` на per-instance сокет, как уже делает `term.rs` (см. ловушку в §4, тест 5).

---

## 3. B-кандидаты: что реально есть в этом билде

### B1. wezterm-native: размер — да, позиция — нет

- Конфиг-ключа `window_position` **не существует**: `wezterm --config window_position='{x=1,y=2}' ls-fonts` → `WARN … 'window_position' is not a valid Config field` (подсказки: `window_decorations`, `window_padding`, `window_close_confirmation`, `window_content_alignment`). Есть `initial_cols`/`initial_rows` (размер первого окна).
- Lua `Window`-объект (docs, this build): `focus, maximize, restore, set_inner_size, set_position, toggle_fullscreen, get_dimensions, …`. **`minimize` нет.** `set_position` документирован как «no effect on Wayland» — подтверждено живьём (см. §2 A3, тест Wayland-пробы: Lua ошибок нет, окно в тайле).
- `set_inner_size` на **флоат**-окне сработает (WM-флоат берёт размер клиента), на тайловом — затирается композитором (замер выше).
- Вывод: позицию окна wezterm сам на Wayland не задаст никогда → геометрию делает Hyprland (B2), размер можно либо оттуда же (`resizewindowpixel exact`), либо Lua. Одна точка правды — Hyprland.

### B2. Hyprland-контроль: точные команды, которые запустит приложение

Чтение (read-only, уже в `windows.rs::hyprctl_clients`): `hyprctl clients -j` → ищем `class == "atlas-wezterm"` → получаем `address`, `at`, `size`, `floating`, `workspace`, `monitor`.

Синтаксис диспатчеров (wiki Dispatchers, параметр `resizeparams` = `dx dy` | `W% H%` | `exact W H` | `exact W% H%`; селектор окна = босая регэкспа по классу или `class:…`/`initialclass:`/`title:`/`pid:`/`address:…`; wiki-пример: `resizewindowpixel 100 100,^(kitty)$`; **после запятой не должно быть ведущих пробелов** — hyprwm/Hyprland#7999):

```sh
# ДОК: точный прямоугольник (пиксели считаем из hyprctl clients/monitors)
hyprctl dispatch resizewindowpixel "exact 846 680,class:^(atlas-wezterm)$"
hyprctl dispatch movewindowpixel   "exact 2582 1495,class:^(atlas-wezterm)$"
# дешёвая монитор-агностичная альтернатива (в скобках — от экрана, глазомер владельца):
hyprctl dispatch resizewindowpixel "exact 25% 100%,class:^(atlas-wezterm)$"
hyprctl dispatch movewindowpixel   "exact 75% 0,class:^(atlas-wezterm)$"

# ФОКУС (прецедент уже в api_windows_focus):
hyprctl dispatch focuswindow "class:^(atlas-wezterm)$"

# Z-ORDER при доке внахлёст (floating и так выше тайла; поднять наверх стека):
hyprctl dispatch alterzorder "top,class:^(atlas-wezterm)$"
```

**windowrule — где живут и что ставить.** Hyprland 0.55+: hyprlang deprecated, конфиг — Lua (омарchy: `o.window(match, rules)` → `hl.window_rule`), владелец уже пользуется: `o.window({ title = ".*Hunt.*" }, { monitor = "DP-2" })` в `~/.config/hypr/hyprland.lua`. Туда же одна строка на атлас-терминал (свойства сверены с wiki Window-Rules 0.56):

```lua
-- ~/.config/hypr/hyprland.lua (владелец, один раз)
o.window({ class = "atlas-wezterm" }, {
  float = true,            -- без этого WM игнорирует точную геометрию (замер в §2/§3)
  border_size = 0,         -- в hyprlang-эпоху это был noborder
  no_shadow = true,
  no_initial_focus = false, -- окно не отбирает фокус при появлении
  -- по вкусу: monitor = "DP-1", workspace = "4 silent",
})
```

Правило — **владельческое, персистентное** (переживает рестарт compositor'а, версионируется его dotfiles). Приложение правила в рантайме **не трогает**: `hyprctl keyword` в Lua-эпоху — хрупкий и временный путь, отказываемся сознательно. Опыт плавающего окна Steam на этом боксе подтверждает: float-окно держит заданную геометрию.

Анти-flap как в `windows.rs` (`wait_window`): после каждого диспатча перечитываем `hyprctl clients -j` и отвечаем честно, подтвердилась ли геометрия/видимость, иначе `ok:false` + «повторите».

### B3. Скрыть/показать: special workspace, не minimize

Диспатчера `minimize`/`unminimize` в Hyprland **нет** (полная таблица диспатчеров сверена) — минимайз не управляем извне, отпадает. Рабочие схемы:

- **Док-режим (терминал прибит к панели):** спрятать = `movetoworkspacesilent "special:atlas-dock,class:^(atlas-wezterm)$"`, показать = `movetoworkspacesilent "<ws панели>,class:^(atlas-wezterm)$"` (номер ws берём из `hyprctl clients` панели) + при желании `focuswindow`. Окно живо, сессии в нём живы, переключений рабочего стола пользователь не видит.
- **Оверлей-режим («выездной» терминал):** `togglespecialworkspace atlas-dock` — дешёвый слайд с анимацией, но это переключение ws целиком (визуально «шторка») — выбрать владельцу.

Текущий `windows.rs` тут не конкурент, а подсказка: там свои панели сворачиваются close+tab внутри atlas; для чужого (wezterm) окна это неприменимо — только ws/диспатчи.

### B4. Фокус и z-order у дока

Floating всегда рендерится над тайлом — для дока внахлёст это нужное поведение, `alterzorder` не понадобится почти никогда (и не умеет увести floating за tiled — не пытаться). Известные грабли: (1) клик мимо дока уводит фокус — вернуть `focuswindow`; правило `stay_focused` **не** используем (залочит фокус); (2) чтобы maximize/fullscreen по случайному клику не ломали док — опционально в правиле `suppress_event = "maximize"`; (3) `focuswindow` по классу ловит **первое** совпадение — у atlas-экземпляра окно одно, но анти-флап перечитывает список, как в `api_windows_focus` (там уже логируется неоднозначность).

### B5. Синк геометрии

GTK-клиент Wayland своей глобальной позиции не знает — но `hyprctl clients -j` знает (`at`/`size` панели atlas видны в замере). Дизайн: прямоугольник дока = f(прямоугольник окна atlas, выбранная политика: правая половина / правая четверть / оверлей области «Терминал»), мульти-монитор — координаты монитора берём из `hyprctl monitors -j`, считаем локально для монитора панели. Триггеры: ресайз/перенос панели → а) подписка на `$XDG_RUNTIME_DIR/hypr/<sig>/.socket2.sock` (события `movewindow`/`resizewindow`/`openwindow` — сокет есть, проверено) — правильный путь, одна async-таска; б) поллинг 1–2 с пока док активен — дешёвый старт. Стоимость: **M** (матрица прямоугольников + слушатель + троттлинг диспатчей).

---

## 4. Протокол эмпирических тестов (воспроизведение)

1. `wezterm --config window_position='{x=1,y=2}' ls-fonts` → `WARN … not a valid Config field` (ключа нет).
2. Проба Wayland: `/tmp/wz-probe.lua` с `gui-startup` → `set_position(1400,200)` + `set_inner_size(600,400)`; `hyprctl clients -j` → `at [1727,1495] size [1701,1373]`, float False. Lua-лог: `PROBE … ok=true` (API есть, эффекта на тайле нет).
3. Проба XWayland: `env -u WAYLAND_DISPLAY wezterm …` → `xwayland: true`, всё та же тайловая геометрия.
4. `get-text`: латентность 34–54 мс, скроллбэк `--start-line -10` возвращает уехавшие строки, 10 вызовов = 323 мс.
5. Ловушка адресации: `wezterm cli list` без `WEZTERM_UNIX_SOCKET`/`--class` нашёл **личный** экземпляр владельца (`ws default`) — ровно то, от чего `term.rs` защищается маркером workspace + явным сокетом. Мёртвый экземпляр оставляет **протухший** `gui-sock-<pid>` файл, а cli без `--no-auto-start` пытается поднять `wezterm-mux-server` (лог: `unable to lock pid file`) — обоснование текущего probe-then-trust в `atlas_wezterm_instance`.
6. Уборка: `pkill -f atlas-probe`, протухший сокет удалён; личный экземпляр владельца (`gui-sock-958531`) не тронут.

---

## 5. Рекомендуемый поэтапный дизайн

### Этап A — «управление» (S–M): док/андок, показать/спрятать, фокус

1. Владелец добавляет windowrule (§B2, 4 строки в `hyprland.lua`) — единственный ручной шаг.
2. В `windows.rs` (или новый `term_dock.rs` рядом) — то же прицельное хозяйство, что уже есть для фокуса: `hyprctl clients -j` → адресат `class:^(atlas-wezterm)$` → `movewindowpixel`/`resizewindowpixel`/`movetoworkspacesilent`/`focuswindow` → перечитали состояние → ответ. Политики дока (`right-half`, `right-quarter`, `overlay-terminal`) — прямоугольник из окна atlas + `hyprctl monitors -j`.
3. Панель «Терминал»: кнопки «Док / Из дока / Спрятать / Показать / Фокус» поверх существующего «↗»; состояние дока (в доке, ws, прямоугольник) — в реестре сервера.
4. Спрятать = special workspace (§B3) — сессии живут, экземпляр не гасится. Кнопка закрытия окна по-прежнему = «экземпляр упал, поднимем при следующем ↗» (текущее поведение).

### Этап B — «хром» (M): док впритык и опциональный титлбар atlas

1. Спавн экземпляра дополняется пер-инвокационными оверрайдами (user's `wezterm.lua` не трогаем): `--config window_decorations=RESIZE` (без титлбара, края живые; `NONE` — если владелец захочет совсем голое) `--config hide_tab_bar_if_only_one_tab=true`.
2. Док впритык: точные `movewindowpixel`/`resizewindowpixel`; на глаз проверить швы (внутренние гэпы Hyprland добавляют пиксели даже float-ам — компенсировать в расчёте прямоугольника; это пункт «глазами владельца»).
3. Титлбар atlas — при желании: узкая кнопочная полоса в панели, визуально сросшаяся с краем дока; рисуется в webview, отдельной родни с окном нет — «наш хром» остаётся геометрией, это осознанный предел.
4. Синк при ресайзе панели (§B5): сначала поллинг 1–2 с (S), потом `.socket2.sock` (M).

### Этап C — «превью» (S–M): хвост сессии в панели через get-text

1. Для каждой сессии-чипа с живым wezterm-пейном: раз в 1–2 с `get-text --pane-id` через `WEZTERM_UNIX_SOCKET`; отрисовка в `<pre>` (моноширинный хвост, ~40 мс на вызов — копейки).
2. Честные границы: снапшот без цветов/курсора, фуллскрин-сессии выглядят ступенчато; ввод только в окне wezterm.
3. Альтернатива «дешёвый стриминг» (спавн сессии под `script -f лог` и tail) — возможна, но размазывает владение PTY; откладываем сознательно, get-text закрывает наблюдение.

### Fallback

Текущее поведение (вкладки в выделенном окне, `POST /api/terminal/{id}/open`) остаётся режимом «андок»: док — это оверлей-состояние того же экземпляра, а не второй терминал. Потеря окна = потеря сессий (сегодняшняя семантика сохраняется; персистентность через unix-домены mux — отдельная история, не в этой задаче).

### Риски и «глаза владельца»

- windowrule — ручной, владельческий (Lua-синтаксис 0.55+ едет вместе с omarchy; застарелый hyprlang-вариант `windowrule = float, class:…` устарел).
- Примерка диспатчей (точные пиксели, `%`-форма, швы гэпов, поведение `togglespecialworkspace` как «шторки») — только глазами владельца: гвард запрещает агенту `hyprctl dispatch/keyword`.
- Дрейф API: Lua-имена правил (`border_size`, `no_shadow`) — свежий синтаксис; при апгрейде Hyprland их могло бы повести, но omarchy догоняет апстрим сам.
- `focuswindow` по классу ловит первое совпадение — держим «одно окно на экземпляр» (сегодняшняя модель term.rs).

**Итог:** заэмбедить — нельзя (протокол, не wezterm). Плотно надеть наш хром на выделенное окно wezterm — можно уже сейчас, и это на 80 % переиспользование прецедентов `windows.rs` + spawn-argv из `term.rs`.
