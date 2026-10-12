# Встраивание терминала в окно atlas (feasibility spike)

Вопрос: можно ли «засунуть wezterm с herdr внутрь окна atlas», чтобы оркестратор
жил в atlas и workflow был цельным из коробки. Проверено на живой машине
(Omarchy/Hyprland 0.56.2/Wayland, herdr 0.9.3, wezterm 20260716-195552-76b606ec):
кли `--help` обоих терминалов, исходники и доки herdr (v0.9.3), схема socket-API
herdr (`herdr api schema --json`, protocol 22), `src-tauri/src/orch.rs` и orch/focus
секции `http.rs`, справочник dispatchers Hyprland (плюс сверка имён с установленным
бинарником). Изменений в `src-tauri/`, `static/`, `schema.sql` нет — только этот
документ и скрипты `tools/orch-group.sh`, `tools/orch-spawn.sh` (оба по умолчанию
dry-run; mutating-путь исполнен только через mock-двойки `hyprctl`/`wezterm`, живая
сессия не трогалась).

## Вердикт

Вживить чужое нативное окно (wezterm или herdr) внутрь окна atlas на Wayland
сегодня нельзя — ни один протокол в стеке этого не умеет, а единственная лазейка
(XEmbed/GtkSocket через XWayland) требует, чтобы встраиваемый терминал умел
XEmbed, чего нет ни у wezterm, ни у herdr, и на практике рассыпается на фокусе,
ресайзах и масштабировании. Рабочая связка из существующих компонентов выглядит
так: **сейчас** — Hyprland-группа: atlas-окно и окно orchestrator складываются в
один кадр с вкладками (это ровно «цельное окно» на вид, ноль изменений кода,
скрипт `tools/orch-group.sh` готов), **среднесрочно** — нативная панель
оркестратора в самом atlas: серверный фундамент (`POST /api/orch`, персистентный
`omp --mode rpc`) уже работает и покрыт тестами, не хватает только фронтенда и
нескольких серверных дыр (апрувалы, история, отмена) — оценка M.

## Путь А. Вживить терминал в окно atlas — сегодня невозможно

### Wayland: протокола встраивания нет

На Wayland клиент не может ни отрисовать чужую поверхность внутри своего окна,
ни даже задать позицию собственного. Прямое свидетельство из `wezterm start --help`:

```
--position <POSITION>
      ...
      Note that Wayland does not allow applications to control window positioning.
```

Встраивание потребовало бы композиторного протокола «reparent чужого toplevel в
моё окно» — такого нет ни в core-протоколе (xdg_shell создаёт только независимые
toplevel'ы), ни в wayland-protocols. Ближайшее, что есть у Hyprland
(`hyprland-protocols`, 9 протоколов) — `hyprland_toplevel_mapping_v1`, и это
только справочник, не встраивание:

```xml
<description summary="mapping of toplevels to windows">
    This protocol allows clients to retrieve the mapping of toplevels to
    hyprland window addresses.
</description>
```

Остальные — capture/export (`toplevel-export`), инпут-граб, shortcuts: единство
«на уровне пикселей» они не дают.

### herdr: публичный API — управляющий, не экранный

herdr — это tmux-подобный клиент-сервер: headless-сервер (`herdr server`, здесь
PID 3083) держит PTY и состояние, а TUI-клиент рисует всё внутри **чужого**
терминала. Собственных окон у herdr нет вообще: `hyprctl clients -j` не содержит
ни одного окна herdr, процесс-клиент — обычная CLI-программа на pts/8 внутри
вкладки wezterm (не quickshell — единственный quickshell на машине принадлежит
omarchy-shell). Отсюда два следствия:

1. «Хостить herdr-панель» в atlas = написать ещё один клиент herdr, включая
   экран/инпут-кодеки. Они приватные и не для третьих лиц — из официальной доки
   (`docs/socket-api.mdx`, v0.9.3):

   > The client-rendered Herdr UI uses a stable endpoint generation for local
   > and SSH servers. … The numbered binary protocol remains for same-install
   > and internal operations, including direct terminal attach and live handoff.

   `herdr terminal attach <TERMINAL_ID>` — сырой поток терминала — принадлежит
   именно этому внутреннему бинарному протоколу; `client_shell.surface.set`
   (`"Updates whether the requesting client shell receives and controls pane
   presentation"`) — рукопожатие штатного клиента.

2. Публичный socket-API (JSON, protocol 22) — управляющий: `pane.read` (текст),
   `pane.send_text/send_keys`, `events.subscribe`, `agent.*`, `layout.*`. Заброшенный
   пользователем `herdctl` (~/, Python/Textual) — живое доказательство: это
   контрольная панель *над* herdr, а не его второй экран; панель из `pane.read`
   будет текстовым слепком, а не терминалом (omp-TUI в ней не отрисуется).

### X11-лазейка: XEmbed через XWayland — не рекомендуется

Классический путь: контейнер с GtkSocket + встраиваемое приложение как GtkPlug
(протокол XEmbed). Для atlas стек Tauri v2 = GTK3 (`Cargo.lock`: gtk 0.18.2, tao
0.37.1, wry 0.57.0 — gtk3, а не gtk4), то есть GtkSocket формально доступен. Но:

- ни wezterm, ни herdr не реализуют XEmbed-сторону (ни `wezterm --help`, ни доки
  не упоминают plug/socket/reparent) — останется «глотание» через сырой
  XReparentWindow, с ручным согласованием ресайзов и известными поломками фокуса;
- терминал придётся гнать под XWayland (wezterm умеет X11-фронтенд) — теряем
  Wayland-нативное масштабирование/IME, получаем второй графический стек;
- при каждом обновлении GTK/tao риск тихой деградации. Цена владения не
  сопоставима с выигрышем.

Частный случай «второй вьювер той же панели»: mux у wezterm позволяет двигать
панели (`wezterm cli move-pane-to-new-tab --pane-id N --window-id M`) и открывать
ту же панель из двух окон — но это по-прежнему отдельное окно, а не встраивание.

## Путь Б. Hyprland-группа — рекомендовано сейчас

Группа в Hyprland — контейнер-вкладки в одном кадре, «A group is like i3wm's
"tabbed" container. It takes the space of one window» (wiki, Configuring →
Dispatchers). Это ровно «цельное окно» без встраивания: atlas-панель и терминал
оркестратора выглядят как одно окно с ярлычками. Все нужные диспатчеры есть в
установленном Hyprland 0.56.2 (сверено по бинарнику) и описаны в wiki:

| Dispatcher | Семантика (wiki 0.54) |
|---|---|
| `togglegroup` | «toggles the current active window into a group» |
| `moveintogroup <dir>` | «Moves the active window into a group in a specified direction. No-op if there is no group in the specified direction» |
| `moveoutofgroup [win]` | «Moves the active window out of a group … or window for a specific window» |
| `changegroupactive f\|b\|N` | «switches to the next window in a group» |

Рецепт из четырёх команд (окна ищутся по классу `atlas` и заголовку
`atlas-orchestrator` — ровно та метка, которую резервирует O-кнопка atlas через
`wezterm cli set-tab-title`, см. README «Фокус»):

```sh
hyprctl dispatch focuswindow address:0xATLAS   # фокус на atlas-окно
hyprctl dispatch togglegroup                   # atlas становится группой
hyprctl dispatch focuswindow address:0xORCH    # фокус на окно orchestrator
hyprctl dispatch moveintogroup l|r|u|d         # вкладываем в группу рядом
```

`tools/orch-group.sh` делает это и обратное: по умолчанию — dry-run (печатает
план с адресами и направлением), `--apply` — исполняет, `--ungroup` — вынимает
orchestrator и распускает группу из двух окон. Ограничения честные: окна должны
быть на одном воркспейсе (иначе подсказывает готовую команду
`movetoworkspacesilent`), направление считается по центрам окон (доминирующая
ось), при apply фокус мигнёт — иначе никак: группировка в Hyprland работает через
фокус. Группы с плавающими окнами поддерживаются (тесты Hyprland
`hyprtester/src/tests/main/groups.cpp` проверяют `floating: 1` в группе), но
живые atlas-окна сейчас тайловые (`floating: false` в `hyprctl clients -j`), так
что штатный случай — тайл.

Дальше эту связку можно автоматизировать в самом atlas: спавнить orchestrator
отдельным экземпляром wezterm с другим app_id (`wezterm start --class` — «Under
Wayland this changes the app_id») и вешать windowrule `group` — но это уже код в
`src-tauri/`, за рамками спайка.

## Путь В. Нативная панель оркестратора — среднесрочно, оценка M

Что уже работает сегодня (`src-tauri/src/orch.rs`, `http.rs:995-1046`):

- персистентный ребёнок: один `omp --mode rpc -c --cwd <atlas> --append-system-prompt
  "You are the atlas orchestrator…"` на процесс сервера, ленивый рестарт при
  смерти канала (`ensure()`/respawn-once в `prompt()`);
- `POST /api/orch {"message": str}` → SSE-поток; повороты сериализуются гейтом
  (одно сообщение за раз), таймаут поворота 240 с (`TURN_TIMEOUT_SECS`), при
  простое — keepalive-событие `thinking` раз в 2 с;
- события, которые сервер выделяет из RPC-потока (`route()`): `text` (дельты
  ответа), `tool` (имя запущенного инструмента), `thinking`, `done`,
  `proc_exit`. Плюс O-кнопка (`/api/focus`, target `orchestrator`) умеет
  найти/создать wezterm-окно orchestrator с зарезервированным заголовком.

Чего нет (и что нужно панели): собственно UI (в `static/` нет ни одного
обращения к `/api/orch`); апрувалы — `route()` молча отбрасывает все кадры
разрешений инструментов, а omp без ответа на запрос стоит до таймаута поворота
(240 с); детали инструментов — только имя, без аргументов и результатов; история
разговора — нет GET-эндпоинта, перезагрузка страницы теряет транскрипт; отмена
поворота — нет (`/stop` из herdctl-README здесь нечему отправлять); сессии —
ровно одна нить на сервер (`-c` продолжает последнюю сессию atlas), никаких
`/new` и `/model`; устойчивость — `spawn()` делает `.expect("spawn omp rpc")`, т.е.
отсутствие бинаря omp роняет таск-воркер, а не отвечает ошибкой. Самое рискованное
неизвестное — формат кадров апрувалов в `omp --mode rpc` (нужен реверс протокола
или yolo-режим прав); второе — поведение broadcast при медленном SSE-потребителе
(`Ok(Err(_lagged)) => continue` — события теряются молча).

Размер: эхо-панель (ввод + рендер `text|tool|thinking|done`) — S-M (день-два,
чисто фронтенд); полезная панель (markdown, таймлайн инструментов, ошибки
процесса, история в БД) — M; апрувалы — L с непредсказуемой частью. Ориентир
архитектуры уже лежит в репо: `~/Projects/herdctl` показывает ровно такой
«панельный» клиент над API (herdr socket API + omp session cast).

## Демо для человека (что вы увидите)

```sh
cd ~/Projects/atlas

tools/orch-spawn.sh               # dry-run: покажет план (или найдёт существующую панель)
tools/orch-spawn.sh --apply       # откроет НОВОЕ окно wezterm с omp -c и заголовком вкладки atlas-orchestrator

tools/orch-group.sh               # dry-run: печатает найденные окна, направление и 4 dispatch-команды
tools/orch-group.sh --apply       # atlas-окно и окно orchestrator сольются в один кадр с вкладками;
                                  #   переключение вкладок: клик по ярлычку или
                                  #   hyprctl dispatch changegroupactive f|b
tools/orch-group.sh --ungroup --apply   # вернуть как было
```

Ожидаемый вид: одно окно-рамка с tab-баром (ярлыки `atlas — …` и
`atlas-orchestrator`), Alt+клик/ярлычки переключают; закрытие вкладки
orchestrator не задевает atlas. Откат — `--ungroup`. Скрипты отказываются трогать
что-либо, кроме окон, найденных по классу `atlas` / заголовку
`atlas-orchestrator`, и не выполняют ни одной команды в dry-run.

## Риски и неизвестные

- Группировка меняет фокус (диспатчеры работают через фокус) — на живой сессии
  это видимая, но краткая помеха; dry-run показывает план заранее.
- Направление `moveintogroup` — эвристика по центрам окон; при экзотической
  раскладке Hyprland может выбрать не ту сторону — операция повторяемая
  (`--ungroup`).
- Метка orchestrator держится на заголовке вкладки wezterm: промпт в shell может
  перезаписать title панели, но не tab_title, который ставит atlas/O-скрипт.
- Путь А остаётся закрытым, пока в Wayland/xdg-протоколах не появится
  встраивание, а в wezterm/herdr — поддержка соответствующей роли.
- Для пути В главное неизвестное — протокол апрувалов `omp --mode rpc`; без него
  панель будет умирать на первом запросе прав (или нужно осознанно включить
  auto-approve).

## Бонус: другие терминалы

Установлены: wezterm (git-сборка), foot 1.28.0, herdr 0.8.2 (пакет) / 0.9.3
(`~/.local/bin`); kitty и alacritty отсутствуют. Ни у одного из установленных нет
истории встраивания на Wayland: `foot --server` — это только быстрый старт
клиентов (отдельные окна, никакого embed), kitty собирал бы `--single-instance`
(переиспользование процесса — опять же отдельные окна). «Swallow»-эффекты в
тайлинге (закрыть/спрятать окно под другое) — это композиторные трюки, не
встраивание, и в Hyprland без них жить проще через группы.
