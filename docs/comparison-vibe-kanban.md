# atlas ↔ Vibe Kanban: сравнение

**Вердикт: Vibe Kanban — НЕ замена atlas и НЕ дополнение, а источник идей.** Это канбан для диспетчеризации агентов-кодеров (Claude Code, Codex, …) с git-worktree-изоляцией, diff-ревью и PR-флоу — узкая, но глубокая пайплайн-машина «задача → воркспейс → агент → ревью → PR» ([README](https://github.com/BloopAI/vibe-kanban)). atlas — личный пульт над всем хозяйством (проекты × сессии × vault): поиск, зависимости, herdr/терминал/оркестратор. Пересечение ядра честно ~30–35%: только «доска задач + наблюдение за агентами»; у atlas доска личная (Эйзенхауэр), у VK — тикеты под агентов. Три решающих «против» замены: (1) **компания bloop закрылась 10.04.2026**, проект передан сообществу, облачные части удаляются за 30 дней ([объявление](https://www.vibekanban.com/blog/shutdown)); (2) **omp не поддерживается** — жёсткий набор executors в коде ([crates/executors](https://github.com/BloopAI/vibe-kanban/tree/main/crates/executors/src/executors)), а наш флот — это omp; (3) нужен **Node ≥ 20 в рантайме** (`npx vibe-kanban`) и фронт на pnpm/React ([README](https://github.com/BloopAI/vibe-kanban)) — против нашего правила «один бинарник, без npm». Зрелость: Apache-2.0, ~28,3k звёзд, v0.1.45 (ещё не 1.0) ([repo](https://github.com/BloopAI/vibe-kanban), [releases](https://github.com/BloopAI/vibe-kanban/releases)).

## Что это сегодня

- Rust-бэкенд (sqlx/SQLite), локальный сервер + веб-UI (`packages/local-web`), раздача через npm ([README](https://github.com/BloopAI/vibe-kanban)). Self-host Cloud — Docker Compose ([docs](https://vibekanban.com/docs/self-hosting/deploy-docker)).
- Воркспейс = изолированная среда на задачу: **git worktree + ветка**, встроенный терминал, dev-сервер, браузер-превью с devtools; мульти-репо и несколько сессий агентов на воркспейс ([docs](https://vibekanban.com/docs/workspaces/index.md)).
- Жизненный цикл: issue на доске → воркспейс (worktree, агент) → сессия агента (Running/Idle/Needs Attention) → ревью диффа с инлайн-комментариями (фидбек агенту) → PR с AI-описанием, rebase/merge, draft, мульти-репо PR ([workspaces](https://vibekanban.com/docs/workspaces/index.md), [git](https://vibekanban.com/docs/workspaces/git-operations.md)).
- Агенты: только встроенные — Claude Code, Codex, Gemini, Copilot, Amp, Cursor, OpenCode, Droid, CCR, Qwen ([README](https://github.com/BloopAI/vibe-kanban)); профили-варианты (модель, plan, sandbox, `append_prompt`, `base_command_override`, `additional_params`) в `profiles.json` ([agent-configurations](https://www.vibekanban.com/docs/configuration-customisation/agent-configurations)). Подмена команды не добавляет парсера вывода — произвольный CLI (omp) не распознаётся.
- MCP-сервер (локальный stdio, `--mcp`): create_issue/start_workspace/run_session_prompt и др.; executors для MCP — тот же закрытый список ([MCP docs](https://vibekanban.com/docs/integrations/vibe-kanban-mcp-server.md)).
- После шатдауна: локальная архитектура остаётся, cloud-сервисы (issues, comments, projects, organizations) удаляются; заявлен community roadmap ([shutdown](https://www.vibekanban.com/blog/shutdown)).

## Таблица

| Возможность | atlas (модуль) | Vibe Kanban |
|---|---|---|
| Доска задач для агентной работы | Эйзенхауэр-доска личных дел, done-flow; агенты не привязаны к карточкам (`db.rs`, `http.rs`, board UI) | Ядро продукта: issues со статусами/приоритетами/тегами/ассайни, board + list ([docs](https://vibekanban.com/docs/issue-management.md)) |
| Диспетчизация агента на задачу | Нет (оркестратор получает промпты в чате; `tasks.rs` гонит только `make`) | Да: сессия агента на тикет/воркспейс, выбор executor+варианта, запуск по MCP ([MCP](https://vibekanban.com/docs/integrations/vibe-kanban-mcp-server.md)) |
| Worktree-изоляция | Нет: herdr-агенты работают прямо в каталоге проекта | Да: worktree+ветка на воркспейс, cleanup, флаг `DISABLE_WORKTREE_CLEANUP` ([workspaces](https://vibekanban.com/docs/workspaces/index.md), [README](https://github.com/BloopAI/vibe-kanban)) |
| Diff/ревью UI | Нет: только терминал/herdr-вывод, diff руками в редакторе | Changes panel: подсветка диффа, инлайн-комменты → фидбек агенту ([changes](https://vibekanban.com/docs/workspaces/changes.md)) |
| PR-флоу | Нет | PR из UI, AI-описание, draft, rebase/merge, конфликты, мульти-репо; GitHub/Azure ([git-operations](https://vibekanban.com/docs/workspaces/git-operations.md)) |
| Параллельные агенты | Через herdr (агенты уже параллельны), но без управления из atlas | Основа дизайна: независимые воркспейсы, несколько сессий на воркспейс ([workspaces](https://vibekanban.com/docs/workspaces/index.md)) |
| Обзор флота/статусы | Да: таблица проектов + бейджи агентов + панель herdr + дайджест (`importers.rs`, `patrol.rs`) | Частично: статусы воркспейсов Running/Idle/Needs Attention; «флота проектов» как пульта нет |
| Реестр проектов | Да: projects + архив + web_url + make-цели (`importers.rs`, `tasks.rs`) | Да: проекты-контейнеры репо; per-repo setup/cleanup/dev-server скрипты ([docs](https://vibekanban.com/docs/settings/projects-repositories.md)) |
| Граф зависимостей + провенанс | Да: `/api/deps` с `origin` на источник ребра (`deporigin.rs`) | Нет (только issue-связи blocking/related — не граф кода) |
| Поиск по источникам | Да: FTS5 + e5-векторы + RRF по vault/omp/mnemopi/zen/board (`http.rs`) | Нет: поиск/фильтры только по issues ([filtering](https://vibekanban.com/docs/cloud/filtering.md)) |
| Уведомления | Кольцо + колокольчик + CLI `atlas notify` + patrol-дайджесты (`patrol.rs`, `cli.rs`) | Настройки нотификаций, звуковые ассеты в репо ([settings](https://www.vibekanban.com/docs/settings/general)) |
| Терминал | PTY-сессии проектов в сервере, WS-мост, 1 зритель (`term.rs`) | Встроенный терминал воркспейса (cwd = worktree) ([interface](https://vibekanban.com/docs/workspaces/interface.md)) |
| Встраивание локальных web-UI | Да: iframe «Просмотр» только для 127.0.0.1 (`gui.rs`, CSP) | Своего аналога нет; зато их встроенный браузер-превью с devtools ([browser-testing](https://vibekanban.com/docs/browser-testing.md)) |
| Плановые обходы | Да: детерминированный patrol без модели, дайджест в `meta` (`patrol.rs`) | Нет |
| Персона/промпты агента | Да: `orchestrator.md` как system-prompt rpc-канала (`orch.rs`) | Да, богаче: профили-варианты, `append_prompt`, теги-снипеты `@mentions` ([profiles](https://www.vibekanban.com/docs/configuration-customisation/agent-configurations)) |
| Темы/десктоп | Да: omarchy-темы, Hyprland-правила, per-window настройки, H/T/W (`settings.rs`, `focus.rs`, `gui.rs`) | Внешний вид в настройках + VSCode-интеграция; десктоп-интеграции нет ([general](https://www.vibekanban.com/docs/settings/general)) |

## Что atlas мог бы взять (по убыванию ценности)

1. **Worktree на делегированную задачу** (M): перед выдачей кодерского промпта воркеру — `git worktree add ../<proj>-task-<slug>` и запуск omp там; мерж после ревью. Живёт в `focus.rs`/новом `wt.rs` + правка `orchestrator.md`; проект получает защиту main-ветки от экспериментов воркера.
2. **Ревью-шаг делегированных изменений** (M): после хода воркера — `git diff` проекта (или worktree) в панель Оркестратор/herdr с кнопками «принять/отправить фидбом воркеру». Живёт в `orch.rs` + `http.rs` (`/api/orch/diff?project=`), UI в `static/app.js`.
3. **Очередь задач с назначением агента** (M/L): карточке доски — поля `assignee` (herdr-агент/omp-воркер) и статус `queued→running→review→done`. Живёт в `db.rs` (колонки board), `http.rs` PATCH, board UI; patrol добавит «в очереди N» в дайджест.
4. **«N агентов на одну задачу»** (L): K воркеров в K worktrees на один промпт, выбор лучшего диффа через ревью-шаг. Требует п.1–2; живёт в `orch.rs` (фан-аут хода) + wt-модуль.
5. **Снипеты-теги для промптов** (S): `@mentions`-подстановки в поле Оркестратора. Живёт в `settings.rs` + orch-панель UI.

## Что сознательно не берём

- **Cloud/relay/organizations**: туннели `VK_TUNNEL`, relay-деплой, команды и ассайни-пользователи ([README](https://github.com/BloopAI/vibe-kanban)) — у нас одна машина и local-first; облачная половина и так удаляется после шатдауна ([shutdown](https://www.vibekanban.com/blog/shutdown)).
- **Node-стек**: рантайм Node ≥ 20 (`npx`), фронт pnpm/React ([README](https://github.com/BloopAI/vibe-kanban)) — ломает «один Rust-бинарник, vanilla JS, без npm».
- **GitHub/Azure-центричный PR-флоу**: CI, draft-PR, ревью на хостинге ([git-operations](https://vibekanban.com/docs/workspaces/git-operations.md)) — наши проекты в основном локальные, мерж локальными средствами.
- **Issue-модель командной работы**: теги, ассайни, приоритеты urgent/high/medium/low ([MCP docs](https://vibekanban.com/docs/integrations/vibe-kanban-mcp-server.md)) — Эйзенхауэр-доска atlas про личный фокус, не про PM-процесс.
- **Встроенный браузер-превью с devtools**: у atlas панель «Просмотр» уже вставляет локальные UI; отдельный devtools-браузер — зоопарк.

## Интероп: рулить VK через MCP вместо своей очереди?

VK даёт локальный stdio MCP с `create_issue`/`start_workspace`/`run_session_prompt` ([MCP docs](https://vibekanban.com/docs/integrations/vibe-kanban-mcp-server.md)) — технически omp-оркестратор мог бы ставить им задачи. Но: (1) executor omp отсутствует — делегировать можно только чужие CLI, т.е. наш флот мимо; (2) это второй живой сервис (Node-процесс) с БД и своим UI — второй мозг, от которого мы уже избавлялись с терминалом; (3) шатдаун = судьба проекта у сообщества; (4) наш rpc-канал omp уже даёт стриминг, approvals и историю — очередь дублировала бы это, не добавляя omp-специфики. **Вердикт: нет.** Дешевле и честнее перенять паттерны (worktree, ревью-дифф, очередь) в atlas — п.1–3 выше.
