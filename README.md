# ⛔ ЗАМОРОЖЕН 2026-10-14

atlas закрыт и заархивирован (тег `archive-2026-10-14`). Наследник — `~/Projects/atlas-v2` (агент-канбан на чужом ядре agent-kanban, борд на `http://127.0.0.1:7777`). Данные перенесены: `scripts/import_atlas.py`. Этот репозиторий и его сервисы (`atlas.service`, `atlas-dev.service`) выключены; копии БД и прогонов — в `archive/`.

---

# atlas

Control panel for daily work: **projects × sessions × vault**. Five always-on
desktop windows (Projects, Board, Dependencies, Search, Sources) over one HTTP
control plane, backed by a single SQLite database.

## Architecture

One Rust binary (`src-tauri`), nothing else at runtime:

- **axum** HTTP server on `127.0.0.1:8787` (embedded static UI via rust-embed +
  Tauri `frontendDist: ../static` — plain HTML/CSS/JS, no node build step).
  Frontend is plain browser ES modules (no bundler): `static/app.js` is a thin
  entry (boot + key handlers + transitional `window` shim — inline HTML
  handlers will be converted later, until then expose them there); the code
  lives in `static/js/<domain>.js` (core, projects, board, deps, search,
  sources, setview, herdr, view, terminal, stt, orchestrator) with shared
  helpers in `static/js/lib/` (dom, api, menu, modal, ticker, smooth, fmt).
- **Tauri v2** GUI: в `tauri.conf.json` объявлено ровно одно окно — host
  `projects`; остальные панели (`board`, `deps`, `search`, `sources`, `herdr`,
  `view`, `terminal`, `orchestrator`) процесс создаёт сам при старте — только
  те, что НЕ свёрнуты вкладками в `ui.tabs` (определения — таблица
  `SYNTH_WINDOWS` в `gui.rs`). Итого девять панелей-меток: `projects`, `board`,
  `deps`, `search`, `sources`, `herdr`, `view`, `terminal`, `orchestrator`;
  все, кроме host, сворачиваются во вкладки хоста `projects`. Панели `settings`
  нет — «Настройки» открываются
  внутри того окна, где нажата кнопка. Окна общаются только с локальным
  сервером
  (`connect-src 'self' http://127.0.0.1:8787 ws://127.0.0.1:8787`), а
  «Просмотр» встраивает локальные веб-приложения
  (`frame-src 'self' http://127.0.0.1:* http://localhost:*`).
- **SQLite** database at `~/.local/share/atlas/control.db` (WAL mode).
- Logs: `~/Projects/atlas/atlas-rs.log` (file sink active with `--serve-only`;
  stderr otherwise).

## Build

```sh
cd src-tauri && cargo build --release
```

No npm, no Python, no tauri-cli needed. Binary lands at
`src-tauri/target/release/atlas`.

## Run

```sh
atlas                 # GUI + server on 127.0.0.1:8787 (в одном процессе)
atlas --serve-only    # headless server (systemd unit runs this)
```

| Flag | Meaning |
|---|---|
| `--serve-only` | run the HTTP server without windows (default port 8787) |
| `--port N` | HTTP port (default 8787) |
| `--db PATH` | SQLite path (default `~/.local/share/atlas/control.db`) |

### CLI-команды

```sh
atlas help
atlas notify "текст" [--project P] [--level info|warn|err] [--url http://127.0.0.1:8787]
atlas board add "<title>" [--project P] [--quadrant q1..q4] [--description "..."] [--dedupe-key KEY]
atlas board list [--quadrant qN] [--json]
atlas board status [--json]
```

**Контракт кодов выхода — стандарт для ВСЕХ CLI-глаголов atlas:** `0` —
успех · `1` — ошибка исполнения (сервер ответил ошибкой/неожиданным телом) ·
`2` — ошибка использования (парсер отвергает до любого I/O; серверные флаги с
командами не совмещаются) · `3` — сервер недостижим. Любой отказ печатает
одну именованную строку в stderr — никакого молчаливого успеха и локальной
очереди.

`notify` — чистый HTTP-клиент к работающему серверу (никакой БД, никаких
окон): печатает `id=N unread=M` и выходит с 0 (исторические коды notify —
0/1/2; соединение тоже 1). `board add` печатает `id=N`, а с `--dedupe-key`
при уже существующем деле с тем же ключом — `id=N deduped` и код 0
(идемпотентный повтор: делегированная задача, добавленная дважды, не
дублируется; ключ хранится в `items.dedupe_key` и виден в `GET /api/board`).
Недопустимый квадрант/флаг → код 2 ещё до сети; сервер недоступен → код 3.
`board list` — таблица (id, квадрант, заголовок, проект, done, шаги, файлы)
или `--json`-массив дел; `board status` — счётчики по квадрантам + закрытые
дела + открытые шаги (`{"q1":…,"done":…,"open_steps":…}` в `--json`).
Удобно оркестратору: `atlas notify "сборка упала" --level warn --project
atlas`, постановка задачи — `atlas board add "… " --project atlas
--dedupe-key <slug>`.

На PATH `atlas` — это симлинк `~/.local/bin/atlas` →
`~/Projects/atlas/src-tauri/target/release/atlas` (создаётся один раз вручную),
поэтому `atlas notify …` работает из любого каталога, в том числе из сессии
оркестратора. Если в `~/.local/bin` уже лежит настоящий файл `atlas` — симлинк
не перезаписывается.

### Один сервер на порт (GUI против systemd-юнита)

Инвариант: процесс, владеющий портом 8787, должен владеть и окнами — иначе
операции окон (`/api/windows/*`) из GUI навсегда отвечают «GUI не запущен».
Поэтому:

- при старте GUI, если порт занят, процесс останавливает
  `atlas.service` (`systemctl --user stop`, ожидание до 5 с) и перепривязывает
  порт (до 2 попыток с паузой ~1 с); не получилось — GUI работает против
  чужого бэкенда, и в логе прямо сказано, что оконные операции из этого
  процесса работать не будут;
- `GET /api/ui` честно сообщает: `"gui": true|false` (в этом процессе есть
  настоящие окна) и `"port_owner": "self"|"other"` (этот процесс занял порт
  или работает поверх чужого сервера). Фронтенд по этим полям отключает
  кнопки окон с внятным объяснением;
- ярлык запускает обёртку `~/.local/bin/atlas-gui`: она останавливает юнит,
  запускает бинарник, а после выхода GUI снова поднимает
  `atlas.service` — headless-сервер возвращается автоматически.

### Один экземпляр GUI (single instance)

`tauri-plugin-single-instance` инициализируется **первым** плагином в
builder. Второй запуск `atlas` (GUI) не создаёт ни одного окна и не остаётся
висеть: плагин пересылает аргументы первому экземпляру и немедленно завершает
второй процесс. На стороне первого экземпляра колбэк только поднимает его окно
(host `projects`, а если пользователь его закрыл — любую живую панель);
больше колбэк ничего не делает. Раньше каждый запуск программно открывал весь
свой набор панелей, поэтому два запуска означали две группы окон `atlas` на
рабочем столе.

Обёртка `atlas-gui` со всем этим согласована: она останавливает
`atlas.service` ДО запуска GUI — это обязательное условие, иначе headless-юнит
(тот же бинарник с `--serve-only`) первым захватит single-instance-замок, и
GUI-запуск завершится вторым экземпляром, не открыв ни окна. Если второй GUI
всё же стартует (например, юнит успел подняться между остановкой и запуском),
он выходит мгновенно, и обёртка после выхода перезапускает сервис — цикл
«остановил → GUI → поднял» сохраняется. Прямой запуск бинарника при живом
юните просто закроется, не тронув ничего.

## Конфигурация

Необязательный файл `~/.config/atlas/config.toml` (или
`$XDG_CONFIG_HOME/atlas/config.toml`, если переменная задана абсолютным
путём) переопределяет известные пути и внешние программы. Файла нет — работают
значения по умолчанию (ровно то, что зашито в коде); файл битый (опечатка в
ключе, кривой TOML) — в лог уходит warning, и сервер стартует на значениях по
умолчанию, никогда не падая из-за конфига. Значения вида `~/…` разворачиваются
в домашний каталог. Файл читается один раз при старте; `atlas --help`
напоминает о нём.

| Ключ | По умолчанию | За что отвечает |
|---|---|---|
| `paths.projects` | `~/Projects` | корень git-репозиториев (источник `git`, `POST /api/projects/create`) |
| `paths.vault` | `~/vault` | корень Obsidian-vault (источник `vault`) |
| `paths.vault_projects_subdir` | `Проекты` | каталог проектов внутри vault |
| `paths.vault_areas_subdir` | `Области` | каталог областей внутри vault |
| `paths.vault_inbox_subdir` | `Инбокс` | каталог инбокса внутри vault |
| `paths.data` | `~/.local/share/atlas` | состояние atlas: `control.db`, сессии оркестратора/делегаций, кэш fastembed, вставки буфера |
| `paths.omp_sessions` | `~/.omp/agent/sessions` | логи сессий omp (источник `omp`) |
| `paths.memory_db` | `~/.omp/agent/memories/mnemopi/mnemopi.db` | база mnemopi (источник `memory`) |
| `paths.worktrees` | `~/.atlas-wt` | корень worktree-делегаций («Этап B») |
| `paths.theme` | `~/.local/state/omarchy/current/theme/colors.toml` | тема Omarchy для палитры панелей |
| `paths.zen_script` | `~/Projects/ai/zen-tabs/bin/spaces.mjs` | скрипт zen-tabs (источник `zen`) |
| `paths.voxtype_config` | `~/.config/voxtype/config.toml` | конфиг voxtype (голосовой ввод) |
| `binaries.omp` | `omp` | CLI omp: оркестратор, делегации (по умолчанию сначала пробуется `~/.local/bin/omp`) |
| `binaries.herdr` | `herdr` | CLI herdr: фокус H/T, пульт (по умолчанию сначала пробуется `~/.local/bin/herdr`) |
| `binaries.bun` | `bun` | рантайм скрипта zen-tabs (по умолчанию сначала пробуется mise-путь) |
| `binaries.opener` | `gio` | лаунчер «открыть файл» (`<opener> open <path>`; нет на PATH → `xdg-open`) |
| `binaries.voxtype` | `voxtype` | локальная расшифровка STT |
| `binaries.pw_record` | `pw-record` | запись микрофона STT |
| `delegation.model` | *(пусто)* | модель, закреплённая за воркерами делегаций (argv `--model`); пусто — флаг не передаётся, действует дефолт omp |
| `delegation.run_in` | `"systemd"` | где живёт воркер `omp -p` делегации: `"systemd"` — транзиентный юнит `atlas-deleg-*` (как раньше); `"herdr"` — отдельное рабочее пространство/панель herdr с тем же ярлыком: сырой вывод воркера виден вживую в терминале (и в мобильном relay), отмена — ctrl+c и закрытие панели. rpc-ребёнок orchestrator-цели всегда идёт через systemd/direct — его stdio разговаривает с atlas |
| `delegation.silence_warn_secs` | `300` | порог предупреждения о тишине: поллер делегации (раз в ~30 с) уведомляет владельца ОДИН раз за прогон, когда rpc-ребёнок столько не выдаёт кадров («возможно, долгий thinking»); `0` — никогда не предупреждать. Таймеры ход НЕ убивают: долгая, но полезная операция не умирает от молчания, отменить — вручную |

Для программ значением может быть имя (ищется на PATH) или абсолютный путь
(используется как есть, без проверки существования — ошибка всплывает громко
при спавне). Пример:

```toml
[paths]
projects = "~/git"
worktrees = "/tmp/atlas-wt"

[binaries]
opener = "/usr/bin/kde-open"
```

Производные файлы отдельными ключами не настраиваются: `control.db`,
`atlas-orchestrator.jsonl`, `fastembed/`, `paste/` всегда живут внутри
`paths.data`, а макет vault — это три имени подкаталогов внутри `paths.vault`.

Флаг `atlas --config PATH` подменяет сам файл конфигурации (по умолчанию —
`~/.config/atlas/config.toml`); без флага поведение не меняется. На нём стоит
изоляция разработки ниже.

## Изоляция разработки

У atlas два инстанса на этой машине. Живой — то, чем пользуется владелец:
GUI-процесс (панели) или headless-юнит `atlas.service` на **8787**. Дев —
отдельный headless-инстанс для агентов на **18787**: свой конфиг
`~/.config/atlas/config-dev.toml` (единственная настройка —
`paths.data = ~/.local/share/atlas-dev`, остальное наследует дефолты, так что
дев отличается от живого только тем, куда пишет), свой юнит
`~/.config/systemd/user/atlas-dev.service`
(`atlas --serve-only --port 18787 --config …/config-dev.toml`,
`Restart=on-failure`), своя база `~/.local/share/atlas-dev/control.db`.

Правила:

- **агенты тестируют только на 18787; живой GUI (8787) не трогают**;
- обновление живого — только `~/.local/bin/atlas-update`, и запускает его
  владелец (или агент с его явного согласия): скрипт собирает release,
  аккуратно гасит headless-юнит и GUI, перезапускает GUI через `atlas-gui`
  и проверяет `gui:true port_owner:self`; дев-инстанс после пересборки
  просто перезапускают: `systemctl --user restart atlas-dev.service`;
- быстрый контроль здоровья дева: `curl 127.0.0.1:18787/api/ui`
  (ожидаются `"gui":false`, `port_owner":"self"`).

Почему не docker: проверяемая поверхность atlas — это HTTP API **плюс**
интеграция с Hyprland/wezterm на конкретной машине. Контейнер не отдаст GUI-панели
в Wayland-сессию пользователя и не имеет того самого `hyprctl`/`wezterm` на
хосте — оконные и фокус-фичи в контейнере непроверяемы в принципе. Второй
`--serve-only`-инстанс на своём порту с своим каталогом данных даёт изоляцию
ровно там, где она нужна: разные порты, разные данные, общая машина.

## Autostart (server)

`~/.config/systemd/user/atlas.service` starts the headless server at login:

```sh
systemctl --user daemon-reload
systemctl --user enable --now atlas.service
```

`Restart=on-failure`, `RestartSec=3`. The GUI is not autostarted — launch it
from the desktop entry (`atlas`, `~/.local/share/applications/atlas.desktop`).

Stop/restart the server only via `systemctl --user stop|restart atlas`.
`pkill -x atlas` is not a stop-procedure: the GUI and the headless
`--serve-only` server share one binary name, so a name-targeted pkill takes
both down (a clean SIGTERM is not restarted by `Restart=on-failure`).

## Hyprland rules

`docs/hyprland-atlas.lua` holds the Lua-form rule
`o.window("atlas", { float = true })`. This Omarchy setup has no classic
`hyprland.conf`/`conf.d` — the config is `~/.config/hypr/hyprland.lua` with
personal modules (`require("hypr.monitors")`, `hypr.looknfeel`, …), so put the
line into one of those personal modules (or a new `require`-ed file), e.g.
`~/.config/hypr/looknfeel.lua`.

## API

All endpoints on `http://127.0.0.1:8787`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/theme` | current Omarchy theme colors |
| GET | `/api/theme/stream` | SSE-push темы: палитра при подключении и на каждое изменение (event `theme`), keep-alive каждые 20 с |
| GET | `/api/projects` | project list (with `archived`/`favorite` flags and `deps`; избранные первыми) |
| POST | `/api/projects/create` | создать проект: каталог + `git init` + README-заглушка + строка в БД (`422` за имя/parent, `409` за занятые) |
| POST | `/api/projects/archive` | archive/unarchive a project (flag only, nothing is deleted) |
| POST | `/api/projects/favorite` | добавить проект в избранное / убрать: `{"name","favorite"}` → `{"ok","name","favorite"}` (флаг, 404 за незнакомый проект, 422 за не-булево `favorite`) |
| POST | `/api/projects/{name}/sources/{kind}/create` | досоздать источник: `herdr` (воркспейс без фокуса), `git` (`git init`), `vault` (каталог в `~/vault/Проекты/<имя>`); `omp`/`zen` → 422 «источник появится позже» |
| GET | `/api/session` | agent session view |
| GET | `/api/session/stream` | live session event stream (SSE) |
| GET | `/api/board` | kanban board |
| POST | `/api/board/item` | create board item (с необязательными `description` и `dedupe_key`) |
| PATCH | `/api/board/item/{id}` | update board item (`title`/`quadrant`/`done`/`description`; `done:true` = закрыто) |
| PUT | `/api/board/item/{id}/files` | заменить набор прикреплённых файлов дела: `{"paths":[…]}` (существующие файлы, ≤20, 422 со списком нарушений) |
| PUT | `/api/board/item/{id}/steps` | заменить чек-лист дела («Этап B»): `{"steps":[{"text","done"?}]}` — trim, ≤50 символов, ≤20 шагов, 404/422 |
| POST | `/api/board/item/{id}/delegate/preview` | «Этап B»: скомпилировать бриф делегации, ничего не запуская → `{"ok","brief","cwd","resume"}` (`resume` — сессия прошлого прогона этого дела, которую продолжит follow-up, или `null` — свежая) |
| POST | `/api/board/item/{id}/delegate` | делегировать дело: `{"target":"orchestrator"\|"worktree"\|"dir"}` → `{"ok","run_id","state","ref"}` (409/422/404) |
| GET | `/api/board/item/{id}/run` | состояние прогона делегации + хвост кольца + итог воркера (`state`,`lines`,`exit`,`started`,`duration_s`,`truncated`,`final`) |
| POST | `/api/board/item/{id}/delegate/cancel` | остановить воркера (`systemctl stop` юнита или SIGTERM; SIGKILL через 5 с) или прервать ход → `failed` (409, если пусто) |
| POST | `/api/board/item/{id}/delegate/reset` | вернуть дело в бэклог: стереть все `delegation_*` поля до исходных (на доске снова `delegation:null`); `running` → 409 — сначала отмена (404, если нет дела) |
| GET | `/api/board/item/{id}/diff` | `git -C <ref> diff HEAD` + `--stat` прогона (обрезка 400 строк / 200 kB; `ok:false` вне git) |
| POST | `/api/board/item/{id}/accept` | приём на ревью: `{"merge":bool}` → `done` (конфликт слияния → `ok:false` + `review`) |
| POST | `/api/board/item/{id}/reject` | вернуть дело в работу: `{"feedback":"…"}` → `queued` + фидбек в следующий бриф |
| GET | `/api/project/{name}/files` | автодополнение путей: `?q=<подстрока>` → первые 20 абсолютных путей (`path` проекта + vault) |
| GET | `/api/deps` | dependency graph with per-edge `origin` provenance |
| GET | `/api/search` | cross-source search |
| GET | `/api/sources` | source registry/status |
| POST | `/api/sync` | re-sync sources into the DB |
| POST | `/api/focus` | focus/create a target for a project (H/T/W buttons; `dry_run` supported) |
| GET | `/api/focus/targets` | supported focus targets (buttons for the UI) |
| GET | `/api/settings` | effective UI settings: `settings`/`global`/`windows`/`defaults` (`?win=<метка>` — действующие для окна) |
| PATCH | `/api/settings` | settings: global by default, `{"…","win":"board"}` — для окна, `null` с `win` — сброс (422 называет ключ/метку) |
| POST | `/api/notify` | push a notification into the UI ring (orchestrator/CLI feedback) |
| GET | `/api/notify` | notification feed: `?unread=1&limit=50` (newest first) |
| POST | `/api/notify/read` | mark one (`{"id":N}`) or all (`{"all":true}`) as read |
| GET | `/api/ui` | panels (9), host window, live windows, minimised tabs, `gui`, `port_owner` |
| POST | `/api/windows/minimize` | collapse a panel window into a host tab (`dry_run` supported) |
| POST | `/api/windows/restore` | restore a panel from a tab back to a window (`dry_run` supported) |
| POST | `/api/windows/focus` | поднять УЖЕ ОТКРЫТОЕ окно панели (`{"win":"<метка>"}`; Hyprland `focuswindow`); «окно не открыто» → `ok:false` — фронтенд откатывается к restore. Кнопка пользователя — агентам вызывать запрещено |
| GET | `/api/herdr` | пульт herdr: воркспейсы/агенты/вкладки + сессия клиента `herdr` в терминале atlas |
| POST | `/api/herdr/focus` | `{"kind":"workspace"\|"tab","id":"…"}`: сессия клиента `herdr` + фокус |
| POST | `/api/projects/weburl` | задать/сбросить `web_url` проекта («Просмотр»; только локальный http) |
| GET | `/api/project/{name}/tasks` | цели makefile проекта (с `## doc`) |
| POST | `/api/project/{name}/tasks/run` | запустить `make <target>` (409, если уже идёт) |
| GET | `/api/project/{name}/tasks/run` | статус запуска + кольцо вывода (до 400 строк) |
| POST | `/api/project/{name}/tasks/kill` | SIGTERM активной задаче |
| GET | `/api/terminal/sessions` | сессии терминала: `proj-<имя>` (созданные) и `herdr` (клиент herdr) |
| POST | `/api/clipboard/image` | сырые байты изображения (png/jpeg/webp/gif, ≤25 МБ) → файл в `~/.local/share/atlas/paste/` → `{ok,path,bytes}`; терминал вставляет путь |
| GET | `/api/terminal/ws/{id}` | websocket-мост к PTY сессии (поднимается лениво) |
| POST | `/api/terminal/{id}/kill` | убить и забыть сессию (`proj-*` и `herdr`; id `orch` → 422) |
| POST | `/api/terminal/{id}/restart` | пересоздать сессию при следующем подключении (`proj-*` и `herdr`; id `orch` → 422) |
| POST | `/api/terminal/orch/restart` | старый адрес: алиас `POST /api/orch/restart` |
| POST | `/api/open` | открыть локальный файл записи (`gio open`; `dry_run`) |
| POST | `/api/stt/start` | голосовой ввод: начать запись с микрофона (`pw-record`, 16 kHz mono); 409, если запись уже идёт |
| POST | `/api/stt/stop` | синхронная расшифровка (`voxtype`, локально, ru) → `{"ok","text","duration_s"}`; 409 без записи |
| GET | `/api/stt/status` | `{"recording":bool,"transcribing":bool}` — опрос во время записи |
| POST | `/api/orch` | промпт оркестратору; ответ — SSE с каноническими событиями хода (409, если ход уже идёт) |
| GET | `/api/orch/status` | состояние rpc-канала: `alive`,`pid`,`cmd`,`turn_active`,`pending`,`model` |
| GET | `/api/orch/history` | кольцо последних 500 канонических событий (`{"events":[…]}`) |
| POST | `/api/orch/approve` | `{"id":"…","allow":true\|false}` — ответ на запрос подтверждения (404 у неизвестного/истёкшего id) |
| POST | `/api/orch/cancel` | прервать текущий ход (rpc `abort`) |
| POST | `/api/orch/restart` | убить rpc-процесс; следующий ход поднимет свежий |
| GET | `/api/patrol` | расписание обхода флота + последний дайджест (`last` или `null`) |

Anything else falls through to the embedded static UI (`static/`).

## Фокус (кнопки H/T/W)

`POST /api/focus` — тело `{"project":"<имя>","target":"...","dry_run":true?}`.
Ответ: `{"ok", "detail", "plan", "argv", "reused", "dry_run", "steps",
"session"}`, где `plan` — `focus|create|reuse|none` (сейчас используются
`focus`/`create`/`none`; `reuse` зарезервирован и поведенчески равен `focus`),
`reused` — выбран существующий объект вместо создания нового, `argv` — первая
изменяющая команда плана (как массив аргументов, без шелла), `steps` — ПОЛНЫЙ
план по шагам: `{"desc":"…","argv":[…]}` для изменяющих шагов,
`{"desc":"…","argv":[…],"read_only":true}` для read-only-разведки
(`herdr api snapshot`, `herdr tab list`); разведка в `argv` не попадает. Во
втором шаге T-плана `{new_ws_id}` — id, подставляемый после создания
воркспейса (парсится из вывода `workspace create`). Многошаговые планы
(T при отсутствии воркспейса) выполняются по шагам, остальные шаги видны в
`detail` (`$ cmd1 → out | $ cmd2 → out`).

Целей три: **H** — воркспейс herdr, **T** — вкладка herdr, **W** — терминал
atlas. Цель `orchestrator` (**O**) удалена: оркестратор работает по всему
флоту проектов, а не для одного проекта, и живёт в собственной панели
«Оркестратор» (rpc-чат) — отдельная кнопка для него не имеет смысла.
Цель `wezterm` тоже удалена: W теперь открывает терминал atlas, а не окно
wezterm. Запрос со старой целью (`wezterm`, `orchestrator`) → 422 со списком
поддерживаемых; неизвестный проект → 404.

**wezterm атласом используется ровно в одном месте** — кнопка «↗» в панели
«Терминал» открывает сессию в **выделенном окне wezterm, принадлежащем atlas**
(см. «Терминал» ниже); для проектов, оркестратора и herdr-клиента wezterm-окон
нет (маркер окна `atlas-herdr` и подбор клиента по `ps`/tty удалены), фокус
H/T/W по-прежнему работает с панелями. Инструменты `tools/orch-*.sh` остаются
ручными скриптами для тех, кто предпочитает окно wezterm вместо панели; к
фокусу они больше не имеют никакого отношения.

- **`dry_run:true` ничего не выполняет** — только читает живое состояние
  (read-only subprocess) и возвращает план (`steps` показывает его целиком).
  Это единственный машинный способ проверять фокус: живой сеанс не трогается.
- **клиент herdr (H и T)**: herdr не имеет собственных окон — его видимый UI
  живёт в клиенте, а клиент теперь — **сессия терминала atlas с id `herdr`**
  (голый `herdr`, «Launch or attach to the persistent session», cwd = home;
  см. «Терминал»). Перед H/T atlas обеспечивает эту сессию: живая —
  переиспользуется (read-only шаг, спавна нет), мёртвая — поднимается через
  реестр сессий терминала (это не subprocess: клиенту нужен TTY, и он
  переживает закрытие панели). Один и тот же клиент обслуживает и H, и T, и
  пульт herdr — второго herdr atlas никогда не спавнит. Сессия не создаётся
  на старте сервера — только по первому H/T, `POST /api/herdr/focus` или
  клику по чипу `herdr` в панели «Терминал». herdr-сервер лежит — H/T
  отвечают `{"ok":false,"plan":"none"}` c read-only-шагами в `steps`, без
  спавна сессии и без цикла попыток. Ответы H/T (и W) несут поле
  `"session"` (`"herdr"` для H/T, `"proj-<имя>"` для W) — фронтенд открывает
  панель «Терминал» на этой сессии.
- **`herdr_workspace` (H)**: живое состояние берётся из `herdr api snapshot`;
  воркспейс ищется по пути проекта (сначала точное совпадение cwd агентов
  воркспейса, затем префикс по границе `/`). Найден —
  `herdr workspace focus <id>` (`plan:"focus"`, `reused:true`), id сохраняется
  в `projects.herdr_workspace_id`. Нет — `herdr workspace create --cwd <путь>
  --focus` (`plan:"create"`), созданный id парсится из вывода и сохраняется,
  поэтому повторное нажатие H даёт `focus`, а не второй `create` (дубликаты
  исчезли). Если пути нет, но сохранённый id жив — фокус на него; иначе
  `{"ok":false,"plan":"none","detail":"project '...' has no path"}`.
- **`herdr_tab` (T)**: «вкладка проекта внутри воркспейса проекта, видимая в
  сессии клиента herdr в терминале atlas». Воркспейс разрешается как для H
  (после шага сессии), затем внутри него — фокус на вкладку с меткой проекта,
  иначе на активную вкладку воркспейса (`reused:true`), иначе
  `herdr tab create --workspace <id> --cwd <путь> --label <имя> --focus`.
- **`terminal` (W)**: открывает/переиспользует сессию терминала atlas для
  проекта (`proj-<имя>`): живая сессия → `{"ok":true,"plan":"focus",
  "reused":true,"session":"proj-<имя>"}`, иначе сессия поднимается сразу
  (`plan:"create"`, `reused:false`, `session:"proj-<имя>"`) — `$SHELL -l` с
  cwd = путь проекта. Ничего не фокусируется на рабочем столе: клиент
  терминала — панель «Терминал» (WS `GET /api/terminal/ws/proj-<имя>`), а
  сама сессия живёт в процессе сервера и переживает закрытие панели. У
  проекта нет пути → 422 (шеллу негде стартовать). `dry_run` возвращает план
  (`focus` при живой сессии, `create` иначе) и ничего не запускает.
- `GET /api/focus/targets` → `{"targets":[{"target":"herdr_workspace","letter":"H","label":"herdr workspace"},{"target":"herdr_tab","letter":"T","label":"herdr tab"},{"target":"terminal","letter":"W","label":"терминал atlas"}]}` — набор кнопок для UI.

## Пульт herdr

`GET /api/herdr` собирает снимок живого состояния (`herdr api snapshot`, тот же
парсер, что у импортёра) и состояние сессии клиента `herdr` в терминале atlas:

```json
{"ok":true,"server":true,
 "workspaces":[{"id":"w1D","label":"home","focused":true,
   "agents":[{"status":"working","count":1}],
   "tabs":[{"id":"w1D:t1","label":"zsh","focused":true,"pane_count":1}]}],
 "client":{"attached":true,"session":"herdr","pid":12345}}
```

`agents` — агенты воркспейса, сгруппированные по `agent_status` (порядок по
имени статуса); `tabs` — вкладки воркспейса (`focused` из снимка).
`client.attached` — жива ли сессия терминала `herdr` (видимый клиент),
`client.pid` — pid её PTY-процесса. herdr лежит → `ok:false, server:false`,
воркспейсов нет, поля клиента `false`/`null`: выдуманных данных нет.

`POST /api/herdr/focus` — тело `{"kind":"workspace"|"tab","id":"…"}`. Сначала
обеспечивается сессия клиента `herdr` ровно как в H/T (живая —
переиспользуется, мёртвая — поднимается через реестр сессий терминала), затем
выполняется `herdr workspace focus <id>` / `herdr tab focus <id>`. Ответ
`{"ok":true,"detail":"…","argv":["…"],"reused":true,"session":"herdr"}`:
`argv` — команда фокуса, `reused` — переиспользована ли уже живая сессия,
`detail` — выполненные шаги в формате `$ cmd → out`. Неизвестный `kind` или
пустой `id` → 422. herdr лежит →
`{"ok":false,"detail":"herdr сервер недоступен"}` и НИЧЕГО не спавнится.

## Создание проектов и источников

`POST /api/projects/create` — тело `{"name":"…","parent":null|"sub/dir",
"git_init":true?}`. Правила: `name` обязан соответствовать
`[A-Za-z0-9][A-Za-z0-9._-]*` (ASCII буква/цифра в начале; это исключает `.`,
`..`, разделители путей и пустые имена — 422 называет шаблон); `parent`
(необязательный, `"sub/dir"`) после лексической нормализации обязан
оставаться внутри `~/Projects` — любой вынос `..` наружу и абсолютный путь
мимо него → 422; целевой каталог уже существует → 409 «каталог уже
существует»; проект с таким именем уже есть в БД → 409 (проверка до всяких
изменений на диске). Поведение: создаётся каталог `<~/Projects>/<parent>/<name>`,
при `git_init` (по умолчанию true) — `git init` (argv-массив, без шелла),
пишется двухстрочный `README.md` (`# <имя>`), и строка INSERT-ится в
`projects` (name, path; остальное — по умолчанию), так что проект появляется
в `GET /api/projects` немедленно. Строка **переживает `POST /api/sync`**:
sync только перевставляет найденные проекты и никогда не удаляет незнакомые
строки; а с `git_init` git-импортёр (обход `~/Projects`, глубина ≤ 4)
переоткрывает каталог сам.
Делегированный ворктри (`~/.atlas-wt/<проект>-<id>-<hash>`) проектом не становится никогда: сессия такого прогона приписывается родительскому проекту, а git-обход его не видит.
Ответ `{"ok":true,"name":…,"path":…}`.

`POST /api/projects/{name}/sources/{kind}/create` — досоздать отсутствующий
источник проекта:

- `herdr` — создать воркспейс проекта **без фокуса**:
  `herdr workspace create --cwd <путь>` (без `--focus`), созданный id
  парсится и сохраняется в `projects.herdr_workspace_id` →
  `{"ok":true,"workspace_id":"…"}`. 409, если у проекта уже есть живой
  воркспейс: сначала проверяется сохранённый id против свежего
  `herdr api snapshot`, затем — живой воркспейс, совпадающий по пути проекта
  (дубликат создавать нельзя; его id сохраняется для будущего фокуса).
  herdr лежит → 503, ничего не создаётся; у проекта нет пути → 422.
- `git` — `<path>/.git` существует → 409 «уже репозиторий»; нет пути → 422;
  иначе `git init` (argv-массив) → `{"ok":true,"repo":true}`. G-баджи
  пересчитываются из `<path>/.git` прямо в `GET /api/projects`, так что
  бейдж горит сразу, а после sync — тем более.
- `vault` — конвенция взята из кода (`importers::import_vault`):
  vault-проекты — это ровно каталоги `~/vault/Проекты/`, сопоставляемые с
  проектами по имени каталога; значит честное создание —
  `mkdir -p ~/vault/Проекты/<имя>` + минимальный `index.md`
  (`# <имя>`), затем `projects.vault_path` = этот путь →
  `{"ok":true,"vault_path":"…"}`. V-баджи считаются из `vault_path`, так что
  бейдж горит сразу, а sync переоткрывает каталог тем же импортёром и
  COALESCE-ом сохраняет значение. `vault_path` уже задан → 409.
- `omp` и `zen` → 422 «источник появится позже» (пункт меню рисуется
  отключённым с этой же подсказкой); неизвестный `kind` → 422 со списком
  поддерживаемых; неизвестный проект → 404.

`POST /api/projects/favorite` — тело `{"name":"…","favorite":true|false}`:
избранные проекты идут первыми; флаг хранится в `project_flags` (как
`archived`), sync его не сбрасывает.

## Просмотр (iframe)

`POST /api/projects/weburl` — тело
`{"name":"…","web_url":"http://127.0.0.1:3000/"|null}`: задаёт адрес
локального веб-приложения проекта для панели «Просмотр» (iframe). Ответ
`{"ok":true,"name":<каноническое имя>,"web_url":…}`; `web_url: null` (или пустая
строка) очищает значение. Разрешён только `http://127.0.0.1:<порт>/…` или
`http://localhost:<порт>/…`, порт ≥ 1: `https`, внешние хосты, отсутствие порта
и другие схемы → 422, и в `detail` названо нарушенное правило; неизвестный
проект → 404. Имя проекта ищется по нормализованному виду (как везде), а
хранится под каноническим.

Значение живёт в таблице `project_meta` (`name`, `web_url`, `updated_at`) —
её не переписывает sync, в отличие от `projects`. `GET /api/projects` отдаёт
его в каждом проекте полем `"web_url"` (`null`, если не задан). CSP приложения
расширен под iframe и терминал: `connect-src 'self' http://127.0.0.1:8787
ws://127.0.0.1:8787` и `frame-src 'self' http://127.0.0.1:* http://localhost:*`.

## Задачи make

`GET /api/project/{name}/tasks` парсит makefile проекта (`Makefile`, `makefile`,
`GNUmakefile` — первый существующий) →
`{"ok":true,"makefile":"/abs/путь","tasks":[{"name":"build","doc":"собрать релиз"}]}`.
Правила парсера: строка — цель, если она `^([a-zA-Z0-9][a-zA-Z0-9_.%/-]*)[ ]*:(?!=)`;
строки-рецепты (начинаются с таба) и имена с ведущей точкой (`.PHONY`,
`.DEFAULT_GOAL`, …) целями не считаются; `:=` и `::=` (и вообще `=`) —
присваивания, а не цели; несколько целей в строке (`a b:`) перечисляются все;
переносы строки по `\` склеиваются в одну логическую строку, поэтому список
целей, разнесённый на несколько строк (`build test \` + `lint: dep`), не
теряется; файл в CRLF разбирается как обычно. Строка `## текст`
непосредственно над правилом становится его `doc`; если док сверху не задан,
берётся `## текст` в хвосте самого правила (`up: ## поднять стек` — приём,
которым размечены цели в реальном `lamplife/Makefile`); любой другой текст
между доком и правилом «док» отменяет.

**Makefile ищется только в корне `path` проекта** — рекурсивного обхода
подкаталогов нет (осознанно: у проекта может быть десяток вложенных
репозиториев/сборок, и «какой из них настоящий» угадать нельзя). Поэтому у
проекта, чей Makefile лежит глубже (например, `renzora` → `crates/dogoap/`),
список будет пустым; путь найденного makefile всегда возвращается в `makefile`,
так что ответ остаётся проверяемым. Нет makefile в корне — или у проекта нет
пути → `{"ok":true,"makefile":null,"tasks":[]}`.

`POST /api/project/{name}/tasks/run` — тело `{"target":"build"}`: запускает
`make <target>` массивом argv (без шелла) с cwd = путь проекта, stdout и stderr
сливаются построчно в кольцо вывода проекта. Ответ `{"ok":true,"id":N}`. Если
задача уже идёт → 409 `{"detail":"у проекта уже идёт задача"}`; у проекта нет
пути → 422; неизвестный проект → 404.

`GET /api/project/{name}/tasks/run` → `{"active":bool,"id":N|null,
"exit":i32|null,"lines":["…"],"started_at":ts,"duration_s":f}` (`exit` — `null`,
пока задача идёт; кольцо хранит до 400 последних строк, при переполнении первым
элементом идёт маркер обрезки). Задачу ещё не запускали → `active:false`,
`id:null`, `exit:null`, `lines:[]`, `started_at`/`duration_s` — `null`.

`POST /api/project/{name}/tasks/kill` — SIGTERM активному дочернему процессу →
`{"ok":true}`; нет активной задачи → `{"ok":false,"detail":"нет активной
задачи"}`. По завершении задачи в уведомления кладётся строка
`make <target> — exit N (<проект>)` (уровень `err` при ненулевом коде, иначе
`info`). Состояние задач живёт в процессе сервера (`Mutex<HashMap>`):
запущенная задача умирает вместе с сервером и не восстанавливается — так
задумано.

## Терминал

Терминал atlas — это сессии PTY, которыми владеет процесс сервера. В панели
«Терминал» живут оболочки проектов и клиент herdr:

| id | метка | команда | cwd |
|---|---|---|---|
| `proj-<имя>` | имя проекта | `$SHELL -l` | путь проекта |
| `herdr` | `herdr` | голый `herdr` («Launch or attach to the persistent session») | home |

Оркестратор здесь **не живёт**: второй живой `omp -c` в терминале — это ровно
та «двухмозговая» путаница, которую убрали. Оркестратор — отдельная панель
«Оркестратор» поверх rpc-канала (см. ниже). Запрос к терминальным маршрутам с
id `orch` (`ws`/`kill`/`restart`) → 422 «оркестратор живёт в панели
Оркестратор».

Сессия `herdr` — это **видимый клиент herdr**: herdr не имеет собственных окон,
и H/T, и пульт herdr поднимают именно её (см. «Фокус»). Она создаётся лениво —
по первому H/T, `POST /api/herdr/focus` или клику по её чипу в панели; на
старте сервера ничего не спавнится. Kill/restart для неё работают так же, как
для проектных оболочек: kill забывает сессию (следующий H/T или клик поднимет
свежую), restart пересоздаёт при следующем подключении.

`proj-<имя>` — это слаг имени проекта: нижний регистр, всё не-буквенно-цифровое
сжимается в `-` (`CBQN` → `proj-cbqn`, `omp-graft` → `proj-omp-graft`,
`Игры` → `proj-игры`). Сессия проекта создаётся по требованию: по фокусу W
(`POST /api/focus {"target":"terminal"}`) или при первом подключении к её WS.
**Сессии живут в процессе сервера** (в памяти, не в БД): перезапуск сервера
их теряет, и следующее подключение поднимает свежую.

`GET /api/terminal/sessions` →
`{"sessions":[{"id":"proj-atlas","label":"atlas","cmd":["/bin/bash","-l"],"alive":true,"pid":12345},…]}` —
`proj-*` появляются после создания, `herdr` — после первого поднятия. **Список
никогда ничего не спавнит.** `cmd` — честная команда живого PTY, а не догадка.

**Вставка изображения.** Терминалу картинка не нужна — ей нужен путь. Ctrl+V
или Ctrl+Shift+V с изображением в буфере сохраняют его в
`~/.local/share/atlas/paste/<millis>.<ext>` (`POST /api/clipboard/image`) и
вставляют в сессию **путь** — отдай его агенту, и он откроет файл. Текст
вставляется как раньше. **Вставка файла:** файл, скопированный в файл-менеджере
(буфер несёт `text/uri-list`), вставляется в сессию **путём** — `file://`
разбирается и percent-декодируется, несколько файлов идут через пробел, путь в
кавычках только если содержит пробелы или кавычки; файлы/uri-list старше
обычного текста.

`GET /api/terminal/ws/{session_id}` — websocket-мост к PTY сессии; PTY
поднимается лениво при первом подключении и переживает закрытие панели и окна
(владелец — сервер). Если дочерний процесс завершился, следующее подключение
поднимает новый. Неизвестный id → 404; id `orch` → 422; `proj-*` у проекта без
пути → 422. Так клик по чипу `herdr` в панели поднимает клиент herdr тем же
ленивым механизмом.

Сессии терминала можно открыть в wezterm-окне, **принадлежащем atlas**:
`POST /api/terminal/{id}/open` — кнопка «↗» на чипе сессии (и «↗ herdr в
wezterm» в пустом списке) в панели «Терминал». Atlas держит своё выделенное
окно wezterm и никогда не трогает личные окна пользователя: экземпляр
запускается с маркером `--workspace atlas --class atlas-wezterm` и находится
потом по этому воркспейсу среди per-instance cli-сокетов
(`$XDG_RUNTIME_DIR/wezterm/gui-sock-*`, адресация через `WEZTERM_UNIX_SOCKET`).
Первое открытие запускает экземпляр, и сессия становится его первым окном
(`wezterm start --always-new-process --workspace atlas --cwd … -- …`);
пока экземпляр жив, следующие открытия — вкладки в том же окне
(`wezterm cli --no-auto-start spawn --window-id … --cwd … -- …`). Окно
закрыли — следующее «↗» поднимет экземпляр заново. cwd и команда — те же, что
у встроенной сессии (`$SHELL -l` в пути проекта, `herdr` в домашнем каталоге).
Встроенный xterm-мост остаётся просмотром по умолчанию. herdr-переменные
окружения (HERDR_ENV, HERDR_PANE_ID, …) срезаются при запуске wezterm-детей —
окно, открытое из GUI, запущенного внутри herdr, иначе получает «nested herdr
is disabled» вместо сессии. Бинарник задаётся
ключом `binaries.term` (по умолчанию `wezterm`) в `~/.config/atlas/config.toml`;
если программа не запускается, endpoint отвечает 400 с текстом ошибки.

Окном управляет панель «Терминал» (WEZTERM-EMBED, этап A) — кнопки «Док /
Спрятать-Показать / Фокус» в полосе сессий рядом с «↗». `POST
/api/terminal/wezterm/dock {"preset":"right-half"|"right-quarter"|"overlay"}`
прижимает окно к вычисленному прямоугольнику: floating обеспечивается
`hyprctl dispatch togglefloating` (только когда окно ещё тайлится; свои
windowrules приложение не трогает), геометрия — `resizewindowpixel`, затем
`movewindowpixel` с `exact`-прямоугольником из окна atlas и его монитора
(`hyprctl clients -j` / `hyprctl monitors -j`), ответ подтверждается
перечитанным списком окон (допуск ±8 px). Пресеты: правая половина от
середины окна atlas до края монитора, правая четверть монитора, «поверх» —
в точности прямоугольник окна atlas. `POST /api/terminal/wezterm/hide` прячет
окно в special workspace `special:atlas-dock` (`movetoworkspacesilent` —
диспатчера minimize в Hyprland нет; окно и его сессии живут), `POST
/api/terminal/wezterm/show` возвращает его на воркспейс окна atlas и
фокусирует, `POST /api/terminal/wezterm/focus` поднимает наверх
(`focuswindow` + `alterzorder top`), `GET /api/terminal/wezterm/state`
показывает текущее состояние (`exists`, `docked_rect`, `hidden`, `floating`,
`at`, `size`, `workspace`). Дока без окна: dock отвечает 409 «нет окна
atlas-wezterm — сначала откройте сессию (↗)». Для рамки без полос владелец
МОЖЕТ добавить косметическое windowrule (приложение правило не ставит):
в omarchy-Lua `~/.config/hypr/hyprland.lua` —
`o.window({ class = "atlas-wezterm" }, { float = true, border_size = 0, no_shadow = true })`.

**Хвост вывода.** `GET /api/terminal/wezterm/tail?lines=40` (WEZTERM-EMBED,
§A4/этап C) отдаёт последние строки панели atlas-wezterm как снапшот
`wezterm cli get-text` (`--start-line -N` достаёт скроллбэк; `lines` ∈
5..=300, по умолчанию 40, панель выбирается детерминированно — минимальный
`window_id` воркспейса `atlas`, внутри него минимальный `pane_id`). Кнопка
«Хвост» в полосе сессий разворачивает read-only блок с шапкой «панель ·
время · строк» и опрашивает endpoint раз в 2 с, пока открыт и панель видима.
Это только просмотр: снапшот без цветов/курсора, ввод всегда остаётся в окне
wezterm — в сессию ничего не пишется. Нет окна atlas-wezterm → 404 «сначала
откройте сессию (↗)».

Протокол — JSON в каждом кадре, байты в base64:

| направление | кадр |
|---|---|
| клиент → сервер | `{"type":"in","data":"<base64>"}` |
| клиент → сервер | `{"type":"resize","cols":N,"rows":R}` |
| сервер → клиент | `{"type":"out","data":"<base64>"}` |
| сервер → клиент | `{"type":"exit","code":N}` — после него сервер закрывает соединение |
| сервер → клиент | `{"type":"busy"}` — на эту сессию уже смотрит другой клиент |

Первый кадр клиента — `resize` (сервер и сам ставит 100×30 при спавне).
Одновременно допускается ровно один зритель **на сессию**: второе подключение
к той же сессии получает `{"type":"busy"}` и закрывается (к разным сессиям —
можно). Пока зрителя нет, дочерний процесс продолжает работать, а его вывод
теряется (вещание без получателей): следующее подключение видит только новый
вывод — это осознанное ограничение.

`POST /api/terminal/{id}/kill` — убить и **забыть** сессию (проектную или
`herdr`; следующее подключение поднимает новую):
`{"ok":true,"id":"proj-atlas","killed":true}`; неизвестная сессия → 404.
`POST /api/terminal/{id}/restart` забывает сессию (дочерний процесс
убивается), следующее подключение поднимает свежую → `{"ok":true,"id":"…"}`.
Для `orch` оба адреса → 422. Старый адрес
`POST /api/terminal/orch/restart` оставлен алиасом: теперь он перезапускает
rpc-оркестратор, ровно как `POST /api/orch/restart`.

## Оркестратор

Оркестратор — это **панель «Оркестратор»** (`atlas — Оркестратор`, 900×680)
поверх одного живого rpc-канала: сервер держит единственный дочерний процесс
`omp --mode rpc --config <atlas>/orchestrator-approval.yml --session
<data_dir>/atlas-orchestrator.jsonl --cwd <atlas> --append-system-prompt
<atlas>/orchestrator.md` и общается с ним построчным
JSON (см. `src/orch.rs`).

**Одна выделенная сессия.** Ребёнок всегда привязан к файлу
`~/.local/share/atlas/atlas-orchestrator.jsonl`: первый спавн создаёт сессию
по этому пути (проверено на omp v18.6.1: `--session <путь>` с несуществующим
файлом начинает новую сессию ровно там), каждый следующий — продолжает её же
со всей историей. Раньше child запускался с `-c`, и это «продолжай последнюю
сессию в cwd» иногда подхватывало чужой транскрипт (однажды — транскрипт
родительского кондуктора в том же каталоге). Голый `--session atlas-orchestrator`
не вариант: omp матчит id по префиксу ULID-идентификаторов и отвечает
«Session "atlas-orchestrator" not found» — честный механизм закрепления это
полный путь. Файл сессии лежит вне `~/.omp/agent/sessions`, поэтому никакой
`-c`/`--resume` из других каталогов на него не влияет и наоборот.

Раньше оркестратор жил ещё и как PTY-сессия `orch` в панели «Терминал» — это
давало **две живые головы** (`omp -c` в терминале + rpc-канал), расходящиеся
по контексту. Теперь голова одна: терминал — только оболочки проектов и клиент
herdr, а вся оркестрация — этот rpc-чат.

**Один ход за раз.** Ходы сериализованы гейтом: пока идёт ход,
`POST /api/orch` отвечает 409 с первыми 80 символами текущего промпта. Таймаут
хода — 900 с. Персона оркестратора теперь предписывает и изолированную
делегацию кода: тяжелые правки он выполняет в git-worktree, созданном
нативной командой `omp worktree`, с показом diff на ревью и слиянием только
после явного одобрения пользователя (подсекция «Делегация кода» в
`orchestrator.md`).

### Поток событий (канонические типы)

`POST /api/orch` — вход промпта, тело `{"prompt":"…"}`; старое `{"message":"…"}`
тоже принимается. Ответ — SSE: `data: {событие}\n\n` на каждое событие хода.
Панель на открытии подтягивает `GET /api/orch/history` (кольцо последних 500
событий в памяти процесса; после перезапуска сервера кольцо пустое — панель
честно показывает, что истории нет).

События (одна и та же форма в SSE и в истории):

| событие | поля |
|---|---|
| `turn_start` | `prompt` — отправленный промпт |
| `text` | `delta` — кусок ответа |
| `thinking` | `delta` — кусок reasoning |
| `tool` | `name`, `detail`, `phase` (`start`/`end`) |
| `approval_request` | `id`, `tool`, `detail` — инструмент ждёт подтверждения |
| `approval_resolved` | `id`, `allow` |
| `done` | `duration_s`; при отмене добавляется `"cancelled":true` |
| `error` | `message` |

Маппинг кадров omp rpc (см. `omp://rpc.md`) на эти типы:

| кадр rpc | событие |
|---|---|
| наш собственный `{"type":"prompt"}` | `turn_start` |
| `message_update` c `assistantMessageEvent.type=="text_delta"` | `text` |
| `message_update` c `assistantMessageEvent.type=="thinking_delta"` | `thinking` |
| `tool_execution_start` / `tool_execution_end` | `tool` (`phase` start/end) |
| `extension_ui_request`, `method:"select"` с опцией `"Approve"` | `approval_request` |
| `agent_end` (`isTerminal !== false`) или `prompt_result` (`agentInvoked:false`) | `done` |
| `response` (`success:false`) | `error` |
| наш `{"type":"abort"}` (отмена) | `done` c `cancelled:true` |

### Подтверждения (approvals)

Гейт подтверждений omp в rpc-режиме — это `extension_ui_request` с
`method:"select"` и вариантами `["Approve","Deny"]` (первая строка `title` —
`Allow tool: <имя>`). Сервер больше их **не теряет**: он парсит кадр, кладёт
`{id, tool, detail, ts}` в `pending` и рассылает `approval_request`.
`POST /api/orch/approve {"id":"…","allow":true|false}` пишет ответный кадр
`{"type":"extension_ui_response","id":…,"value":"Approve"|"Deny"}` и рассылает
`approval_resolved`. Неизвестный/истёкший id → 404; недоступный rpc-канал →
422; подтверждение остаётся в `pending` и будет закрыто по таймауту.
**Авто-отказ:** подтверждение, провисевшее 5 минут, отклоняется само (ответный
кадр `Deny`, событие `approval_resolved` с `allow:false` и уведомление уровня
`warn`: `rpc: подтверждение <tool> отклонено по таймауту`). После авто-отказа
ход обычно завершается ошибкой — это корректное поведение: omp отвечает
`response` с `success:false`, и это событие доходит до потока как канонический
`error` (покрыто тестом `approval_auto_deny_then_error_reaches_stream`).

### Что спрашивается, а что нет

По умолчанию оркестратор **не спрашивает** про обычные команды: политика
подтверждений вынесена в файл `orchestrator-approval.yml` в корне репозитория
и подключается к ребёнку как one-shot оверлей omp — `--config
<atlas>/orchestrator-approval.yml`. Оверлей задаёт `tools.approvalMode: yolo`
(инструменты не спрашиваются по своему уровню) и упорядоченный список
`bash.patterns`, в котором правила `prompt` возвращают карточку подтверждения
для классов «разрушительное и наружу».

Почему именно так. Раньше ребёнок запускался с `--approval-mode always-ask` —
отсюда и жалоба «оркестратор постоянно спрашивает разрешение на выполнение
команд, даже тех которые не ломают»: под этим режимом карточку поднимал любой
вызов уровня `write`/`exec`. Обратный режим задачу не решает: `write`
автоодобряет `read`/`write`, но по-прежнему спрашивает `exec`, а все
шелл-команды идут через инструмент `bash` с уровнем `exec` — «свободно, кроме
рискованного» режимом не выражается (см. `omp://approval-mode.md`, таблица
режимов). Выражается оно парой «`yolo` + список `prompt`-правил»: явная
политика инструмента действует и в `yolo` (там же, «Resolution per tool
call», шаг 3), а `deny` абсолютен.

Ловушка измерена живой rpc-пробой 09.10.2026 и объясняется кодом `bash`-инструмента:
**в ветке без разбора на сегменты** (незнакомый шелл или форма, которую парсер не
берёт) для команды, которую **встроенный детектор** считает критичной, инструмент
возвращает «голый» safety-override вместо ответа паттернов, а под `yolo` голый
override игнорируется — `prompt`-правило там не срабатывает (замер: `rm -rf /tmp/x`
прошёл **без карточки**, хотя правило `rm -rf*` с `approval: prompt` стояло в этом
самом файле). В ветке с сегментами правила решаются раньше проверки на
критичность, так что `prompt` там сработал бы, — но полагаться на это не стоит.
Поэтому критичные классы (блок 0 оверлея) **запрещены**, а не спрашиваются:
молчаливое `rm -rf <путь>` — ровно та авария, ради которой файл существует.

- **Свободно** (карточки нет): чтение, правка файлов, сборка/тесты,
  `git status/diff/add/commit/log`, `rm <файл>`, `pacman -Q`, `> /dev/null` и
  всё прочее, что не попало в списки ниже.
- **Спрашивает** (`prompt` → карточка «Одобрить/Отклонить» в панели):
  рекурсивное/принудительное удаление **относительных** целей (`rm -rf build`,
  `rm -r*`, `find -delete`, `shred` — абсолютные пути уходят в запрет, см.
  ниже), дисковые инструменты (`dd` в файл, `fdisk*`, `parted*`,
  `mount`/`umount`), эскалация прав (`sudo`, `doas`, `pkexec`), пакетные
  менеджеры (`pacman -S*/-R*/-U*`, `apt*`, `dnf*`, `npm … -g`, `pip install*`,
  `cargo install*`, `flatpak/snap install*`), сервисы и питание
  (`systemctl *start*/…`, `loginctl *terminate*`, `pkill`, `killall`, `kill -9*`,
  `reboot`, `poweroff`, `shutdown`), история и remote git (`git push*`,
  `git reset --hard*`, `git clean -f*`, `git rebase*`, `git branch -D*`,
  `git filter-branch*`, `git remote …`), рекурсивные `chmod/chown -R*`, запись
  вне домашних каталогов (`> /etc/…`, `> /usr/…`, `> /boot/…`, `tee /etc/…`),
  пайплайны «скачал и выполнил» (`curl … | sh`) и выход на другие машины
  (`ssh`, `scp`, `rsync --delete`, `rsync …@…`).
- **Запрещено всегда** (`deny`; действует и в `yolo`, и у под-агентов):
  синтетический ввод (`wtype`, `ydotool`, `dotool`, `wlrctl`, `xdotool`),
  `hyprctl eval|dispatch|keyword|--batch` и **критичные классы самого `bash`**
  (`rm -rf`/`rm -r`/`rm -f` по абсолютному пути, `rm --no-preserve-root`,
  `sudo rm`, `sh -c`/`bash -c`/`--command`, форк-бомба, `> /dev/sd*`,
  `dd … of=/dev/*`, `shred /dev/*`, `mkfs*`, `cryptsetup`, `chmod -R … /`,
  `chown -R … /`) — блок 0 оверлея.
  Причина для второй группы — врезка выше: карточку для них под `yolo` поднять
  нельзя, `deny` там единственный работающий гейт. Правила про синтетический
  ввод продублированы в оверлее **первыми**: массивы в настройках omp не
  сливаются, а заменяются целиком (`omp://settings.md`, «Merge rules»), поэтому
  оверлей без них затенил бы глобальный гвард. Правите глобальный список —
  синхронизируйте оверлей (read-only `hyprctl clients -j` при этом остаётся
  свободным).

Полный список с комментариями по каждому блоку — в самом файле. Это
**единственный источник истины** для оркестратора (оверлей перекрывает
глобальный и проектный конфиги) и одновременно ручка настройки:
`--approval-mode` в argv больше не передаётся намеренно — иначе он молча
перекрывал бы правки файла. Поменять поведение:

- добавить класс команд → строка `- match: "…"` + `approval: prompt` в нужный
  блок файла;
- снова спрашивать всё → `tools.approvalMode: always-ask` в оверлее;
- не спрашивать ничего → `tools.approvalMode: yolo` и убрать `bash.patterns`;
- запретить вместо вопроса → `approval: deny` (для критичных классов это
  единственный работающий вариант: `prompt` там мёртв, см. врезку выше).

Хост rpc перечитывает оверлеи на лету (`omp://settings.md`, «Live file
changes»), так что правка действует без перезапуска оркестратора; перезапуск
(`POST /api/orch/restart`) нужен, только если хочется начать с чистого листа.
Если файла нет, `--config` не передаётся вовсе: для omp отсутствующий оверлей —
жёсткая ошибка, а оркестратор не должен не подняться из-за файла (см.
`cfg::orch_policy_args`); тогда действуют глобальные настройки, включая их
deny-список.

Честные границы, чтобы не обманываться:

- `bash.patterns` — это **политика, а не изоляция** (`omp://tools/bash.md`,
  «Command policy»): одобренная команда сохраняет весь доступ шелла к ФС,
  сети и подпроцессам, а `*`-глоб видит только текст команды — `find . -exec
  shred {} +`, `mv x /etc/`, свой скрипт или флаг самого инструмента
  (`curl -o /etc/x`) под правило не попадут.
- Паттерны — **анкерный текст с `*`**: правило без ведущего `*` совпадает только
  с началом строки или сегмента. В оверлее включён
  `bash.allowCompoundCommands: true`, и замерено живыми пробами, что он дробит не
  только `&&`, но и `;` (`cd /tmp && rm -rf e6` → карточка `rm -rf*`;
  `x=1; sudo pacman -Syu` → карточка `sudo*`), — поэтому `prompt`-правило видит
  свой класс и в середине строки, а не только в начале. Вне разбора остаются
  формы, для которых замеров нет и на которые поэтому не стоит полагаться:
  `|`/`&`, подоболочки, подстановки, собственный скрипт (`bash script.sh`), а
  также запуск под незнакомым (не-POSIX) шеллом — там работает только
  сопоставление всей строки. Все `deny`-правила блока 0 написаны с ведущим `*`,
  так что ловят класс и в середине строки (замерено: команда, обёрнутая как
  `stat …; rm -rf /tmp/x; stat …`, была заблокирована правилом `*rm -rf /*`).
  Читайте файл как ограду, делающую типовые аварии видимыми, а не как песочницу.
- Правила покрывают инструмент `bash`. Инструмент `eval` может поднять shell
  мимо них: им управляет глобальный `tools.approval.eval` (сейчас `allow`,
  то есть свободно). Хотите закрыть и это — добавьте
  `tools.approval.eval: prompt` в оверлей, но учтите следующий пункт.
- Под-агенты оркестратора (`task`) и headless-воркеры делегации работают без
  UI подтверждений: правило `prompt`, до которого они дойдут, карточку
  показать не может — такой вызов завершается отказом, а не выполнением
  (`omp://approval-mode.md`, «Subagents»). `deny` у них абсолютен, как и
  везде. Отсюда же простое правило: политика оверлея — про интерактивного
  оркестратора, а границей для делегации служит worktree (см.
  «Делегация» → «Безопасность»).

### Статус, отмена, перезапуск

- `GET /api/orch/status` →
  `{"alive":bool,"pid":N|null,"cmd":[…],"turn_active":bool,"pending":[{id,tool,detail,ts}],"model":"provider/id"|null}`.
  `cmd` — argv, которым поднимается (или поднят) процесс; `model` — модель,
  которую сообщил свежий `get_state` (иначе `null`). Статус никогда ничего не
  спавнит.
- `POST /api/orch/cancel` — прерывает текущий ход через **rpc-команду
  `abort`** (в протоколе она есть, так что kill/respawn не нужен). Если ход
  активен, рассылается `done` с `"cancelled":true` и ход закрывается для всех
  подписчиков; если хода нет — `cancelled:false` и ничего не рассылается.
  Ответ: `{"ok":true,"abort_sent":bool,"turn_active":bool,"cancelled":bool}`.
- `POST /api/orch/restart` — убивает rpc-процесс и забывает его; следующий ход
  (или `POST /api/orch`) лениво поднимает свежий. Ответ
  `{"ok":true,"restarted":true,"had_process":bool}`. Старый адрес
  `POST /api/terminal/orch/restart` — алиас этого маршрута.

### Персона оркестратора

Инструкции лежат в `orchestrator.md` в корне репозитория и подставляются как
**файл**: `--append-system-prompt <atlas_dir>/orchestrator.md` (см.
`cfg::orchestrator_prompt`/`cfg::orch_prompt_args`; эта обвязка не менялась).
Их подхватывает **следующий** запуск оркестратора — `POST /api/orch/restart`,
`POST /api/terminal/orch/restart` или перезапуск сервера. Уже работающий процесс
файл не перечитывает, поэтому после правки `orchestrator.md` его нужно
перезапустить. Если файла нет или он нечитаем, запуск откатывается на
встроенную одну строку (`cfg::ORCH_FALLBACK_PROMPT`) и пишет warning в лог —
оркестратор не должен не подняться из-за отсутствующего файла.

CLI `atlas` (`notify`, `help`) доступен как симлинк `~/.local/bin/atlas` →
`src-tauri/target/release/atlas`, поэтому `atlas notify "…"` из инструкций
работает как написано (подробнее — «CLI-команды»).

## Обход флота

«Обход флота» — детерминированный patrol без модели: tokio-задача, поднятая
при старте сервера (и в `--serve-only`, и в GUI-процессе, который владеет
портом), раз в цикл прогоняет **тот же** конвейер синка, что `POST /api/sync`
(общий `SYNC_LOCK` и `importers::sync_all`) и считает дайджест.

- **Расписание:** первый обход ≈ через 2 мин после старта (переопределяется
  env `ATLAS_PATROL_FIRST_RUN_SECS` — тестовый), далее каждые
  `patrol.interval_min` минут. Настройки читаются каждый цикл, поэтому смена
  интервала/включения действует со следующего цикла.
- **Пропуски:** `patrol.enabled:false` → спим; если `SYNC_LOCK` уже занят
  (идёт ручной sync) → цикл пропускается, а не встаёт в очередь.
- **Дайджест:** всего неархивных проектов; агентов в статусе `working` (из того
  же снимка herdr, что у `/api/herdr`; herdr недоступен → 0, а `herdr` попадёт
  в ошибки источников); открытые дела по квадрантам (`done=false`); источники с
  `ok=false` (имена); до 3 заброшенных неархивных проектов с путём — по
  убыванию дней с `last_activity`, только ≥3 дней, как `имя (Nд)`.
- **Уведомление:** ровно одно за цикл, уровень `info` (`warn`, если есть
  источники с ошибками), одной строкой:
  `обход флота: 54 проекта · агентов: 2 · q1: 3 · ошибки источников: нет · заброшенные: mind-limit (12д), mods (9д), tmp (7д)`.
  Сегмент `qN` появляется только для ненулевого квадранта; сегмент
  `делегировано: queued N · running M · review K` («Этап B») — только когда есть
  дела в одном из этих состояний.
- **Хранение:** дайджест пишется в таблицу `meta` под ключом `patrol.last` =
  JSON `{ts, duration_s, notified_level, projects, agents_working, board:{q1..q4},
  delegated:{queued,running,review}, sources_err:[…], stale:[…]}`.
- `GET /api/patrol` →
  `{"enabled":bool,"interval_min":N,"last":{…}|null}` (`last:null` — обход ещё
  ни разу не проходил).
- **Настройки:** `patrol.enabled` (bool, дефолт `true`) и `patrol.interval_min`
  (целое 15..10080, дефолт `360`). Оба ключа типизированы как остальные;
  переопределение на окно механизмом разрешено, но смысла не имеет.

## Открыть файл (из поиска)

Каждый хит `GET /api/search` несёт поле `"file"` — абсолютный путь локального
файла за записью: ссылка `vault:<абсолютный путь>` → этот путь; ссылка, которая
сама является существующим абсолютным путём (так у заметок vault и транскриптов
omp-сессий) → она; всё остальное (`mnemopi:facts/…`, `board:…`, проект, zen) →
`null`. Непустым `file` бывает только если `Path::is_file()` действительно
указывает на файл.

`POST /api/open` — тело `{"ref":"<source_ref или абсолютный путь>"
[,"dry_run":true]}` — применяет то же отображение. Локального файла нет → 422
`{"detail":"за этой записью нет локального файла"}`. `dry_run:true` →
`{"ok":true,"argv":["gio","open","<путь>"],"dry_run":true}` без выполнения; иначе
`gio open <путь>` запускается отдельно (ждём ≤2 с; отказ → `ok:false`; процесс
переиспользуется, чтобы не копить зомби) и ответ `{"ok":true,"opened":"<путь>"}`.

## Тема (push вместо опроса)

`GET /api/theme` остаётся мгновенным снимком (`{"mode","mtime","palette"}`,
парсер `colors.toml` omarchy). Раньше UI узнавал о смене темы только
собственным 30-секундным опросом — теперь сервер толкает сам:

- **Вотчер** (`http::spawn_theme_watcher`, поднимается рядом с `patrol::spawn`)
  раз в 2 с делает дешёвый `stat` файла
  `~/.local/state/omarchy/current/theme/colors.toml`; при сдвиге mtime файл
  перечитывается и палитра целиком кладётся в общее состояние
  (`RwLock<Option<Value>>` + атомарный счётчик версии). Кривой/удалённый файл
  состояние не портит — остаётся последняя хорошая палитра.
- **`GET /api/theme/stream`** (SSE): при подключении — текущая палитра один
  раз, затем на каждое изменение — **полный JSON той же формы, что
  `GET /api/theme`** (event `theme`; фронтенду не надо ходить догонять).
  Раз в 20 с — комментарий `: keep-alive`, чтобы промежуточные буферы не
  рвали соединение. CORS-слой общий. `GET /api/theme` не менялся — это
  резервный путь (и источник первого кадра, пока вотчер не тикнул).
- Смена детектируется по mtime, поэтому тест подменяет файл: перезапись —
  bump, тот же mtime — ничего (`theme_step_bumps_only_on_change`).

## Зависимости и их происхождение

`GET /api/deps` возвращает рёбра с полем `origin`:

```json
{"from":"emigate","to":"lamplife","kind":"strict","note":null,
 "origin":{"source":"seed","ref":"src-tauri/src/importers.rs:19",
           "label":"SEED_LINKS в importers.rs","raw":"(\"emigate\", \"lamplife\", \"strict\"),"}}
```

Два источника:

- **seed** — захардкоженный `SEED_LINKS` в `src-tauri/src/importers.rs`. Номер
  строки честно вычисляется сканом файла в момент запроса; если кортеж не
  нашёлся, `ref` остаётся без строки (`src-tauri/src/importers.rs (SEED_LINKS)`
  — устаревший номер хуже отсутствующего).
- **file** — маркер-файл `~/Projects/<проект>/.atlas-deps`, по правилу на
  строку: `<strict|soft> <целевой-проект> [свободный текст]`. `#` начинает
  комментарий, пустые строки игнорируются. Для таких рёбер
  `origin = {"source":"file","ref":"<абсолютный путь>:<строка>","label":"маркер-файл проекта","raw":"<строка как написана>"}`.
  Неизвестный целевой проект или кривая строка — ребро пропускается, а в
  `summary.warnings` попадает человекочитаемое пояснение.

Дедупликация по `(from, to, kind)`: file перекрывает seed. `summary`
(`strict`, `soft`, `cycles`) и логика циклов не изменились.

Пример `.atlas-deps`:

```
# комментарии начинаются с #
strict finance   — потому что общий API
soft lamplife    Sometimes we borrow the design tokens
```

## Уведомления (обратная связь от оркестратора)

Агент в wezterm (или любой локальный процесс) кладёт сообщение в UI:

```sh
atlas notify "сборка упала" --level warn --project atlas
# → id=42 unread=3
```

Или напрямую: `POST /api/notify` с телом
`{"text":"…","project":null,"level":"info"}` (`project` необязателен; `level`
— `info|warn|err`, по умолчанию `info`; пустой текст → 422). Ответ
`{"ok":true,"id":N,"unread":M}`.

- `GET /api/notify?unread=1&limit=50` →
  `{"items":[{"id","ts","level","project","text","read"}],"unread":M}` —
  свежие сверху, лимит по умолчанию 50, максимум 200, `unread=1` — только
  непрочитанные.
- `POST /api/notify/read` с `{"id":N}` или `{"all":true}` →
  `{"ok":true,"unread":M}` (неизвестный id → 404, повторный read — идемпотент).
- Кольцо: в БД живут только 200 свежайших строк, старые стираются при вставке.
- Ошибка одного уведомления никогда не валит остальные эндпоинты.
- Тот же транспорт из шелла: `atlas notify …` (см. «CLI-команды») — при
  недоступном сервере код 1 и сообщение в stderr.

## Настройки

Настройки бывают **глобальные** и **для конкретного окна** (панели).

`GET /api/settings` →
`{"settings":{…действующие…},"global":{…глобальные…},"windows":{…переопределения…},"defaults":{…}}`.
Без `?win` действующие равны глобальным. `GET /api/settings?win=board` → в
`settings` глобальные значения, перекрытые переопределениями окна `board`;
`global` остаётся глобальным. `windows` содержит только те окна, у которых
есть хотя бы одно переопределение, и только сами переопределённые ключи:
`{"windows":{"board":{"ui.statusbar":"hidden"}}}`.

`PATCH /api/settings` — плоское тело из ключей:

| тело | смысл |
|---|---|
| `{"ui.statusbar":"hidden"}` | записать **глобальное** значение (как раньше) |
| `{"ui.statusbar":"hidden","win":"board"}` | переопределение **для окна** `board` |
| `{"ui.statusbar":null,"win":"board"}` | сбросить переопределение окна (снова видно глобальное) |

Неизвестный ключ, недопустимое значение или неизвестная метка окна → 422 с
именем виновника; `null` без `win` → 422 (глобального «пустого» значения не
существует). Ответ — тот же объект, что у `GET` с `?win`. Значения хранятся в
таблице `settings` как JSON-скаляры: глобальные под своим ключом,
переопределения — под ключом `win:<метка>:<ключ>`; битая строка тихо
откатывается к дефолту, а не ломает запрос.

| Ключ | Значения | Дефолт |
|---|---|---|
| `ui.statusbar` | `always` / `hover` / `hidden` | `always` |
| `ui.tabbar` | `on` / `off` | `on` |
| `ui.autorefresh_sec` | целое 5..600 | `30` |
| `ui.search_k` | целое 5..100 | `20` |
| `ui.show_probe` | true/false | `true` |
| `ui.time_format` | `relative` / `absolute` | `relative` |
| `ui.show_hybrid_details` | true/false | `true` |
| `patrol.enabled` | true/false | `true` |
| `patrol.interval_min` | целое 15..10080 | `360` |

Любой из девяти ключей можно переопределить для любого окна. Кнопка «Настройки»
окном больше **не** является: панель `settings` удалена из реестра (см. «Окна
и вкладки»), а сам интерфейс настроек фронтенд открывает внутри того окна, где
кнопку нажали.

Состояние окон живёт в том же хранилище под ключом `ui.tabs` (JSON-массив
меток панелей) — через `/api/settings` он не меняется.

## Дела (доска)

`GET /api/board` отдаёт четыре массива квадрантов; у каждого дела теперь есть
поле `"done":bool`. `PATCH /api/board/item/{id}` принимает `done` (boolean)
вместе с остальными полями (`title`, `quadrant`), отдаёт обновлённое дело;
`done` не строка/не число → 422. Закрытые дела **остаются** в своих
квадрантах — раскладку (показывать/прятать закрытые) решает интерфейс, а не
сервер. Закрытие дело — это тихое действие пользователя: уведомление не
создаётся. Выполненные дела **исключаются из индекса поиска**: `search_docs`
(`board:<id>`) обновляются при любом изменении доски, и дело с `done:1` из них
выпадает (его доки удаляются из FTS и векторов). Удаления дел нет — только
закрытие.

### Бриф и файлы дела («Этап А»)

У дела есть бриф и список прикреплённых файлов — субстрат делегированной
работы:

- `POST /api/board/item` принимает необязательный `description` (строка|null),
  `PATCH /api/board/item/{id}` — тоже: строка обрезается по пробелам, длина
  после обрезки ≤ 10000 символов (иначе 422 с числом), `null` очищает бриф,
  отсутствие ключа оставляет прежний.
- `GET /api/board` несёт на каждом деле `"description":string|null` и
  `"files":[{"path":"/abs/файл","label":null}]` — порядок из PUT сохранён;
  `label` — задел схемы, в v1 всегда `null`.
- `PUT /api/board/item/{id}/files` — тело `{"paths":["/abs/файл",…]}`:
  **заменяет** набор целиком (v1-семантика). Каждый путь обязан существовать и
  быть файлом (422 перечисляет нарушителей), дубликаты схлопываются с
  сохранением порядка, не больше 20 файлов; неизвестное дело → 404. Ответ
  `{"ok":true,"files":[…]}`. Сами файлы atlas никогда не трогает — это
  только ссылки.
- Деление находит дела по брифу и файлам: тело `search_docs`
  (`board:<id>`) — заголовок, затем проект, затем описание, затем пути
  файлов (по строке). PUT файлов **сразу** пересобирает док (раньше он
  протухал до следующей мутации доски).

### Шаги-чеклист и приём делегатов («Этап B»)

У дела есть чек-лист шагов и ключ идемпотентного повтора:

- `PUT /api/board/item/{id}/steps` — тело
  `{"steps":[{"text":"…","done":false},…]}`: **заменяет** набор целиком.
  Тексты обрезаются по пробелам, пустой текст → 422, длина после обрезки
  ≤ 50 символов → иначе 422, не больше 20 шагов; неизвестное дело → 404.
  Ответ `{"ok":true,"steps":[{"id","text","done"}]}` — порядок из PUT
  (`pos`). `PATCH` дела семантики не менял; счётчик выполнения (`done/total`)
  считает фронтенд.
- `GET /api/board` несёт `"steps":[…]` на каждом деле; тексты шагов входят в
  тело `search_docs` (`board:<id>`) — дела находит и по шагам.
- `POST /api/board/item` принимает необязательный `dedupe_key` (строка,
  сохраняется в `items.dedupe_key`): оркестратор ставит задачу через
  `atlas board add "<title>" --project atlas --dedupe-key <slug>` — повторный
  прогон возвращает существующее дело (`id=N deduped`), дубликаты
  исчезают (см. «CLI-команды»). Так на board попадает делегированная работа,
  которую оркестратор подхватывает сам.

### Делегация (Этап B)

Дело можно **делегировать** воркеру — тремя целями и одной машиной состояний
(`items.delegation_state`: `` | `queued` | `running` | `review` |
`needs-feedback` | `failed` | `done`):

| цель | где работает воркер |
|---|---|
| `orchestrator` | **свой** rpc-ребёнок на дело (тот же `orchestrator-approval.yml`), `cwd` = `path` проекта дела; панельный оркестратор и его «один ход за раз» не участвуют — делегации идут параллельно и не мешают чату |
| `worktree` | отдельный `git worktree` в `~/.atlas-wt/<проект>-<slug>` + ветка `task/<slug>` |
| `dir` | `omp -p "<бриф>"` прямо в `path` проекта, без worktree/ветки |

`slug` = `<id дела>-<4 hex-символа хеша заголовка>` (стабилен для дела+заголовка
и безопасен в пути и git-ссылочном имени); каталог worktree —
`~/.atlas-wt/<проект>-<slug>` (родитель `~/.atlas-wt` создаётся), ветка —
`task/<slug>`. Ворктри помечается файлом `.atlas-deleg.json`
(`{"item","project","branch"?}`), импортёры спрашивают маркер, а шаблон имени —
легаси-fallback для ворктри, созданных до его появления.

**Повторный запуск в том же ворктри.** Если каталог уже существует, делегат
сверяет маркер: тот же `item`+`project` → это повтор с фидбеком (автопилотный
re-delegate или ручной повтор после reject), и новый прогон продолжается в той
же ветке — в этом смысл фидбек-ретраев, и цикл «ворктри уже существует» не
возникает. Нет маркера, он не читается или дело/проект чужие → отказ 409 как
раньше: чужой каталог atlas не трогает. Новое ревью считает diff от HEAD на
момент этого прогона.

**Воркеры в systemd.** Когда на машине есть `systemd-run` (детект — раз, при
старте, без конфиг-ключа), каждый воркер делегации — и rpc-ребёнок цели
`orchestrator`, и headless-воркер `worktree`/`dir` — живёт в transient-юните
`atlas-deleg-<id>-<hash4 заголовка>.service` пользователя
(`systemd-run --user --unit=… --collect --pipe --property=KillMode=control-group
--property=PrivateTmp=yes -- <omp argv>`; `--pipe` прокидывает stdio, поэтому
JSONL-протокол и кольцо вывода работают как раньше; воркер попадает в каталог
плана через родной omp-аргумент `--cwd`). Юниты переживают рестарт atlas только
в пределах сессии `systemd --user` — это **не** «пережить logout». Отмена
зовёт `systemctl --user stop` (SIGTERM всему cgroup, включая
внуков), затем та же эскалация SIGKILL через 5 с. `reap_stale` при старте
спрашивает `systemctl --user is-active` по имени юнита из строки БД и оставляет
живого воркера в `running`; в песочнице пока только `PrivateTmp=yes` — более
глубокая изоляция (ProtectHome/ReadWrite-пути) — осознанный follow-up, когда
видно, что PrivateTmp безопасен. Нет `systemd-run` (иная система/бэкенд) —
прежний прямой спавн без юнитов.

Цель `orchestrator` — отдельный rpc-ребёнок **на дело**: тот же оверлей
одобрений, `cwd` — каталог проекта. Сессия воркера — **свежая, своя на прогон**
(`<data_dir>/atlas-deleg-<slug>-r<run_id>.jsonl`): никакого авто-погружения в
последнюю сессию проекта — измеренная цена такого погружения — 223К токенов
чужого контекста на один ход (дело 24), плюс запись делегации в сессию, где
владелец работает сам. `resume` — только настоящий follow-up: у дела есть
фидбек владельца **и** записан файл сессии прошлого прогона **и** он не больше
5 МБ (больше — в лог и свежая сессия); `preview` показывает в поле `resume`,
что будет продолжено (`null` — свежая). Модель воркера закреплена атласом —
`[delegation] model` в конфиге (пусто — флаг `--model` не передаётся, действует
дефолт omp). Если в каталоге проекта лежит `graft/`, бриф велит ориентироваться
навыком graft, а не чтением вслепую. Делегации не зависят ни от панели, ни друг
от друга. Ребёнок headless: карточку одобрения показать некому — безобидное
выполняется свободно, рискованное по `prompt`-правилам отклоняется (ошибка видна
в ходе), критичные классы запрещены. Ход воркера (его текст и **вопросы**) виден
в «журнале» и в окне ревью над diff; ответ на вопрос — «вернуть с фидбеком»:
фидбек попадает в следующий бриф.

**Живость прогона — наблюдение вместо казни.** Строка прогона пишется в БД
**до** спавна ребёнка — ход виден на доске и в `/run`, пока работает; провал
спавна честно оседает в `failed`. Таймеров, убивающих ход, **нет**: долгая, но
полезная операция — долгий thinking reasoning-модели, большая генерация — не
должна умирать от молчания (пре-2.0 делегация 12 была остановлена потолком
900 с ровно посреди работы — 15 с после последнего вызова инструмента, при
максимальной тишине 76 с). Прогон кончается только честно: выход процесса,
ошибка хода, отмена владельцем, честные «ничего не сделал».

Вместо казни — **наблюдатель**: раз в ~30 с поллер смотрит {процесс жив?,
возраст последнего кадра} и говорит только на переходах — строка в кольце
«наблюдатель: тишина N с, процесс жив», когда живой ребёнок замолчал, и
«наблюдатель: кадры пошли (тишина была N с)», когда вернулся; без повторов.
Когда тишина переходит `[delegation] silence_warn_secs` (дефолт `300`,
`0` — никогда), уходит **одно** warn-уведомление за прогон: «возможно, долгий
thinking; живой журнал — на карточке дела, отмена — вручную». Каждый кадр rpc
(текст, вызов инструмента, его результат, `agent_end`) обновляет возраст —
наблюдатель видит любой реальный прогресс. При ЛЮБОМ терминале
(`review`/`needs-feedback`/`failed`/отмена) хвост кольца (последние 50 строк)
и итог воркера пишутся в строку дела (`delegation_tail`, `delegation_final`):
перезапуск сервера больше не стирает улики — `GET /run` отдаёт сохранённый
хвост, когда живого кольца нет.

**Что делать при провале.** Журнал делегации (чип «✗ провал» или ⏳ на
карточке) сначала показывает **предполагаемую причину** — честную эвристику по
последней строке хода (наблюдение наблюдателя «тишина N с, процесс жив» — это
контекст, не приговор; ошибка вызова; обрыв посреди инструмента или текста),
помеченную как догадку; сырой хвост остаётся рядом, свёрнутым. На самой
карточке проваленного дела — две кнопки: **«Возобновить»** (главная) раскрывает
фидбек — reject вернёт дело в `queued`, следующий preview покажет сессию
провального прогона (`resume=…`), «отправить» продолжит её; **«Повторить»** —
свежий прогон по сохранённой цели (без фидбека сессия новая). У живого
прогона журнал читается прямо с карточки: чип «⏳ идёт» (или ⏳-кнопка)
открывает read-only вид, который опросом `GET …/run` раз в 2 с тянет строки
хода, пока диалог открыт; финал — `exit N · время`, опрос останавливается.
Автопрокрутка доворачивает вниз, только пока читатель сам внизу.

Бриф — ровно этот текст уходит воркеру (секции «Шаги», «Файлы» и «Фидбек»
опускаются, когда пусты; `<cwd>` — рабочий каталог цели; секция «Похоже на»
появляется, только когда к делу приложены файлы узнаваемого стека):

```text
# <заголовок>
<описание или «(без описания)»>
## Шаги
- [x] сделано
- [ ] не сделано
## Файлы (прочти перед работой)
- /abs/путь
## Фидбек к прошлой попытке
<delegation_feedback>
Работай в каталоге <cwd>. Ты работаешь автономно: ответить на вопрос некому — если данных не хватает, остановись, заполни раздел ВОПРОСЫ, ничего не ломай.
## Проверка
По ходу работы проверяй как обычно (сборка, точечные тесты). ФИНАЛЬНУЮ верификацию воркер не гоняет — её при приёмке выполняет гейт atlas.
В ПРОВЕРЕНО перечисли команды гейта: по одной на строку, префикс "$ " (найди сам: Makefile-цели, cargo, go, npm/pnpm, pytest…).
Похоже на:
- Rust — cargo build && cargo test
Нечего проверять — напиши в ПРОВЕРЕНО, почему.
## Итог (обязательно)
Последнее сообщение хода — ровно четыре раздела, один за другим:
СДЕЛАНО: что сделано по делу
ИЗМЕНЕНО: перечень изменённых файлов
ПРОВЕРЕНО: команды проверки для гейта приёмки ($-строки) или почему нечего
ВОПРОСЫ: вопросы по делу; нечего спрашивать — напиши «нет»
Без этого блока работа не считается завершённой. Вместо догадки — ВОПРОСЫ.
```

Разделение ролей (двойной верификации нет): воркер работает и перечисляет,
**гейт измеряет** — при попадании дела в `review` atlas сам прогоняет
`$`-команды из `ПРОВЕРЕНО` (без списка — подсказки по стеку проекта: cargo
test / go test ./... / npm test / pytest; 300 с на команду) и пишет отчёт в
досье (`gate-results.json`) и в `/run` (`"gate": {"ran", "ok", "results"}`);
**ревьюер судит** — модель свежеконтекстного ревью получает результаты гейта
в промпт и проверки сама не повторяет. Упавший гейт — красная строка на
карточке, уведомление и предзаполненный фидбек («вернуть с фидбеком» одной
кнопкой); вето остаётся у владельца.

**Автопилот** (`[delegation] autopilot = true`, по умолчанию выключен) —
снимает руки: когда оба машинных сигнала сошлись, atlas решает сам.
- гейт ✓ + ревьюер `accept` (или ревьюера нет) → дело закрывается само,
  без мержа (worktree остаётся); уведомление «принято автопилотом».
- гейт ✗ или ревьюер `block` → авто-reject с машинным отчётом как фидбеком
  (дело в `queued`, `attempts` +1) и авто-перезапуск той же целью через ≤20 с;
  сессия продолжается — воркер читает отчёт и чинит.
- максимум `DELEGATION_RETRIES = 2` автоповтора; исчерпал → дело остаётся в
  `review` с уведомлением «решение за тобой», вся история — в досье.
- твой ручной reject обнуляет счётчик: «рули я», автопилот молчит.
- `needs-feedback` автопилот не трогает: вопросы адресованы человеку.
Счётчик попыток виден на карточке ревью («автопилот: попытка N из 2») и в
`/run` (`"attempts"`); сбрасывается accept'ом, ручным reject'ом и reset'ом.

| запрос | что делает |
|---|---|
| `POST /api/board/item/{id}/delegate/preview {"target":…}` | компилирует бриф, НИЧЕГО не запускает → `{"ok":true,"brief":"…","cwd":"…"}` |
| `POST /api/board/item/{id}/delegate {"target":…}` | запускает воркера → `{"ok":true,"run_id":N,"state":"running","ref":"…"}`; дело `running` → 409, дело в `needs-feedback` → 409 (сначала ответь), ворктри уже существует → 409, у цели нет пути проекта → 422, неизвестное дело → 404 |
| `GET /api/board/item/{id}/run` | состояние прогона + хвост кольца вывода (400 строк) + итог воркера → `{"state","lines","exit","started","duration_s","truncated","final"}`; когда живого кольца нет (перезапуск сервера) — сохранённый при терминале хвост; никогда не делегировали → `state:""`, пустые `lines`, `null`-ы |
| `POST /api/board/item/{id}/delegate/cancel` | `systemctl stop` юнита воркера (или SIGTERM напрямую), SIGKILL через 5 с → `{"ok":true}`; ничего не выполняется → 409 |
| `GET /api/board/item/{id}/diff` | изменения воркера **от стартового HEAD**, записанного при старте делегации: `git -C <ref> diff <start>` (видно и закоммиченное, и незакоммиченное), список коммитов `git log <start>..HEAD --oneline` (`commits`), неотслеживаемые файлы отдельными хунками; обрезка 400 строк / 200 kB (`truncated`); у старых делегаций без записанного HEAD — прежний `git diff HEAD` и флаг `stale:true`; вне git-репозитория — честный `ok:false` |
| `POST /api/board/item/{id}/accept {"merge":bool}` | приём на ревью (дело обязано быть на `review`, иначе 409) |
| `POST /api/board/item/{id}/reject {"feedback":"…"}` | фидбек сохраняется, дело возвращается в `queued` (фидбек попадёт в следующий бриф); доступен с `review`, `needs-feedback` и `failed` (с `failed` это восстановление: сессия провального прогона остаётся записана — следующий delegate продолжит её); пустой фидбек → 422, никогда не делегировали → 422, `queued`/`running` → 409 |

`ref` прогона: путь worktree (`worktree`), `path` проекта (`dir` и
`orchestrator` — делегация оркестратору работает в каталоге проекта дела; `diff`
для этой цели берёт тот же путь). `GET /api/board` несёт на каждом деле
`"delegation": null | {state,target,ref,run_id,started,ended,exit}` (`null` —
никогда не делегировали).

**Машина состояний.** `items.delegation_state`: `` (никогда) | `queued` |
`running` | `review` | `needs-feedback` | `failed` | `done`.

```text
         ┌─────────── reject (фидбек сохранён) ───────────┐
         ▼                                                │
  '' ⇄ queued ──delegate──▶ running ──ход дошёл ─────────▶ review
                              │                           │
                              ├─ ВОПРОСЫ непусты ─────▶ needs-feedback
                              ├─ diff пуст, вопросов нет ▶ failed
                              ├─ exit≠0 / ошибка хода ──▶ failed
                              ├─ ход упал (exit≠0/EOF) ─▶ failed
                              └─ cancel владельцем ─────▶ failed
                                                         │
                                      accept ────────────┴──▶ done
```

`needs-feedback` и `failed` возвращаются тем же reject (→ `queued`). **Провал →
верни с фидбеком (воркер продолжит свою сессию) или повтори; сброс стирает
контекст.**

`delegate` ставит `running`. На финале хода (`agent_end`) итог воркера
разбирается по контракту: непустые **ВОПРОСЫ** → `needs-feedback` (воркер ждёт
ответа: fresh delegate и accept отказывают, reject — и есть ответ, фидбек уходит
в следующий бриф; `reap_stale` такое дело не трогает, reset забывает); вопросов
нет, а изменений от стартового HEAD нет (ни незакоммиченных, ни коммитов) →
`failed` «воркер ничего не сделал»; иначе → `review`. Вне git-репозитория или
без записанного старта «ничего не сделал» доказать нельзя — машина честно
оставляет `review`. Выход процесса с кодом 0 или `done` хода → `review`;
ненулевой код или `error` хода → `failed`; `cancel` → `failed` с пометкой
«отменено»; `accept` → `done`; `reject` → `queued` (в том числе из `failed`:
фидбек сохраняется, сессия провального прогона остаётся записана — следующий
delegate продолжит её, `session_decision`). Каждый переход кладёт
уведомление: старт — `info`, готово — `info`, вопросы — `warn`, упало/отменено/
отклонено — `err`/`warn`.

**Приём (review).** `accept {"merge":false}` закрывает дело (`done`); для
ворктри уведомление называет оставленный каталог
(«принято, ворктри остался: <путь>»). `accept {"merge":true}` доступен только
для `worktree` (иначе 422). `git merge` сливает **ветку**, поэтому сначала
незакоммиченное состояние ворктри (ровно то, что показал `/diff`) коммитится в
неё под честным сообщением `atlas: делегация дела <id> — <заголовок>` — иначе
работа `omp -p`, который обычно не коммитит сам, потерялась бы молча; если
коммит не проходит, ответ `{"ok":false,"detail":"воркер оставил незакоммиченные
правки — …"}` и дело остаётся на `review`. Затем —
`git -C <проект> merge --no-edit task/<slug>`: при конфликте
`git merge --abort`, состояние остаётся `review`, ответ
`{"ok":false,"detail":"конфликт слияния — решай вручную"}` (+ `warn`); при успехе
дело → `done`, а ворктри удаляется `git worktree remove --force <путь>` (ветка
`task/<slug>` остаётся в проекте).

**Безопасность.** Кодерский воркер — это `omp -p "<бриф>" --cwd <каталог>`
(argv-массив, без шелла) в headless-режиме, где **UI подтверждений нет**:
карточку показать некому, поэтому инструменты выполняются без ответа человека —
на живом воркере (проверка Этапа B) его `read`/`bash`/`todo`/`write` прошли
подряд без единого подтверждения. Оверлей политики оркестратора
(`orchestrator-approval.yml`) воркеру **не передаётся** (в его argv нет
`--config`), так что на делегацию он не влияет: действуют глобальные настройки,
где `tools.approvalMode` не задан, а значит работает схема omp — `yolo`.
Жёсткие deny-правила гварда из `~/.omp/agent/config.yml` (`tools.approval`:
`computer: deny`; `bash.patterns`: синтетический вход и
`hyprctl eval|dispatch|keyword`) блокируют и воркера: `deny` абсолютен и в
`yolo`. Обратная сторона headless: правило `prompt` подтвердить некому, и
вызов, который до него дошёл, **не выполняется** — он завершается отказом, а не
ждёт несуществующую кнопку (`omp://approval-mode.md`, «Subagents»: там же
описано, что под-агент оркестратора headless и унаследованное prompt-правило
для него — отказ). Поэтому границей безопасности служит **изоляция**: правки
идут в отдельном worktree, а человеческий контроль — гейт
`/diff` → `/accept` (плюс явный запрет на `merge` мимо него). Состояние прогонов (кольцо вывода, сторож выхода) живёт в процессе сервера,
как и у make-задач, но сами дети в systemd-режиме — transient-юниты
(`atlas-deleg-*`, см. «Воркеры в systemd» выше): перезапуск сервера их не
убивает в пределах сессии `systemd --user`. Поэтому `reap_stale` при старте
спрашивает `systemctl --user is-active`: живой воркер остаётся `running`,
осиротевшие же дела (и все — без systemd-run) при старте честно переводятся в
`failed` (уведомление `warn`) — вместо вечной лжи «выполняется».

`patrol` («Обход флота») добавляет в дайджест сегмент
`· делегировано: queued N · running M · review K`, когда есть активные дела.

### Завершение файлов для UI

`GET /api/project/{name}/files?q=<подстрока>` — источник автодополнения
путей: обход `path` проекта (и его vault-каталога, если задан
`projects.vault_path`), только файлы, каталоги `.git`, `node_modules`,
`target`, `dist` не заходятся, глубина ≤ 6, сканируется ≤ 2000 записей;
совпадение — без учёта регистра подстрока абсолютного пути; ответ — первые
20: `{"files":["/abs/путь",…]}`. У проекта нет пути → 422, неизвестный
проект → 404.

## Окна и вкладки

Главное (host) окно — `projects`; остальные панели реестра (`board`, `deps`,
`search`, `sources`, `herdr`, `view`, `terminal`, `orchestrator` — восемь штук;
вместе с host в `GET /api/ui` ровно 9 меток) можно свернуть во вкладки хоста:
`POST /api/windows/minimize {"win":"board"}` записывает панель в `ui.tabs`,
закрывает её toplevel и при необходимости создаёт host, чтобы вкладка была
видна. `POST /api/windows/restore {"win":"board"}` убирает панель из `ui.tabs`
и открывает `index.html?win=board`. Панель `settings` из реестра удалена —
«Настройки» теперь режим внутри окна, а не окно: `{"win":"settings"}` → 404,
а метка `settings`, оставшаяся в старом `ui.tabs`, отбрасывается при загрузке.

**Вкладки переживают перезапуск.** В `tauri.conf.json` объявлено ровно одно
окно — host `projects` (та же геометрия, что была). Все остальные панели
создаёт сам процесс по общей таблице `SYNTH_WINDOWS` в `gui.rs`
(`board` 1240×780, `deps` 860×560, `search` 760×540, `sources` 620×460,
`herdr` → `atlas — herdr` 900×640, `view` → `atlas — Просмотр` 1100×760,
`terminal` → `atlas — Терминал` 900×560, `orchestrator` → `atlas — Оркестратор`
900×680), поэтому при запуске открываются
только окна, НЕ свёрнутые в вкладки: решение — чистая функция
`startup_windows(panels, tabs)` (все панели, кроме host, минус `ui.tabs`).
Порядок запуска: серверный поток инициализирует БД (схема + синк) → фоновый
поток ждёт готовности БД (`db::is_ready`), читает `ui.tabs` и создаёт нужные
окна на главном потоке через `run_on_main_thread` (`gui::open_panels`; сама
запись в БД идёт вне главного потока). Записи `ui.tabs` возможны только через
API, т.е. уже после старта HTTP-сервера, поэтому состояние читается до первой
возможной записи. Если GUI работает поверх чужого бэкенда
(`port_owner:"other"`) — своя БД в этом процессе не инициализируется, и окна
не восстанавливаются (host остаётся один).

Оба эндпоинта принимают `dry_run:true` (посчитать план, ничего не трогая) и
идемпотентны; `windows` в `GET /api/ui` выводится из живого списка окон —
окно, закрытое оконным менеджером, исчезает из ответа. Свернуть host в себя
нельзя (422, «главное окно нельзя свернуть в себя»); неизвестная метка → 404.

**`POST /api/windows/focus {"win":"<метка>"}` — поднять уже открытое окно.**
Кнопка «Открыть оркестратор» вызывает сначала его: панель ищется среди
клиентов Hyprland по классу `atlas` И точному заголовку метки
(`atlas — <Панель>`; список — в `gui.rs`, `def_of`). Разведка — read-only
`hyprctl clients -j`; подъём — `hyprctl dispatch focuswindow address:<x>`.
Совпадение ровно одно → диспетч; несколько → поднимается первый, а
двусмысленность пишется в лог (никогда не прячется); ничего →
`{"ok":false,"detail":"окно не открыто"}` (фронтенд откатывается к
`windows/restore`); hyprctl недоступен → честное `ok:false` с причиной, без
диспетча. Это **фича приложения, срабатывающая по клику пользователя** —
агентам вызывать этот эндпоинт запрещено (см. «Правила проверки»).

**Anti-flap: ответы о состоянии окон — только подтверждённые.** Раньше
нажатие W/minimize/restore отвечало «готово» по факту возвраща Tauri-вызова,
хотя toplevel мог ещё не появиться (или не исчезнуть) — отсюда мелькание
«то создалось, то нет» и протухшие чипы сессий. Теперь minimize/restore после
вызова опрашивают живой список окон (`WebviewWindow::get_all`) до
подтверждения (окно реально есть / реально ушло), но не дольше 2 с
(`gui::WINDOW_VERIFY`, шаг 50 мс). Успех → `{"ok":true,…,"appeared":true|false}`
(`appeared` — подтверждённое живое состояние панели на момент ответа:
`true` после restore, `false` после minimize); таймаут → `ok:false` с честным
detail («окно не подтвердилось в живом списке за 2 с — повторите запрос») —
фронтенд делает один повтор, недетерминизм исчезает. `dry_run` семантики не
менял; `windows` в `GET /api/ui` остаётся выведенным из живого списка в момент
запроса (проверено).

Деградация: управление окнами полноценно работает только когда HTTP-сервер и
окна — один процесс. `GET /api/ui` показывает это честно: `"gui": true|false`
(в этом процессе есть настоящие окна) и `"port_owner": "self"|"other"` (этот
процесс занял порт 8787 или работает поверх чужого сервера). В `--serve-only`
ожидаемо `"gui":false,"port_owner":"self"`; minimize/restore в таком процессе
отвечают `{"ok":false,"gui":false,"detail":"GUI не запущен (--serve-only):
окна недоступны"}` и **не** меняют `ui.tabs`; `dry_run` при этом работает и
возвращает план с `"gui":false`. Чтобы конфликт портов не превращался в
постоянную деградацию, GUI при старте сам останавливает `atlas.service` и
перепривязывает порт, а обёртка `atlas-gui` (Exec в desktop-файле) возвращает
юнит после выхода GUI — см. «Один сервер на порт» выше. Если GUI всё же
работает с `port_owner:"other"`, оконные кнопки надо отключать по этим полям:
из чужого процесса окна недостижимы. `plan` у minimize/restore: `close+tab`
(свернули), `create` (открыли окно), `none` (делать нечего).

## Голосовой ввод (push-to-talk)

Запись с микрофона и локальная расшифровка, без облаков:

- **`POST /api/stt/start`** — поднимает `pw-record --rate 16000 --channels 1
  --format s16 <tmpdir>/atlas-stt-<id>.wav` (источник по умолчанию, argv без
  шелла) → `{"ok":true,"id":"stt-<ms>"}`. Пока запись живёт, второй start →
  409 «запись уже идёт»; pw-record не поднялся (нет микрофона/бинарника) →
  честное `{"ok":false,"detail":…}`. Запись старше 60 с (voxtype
  `max_duration_secs`) авто-останавливается watchdog'ом сервера тем же путём,
  что и stop: результат держится до первого `stop`-запроса.
- **`POST /api/stt/stop`** — **синхронный**: SIGTERM завершает pw-record
  (дозапись WAV-заголовка), затем `voxtype -c <одноразовый конфиг> transcribe
  <wav>`, текст — в ответе `{"ok":true,"text":"…","duration_s":…}`
  (`duration_s` — длина записи). WAV и одноразовый конфиг удаляются в любом
  исходе; расшифровка строго одна за раз (лок). Записи нет → 409 «запись не
  активна», кроме случая, когда авто-стоп только что досчитал — его результат
  отдаётся этим запросом. Ошибка расшифровки → `{"ok":false,"detail":…}` с
  `duration_s`.
- **`GET /api/stt/status`** → `{"recording":bool,"transcribing":bool}` —
  фронтенд опрашивает его во время записи, чтобы пережить смену страницы.
- **Язык.** У `voxtype transcribe` нет флага языка (проверено по `--help`),
  а конфиг демона (`~/.config/voxtype/config.toml`) говорит
  `language = "en"` — пользователь диктует по-русски. Поэтому каждая
  расшифровка идёт с одноразовым конфигом `-c`: пользовательский конфиг,
  в котором строка `language` переписана на `"ru"` (движок `whisper` и модель
  `large-v3-turbo` сохраняются; конфига нет — минимальный
  `engine = "whisper"` + `language`). Проверено на этой машине:
  `voxtype -c <tmp> config` резолвит `language = Single("ru")` при сохранённой
  модели, и реальный прогон выдаёт русский текст. Если когда-нибудь
  понадобится другой язык — правится здесь же (`stt::ru_config_text`).
- **Задержка.** Каждый stop — свежий процесс voxtype: холодная загрузка
  модели ~4.5 с (замер 09.10.2026, large-v3-turbo), с горячим страничным
  кэшем ~1–2 с. Поэтому запрос синхронный: фронтенд показывает спиннер и
  держит длинный таймаут (сервер убивает voxtype по таймауту 300 с).

## Семантический поиск

`GET /api/search` — гибрид: FTS5 BM25 + векторный поиск, слияние через
Reciprocal Rank Fusion (k=60, равные веса списков).

- **Backend**: [fastembed](https://crates.io/projects/fastembed) — локальный ONNX
  на CPU, без внешних сервисов; модель `intfloat/multilingual-e5-base`
  (768 dims, рус+англ). e5-префиксы `query: `/`passage: ` добавляются вручную.
- **Хранение**: таблица `embeddings` в control.db — float32-блобы по одному на
  строку `search_docs` (sha256 контента для инкрементальности); косинус
  считается brute-force в Rust, sqlite-vec не используется.
- **Индексация**: зацеплена в `POST /api/sync` (и в sync при старте). Эмбеддятся
  только новые/изменившиеся документы. Первый полный проход по текущему корпусу
  ~2–4 мин, инкрементальный sync ~10 с, загрузка кэшированной модели ~1.3 с.
  Модель прогревается в фоне при старте сервера; при отсутствии кэша она один
  раз скачается (~1.1 GB), поиск до этого — FTS-only. Синхронизация больше не
  держит общий DB-лок на время загрузки модели/переэмбедки (вектор пишется
  порциями, ядро — каждый батч по 64).
- **Новые поля ответа**: `match` (`fts`|`vector`|`both`), `bm25`, `cos`,
  `model`, `took_ms`, `vector_error`, а также `file` — абсолютный путь
  локального файла за записью (`null` для `board:`/`mnemopi:`/проект/zen; см.
  «Открыть файл (из поиска)»). Семантика `kind` не изменилась.
- **Латентность**: 18–24 ms на прогретом процессе, ~180 ms на первом запросе
  после старта (warmup ONNX).
- **Кэш модели**: `~/.local/share/atlas/fastembed` (~1.1 GB); control.db
  растёт примерно на 10 MB на 3.2k документов.
- **Деградация**: без модели (нет сети при первой загрузке) поиск остаётся
  FTS-only — ответ 200 с `vector_enabled: false` и `vector_error`, никогда не
  500; следующей попыткой инициализации занимается очередной `POST /api/sync`.

## Data sources

- `~/.local/share/atlas/control.db` — the control plane's own SQLite state.
- `~/Projects` — project directories (repo layout, deps).
- `~/vault` — personal vault notes.
- `~/.omp/agent/sessions` — agent session transcripts (plus the omp memory db).
- `~/.local/state/omarchy/current/theme/colors.toml` — live theme accent/colors.

## Известные грабли

- Остановка и перезапуск фонового сервера — только
  `systemctl --user stop|restart atlas`. `pkill -x atlas` процедурой остановки
  не является: под имя `atlas` попадают и GUI, и systemd-сервер
  (`--serve-only`, тот же бинарник), а чистый SIGTERM при `Restart=on-failure`
  юнит не перезапускает. Если GUI всё же был убит pkill'ом — верните сервер:
  `systemctl --user start atlas`. Именно поэтому окна atlas закрывают через
  оконный менеджер, а не pkill.
- GUI `atlas` и systemd-сервер `--serve-only` делят порт 8787. Если порт уже
  занят сервером, GUI пишет в лог
  «bind 127.0.0.1:8787: Address already in use … keeping the GUI on the
  existing backend» и продолжает работать против существующего бэкенда — это
  штатное поведение, а не ошибка.
- Окна atlas закрываются оконным менеджером. `pkill -x atlas` сервером не
  управляет — он гасит GUI и systemd-сервер одновременно; остановить или
  перезапустить сервер можно только `systemctl --user stop|restart atlas`
  (см. первый пункт этого списка).

## Known quirk

`WEBKIT_DISABLE_DMABUF_RENDERER=1` is baked into `main.rs`: webkit2gtk's
DMABUF renderer trips over Wayland explicit sync on this compositor (protocol
error 71, `wp_linux_drm_syncobj_surface_v1` "Missing acquire timeline") and
kills the app at startup. Shared-memory rendering is the documented fallback;
an explicit user setting wins. Removable if webkitgtk fixes explicit-sync.

## History

The Python package (`atlas/*.py` + `.venv`) and the npm/Tauri-CLI wrapper were
removed once the Rust port was verified; `server-py.log` is kept only as
historical evidence.

## Правила проверки (для агентов)
- Машинная проверка atlas — это чтение файлов, `curl` к API на 127.0.0.1:8787 и `sqlite3` SELECT: всё это безопасно и воспроизводимо.
- Исключение из машинной проверки: `POST /api/windows/focus` агентам вызывать **запрещено** — это фича приложения, срабатывающая по клику пользователя (поднимает живое окно через `hyprctl dispatch focuswindow`); вызов агентом был бы манипуляцией фокусом живого сеанса. Остальной список запретов не меняется.
- Интерактивные проверки UI (набор текста в окне, клики, перемещение окон, переключение рабочих столов) выполняет ТОЛЬКО человек. Агентам запрещены: синтетический ввод (wtype/ydotool/wlrctl/xdotool), `hyprctl eval|dispatch|keyword`, фокус-перехват и любые манипуляции окнами живого сеанса.
- Разрешены проверки самого atlas агентом: запуск GUI atlas, скриншоты собственных окон atlas (кроп по границам), остановка/запуск atlas.service вокруг пересборок; по-прежнему запрещены синтетический ввод и любые манипуляции чужими окнами.
- Причина: 07.10.2026 воркер, проверяя окно поиска, набрал текст синтетическим вводом; фокус был не на окне atlas, и строка ушла в постороннее окно (публичный канал Discord). Стоимость ошибки — внешнее сообщение, которое нельзя отозвать.
- Если нужна проверка, где требуется ввод данных — попросить человека, либо проверять в headless-браузере на временном порту (как делалось для бейджей поиска), но не в живом окне.
