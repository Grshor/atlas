# Search investigation: «золото» vs «gold», and why cocktail notes match a gold query

Date: 2026-10-07. Method: read-only — `curl` against the running server (127.0.0.1:8787),
`sqlite3 -readonly` on `~/.local/share/atlas/control.db`, source reading. No code, config or DB
changes. All raw outputs below were captured live; index was freshly rebuilt
(`meta.embed_indexed_at = 2026-10-07T14:46:09.432Z`).

## TL;DR (RU)

- FTS-нога с кириллицей работает: `MATCH 'золото'` находит 1 док, `"коктейл"*` — 3, `деньги` даёт 10 FTS-хитов. Проблема не в токенайзере.
- Расхождение «золото»/«gold» — это расщепление корпуса: во всём индексе только 2 дока со строкой «золот…» (1 memory_fact + 1 сессия), а `"gold"*` матчится в 11 доках (goldsmith, golden-тесты, bd_gold, investing.com/gold-scoreboard). Плюс векторная нога, порядок внутри которой — шум.
- Заметки про коктейли — это 9 ПУСТЫХ (0 байт) файлов-заглушек `vault/Ресурсы/Бармэн/методы приготовления/*.md`; в индексе у них есть только заголовок. e5 даёт косинус `query: золото` ↔ `passage: билд\n` = **0.8359 — выше, чем у дока, который буквально перечисляет «золото/платина/серебро» (0.8326)**. Это не баг кода, а провал дискриминации модели на коротких текстах.
- Контрольный бессмысленный запрос `zzzqqq` возвращает 20 хитов с cos 0.805–0.817 — тот же диапазон, что у осмысленных запросов (0.796–0.894, размах внутри top-20 всего 0.03). Векторный top-20 вне лексических совпадений — почти шум.
- Префиксы e5 применены корректно и ровно один раз (`passage: `/`query: `, embed.rs:88-89,135; fastembed 7.1.0 сам префиксы не добавляет). Гипотеза «сломаны префиксы» опровергнута.

## 1. What the pipeline does

Single Rust binary, axum on 127.0.0.1:8787, DB `~/.local/share/atlas/control.db` (rusqlite bundled).

**Indexing (docs).** `sync_search_docs` (importers.rs:763) upserts four kinds into `search_docs`
(upsert keeps rowid + embedding, importers.rs:686-690):
- `vault_note` — `walk_vault_notes` (importers.rs:728-760): every `*.md` under the vault (dirs not starting with `.`, files ≤ 2 MiB), body = `cap_chars(content, MAX_NOTE_BYTES=8*1024)` (importers.rs:14,751; `cap_chars` counts **chars**, util.rs:139-145), title = file stem, id = `vault:<abs path>`.
- `omp_session` — latest session file per slug, last 20 user/assistant messages, body cap 8192 chars (importers.rs:816). Per-message `TEXT_CAP=4000` (importers.rs:16) applies only to transcript event text (importers.rs:656), not to search_docs.
- `memory_fact`, `board_item` — imported from mnemopi facts / board items.

**FTS leg.** `fts_docs` is FTS5 external-content over `search_docs` (schema.sql:59-65) with **no
`tokenize=` option → default `unicode61`** (no stemming, no Russian/English stemmer; splits on
non-alphanumerics, Cyrillic is tokenized normally). Query side (http.rs:615 `api_search`):
tokens from regex `\w+` (Unicode-aware, http.rs:629), each wrapped as phrase+prefix `"tok"*`,
joined with `AND`; if fewer than `depth` hits and >1 token, re-run with `OR` (fts_query
http.rs:607-611, fallback http.rs:675-682). SQL (http.rs:650-659):
`SELECT ..., snippet(fts_docs, 1, '<b>','</b>','…', 14), rank FROM fts_docs JOIN search_docs ... ORDER BY rank LIMIT depth`, `depth = min(k*3, 300)` (http.rs:644).

**Vector leg.** `embed::search` (embed.rs:256-289): embed `q` once as `format!("query: {q}")`,
L2-normalize (embed.rs:134-140), then brute-force cosine over **all** `embeddings` rows
(`WHERE model = ? AND dim = ?`, embed.rs:265-267), keep top-`depth`.

**Embeddings.** fastembed 7.1.0 (Cargo.toml:24), `intfloat/multilingual-e5-base`, 768 dims
(embed.rs:17-20). Doc text = `passage: {title}\n{body truncated to MAX_BODY_CHARS=2000 chars}`
(embed.rs:23, 87-89, 198); `memory_fact` bodies additionally have the boilerplate `fact entity `
prefix stripped before embedding (embed.rs:94-100); doc vectors L2-normalized at store time
(embed.rs:231-233); content hash covers model+kind+title+truncated body, so a model switch
forces re-embed (embed.rs:75-85). **fastembed's `TextEmbedding::embed()` applies no prefixes of
its own** (fastembed-7.1.0/src/text_embedding/impl.rs:379 — raw transform+pool), so the e5
prefixes are applied exactly once. Verified in the local crate source.

**Fusion.** Reciprocal Rank Fusion, `RRF_K = 60` (http.rs:602), `score += 1/(60 + rank + 1)`
per list (http.rs:710), sort desc, take k (http.rs:759-761). `match ∈ fts|vector|both` is
derived purely from which legs produced the doc (http.rs:765-770). Snippet: FTS hits get the
fts5 `snippet()` window (14 tokens around the match, `<b>` markers, body column —
http.rs:651); vector-only hits get `plain_snippet` = **first 180 chars** of body,
whitespace-normalized, no highlighting (embed.rs:308-311).

## 2. Measurements

### 2.1 Corpus (SQLite, read-only)

```
sqlite3 -readonly ~/.local/share/atlas/control.db \
  "SELECT kind, count(*) FROM search_docs GROUP BY kind;"
memory_fact|2880
vault_note|380
omp_session|27
board_item|2
-- total 3289; embeddings: 3289 (missing: 0) — 100% coverage
-- model strings: intfloat/multilingual-e5-base | dim 768 | 3289 rows (single value)
-- meta: embed_model=intfloat/multilingual-e5-base, embed_dim=768,
--       embed_indexed_at=2026-10-07T14:46:09.432Z

-- body lengths (chars): kind | max | >4000 | >2000 | >8192
board_item | 46   | 0 | 0  | 0
memory_fact| 869  | 0 | 0  | 0
omp_session| 8192 | 20| 21 | 0
vault_note | 8192 | 107 | 151 | 0

-- empty / near-empty vault notes:
length(body)=0 → 31 docs;  length(body)<50 → 45 docs
(e.g. «билд», «шейк», «мадл», «свизл», «стир», «стрейн», «лейринг», «драй шейк», «файн стрейн»,
 «сок лимона», «iron», «money», «B41», «D2», …)

-- on disk: all nine 'методы приготовления' .md files are literally 0 bytes
ls -la '/home/gwushor/vault/Ресурсы/Бармэн/методы приготовления/'
-rwxr-xr-x 0 байт билд.md, драй шейк.md, лейринг.md, мадл.md, свизл.md, стир.md, стрейн.md, файн стрейн.md, шейк.md
```

### 2.2 FTS probes (SQLite, read-only)

```
SELECT count(*) FROM search_docs WHERE body LIKE '%золот%' OR title LIKE '%золот%';  → 2
  (fact:055833d76fc9831b «металлы: золото/платина/серебро/палладий…», + 1 omp_session)
SELECT count(*) FROM fts_docs WHERE fts_docs MATCH 'золото';      → 1   (rowid 46290 «fact»)
SELECT count(*) FROM fts_docs WHERE fts_docs MATCH 'gold';        → 6
SELECT count(*) FROM fts_docs WHERE fts_docs MATCH '"gold"*';     → 11  (goldsmith, golden, bd_gold, …)
SELECT count(*) FROM fts_docs WHERE fts_docs MATCH '"коктейл"*';  → 3
```

The FTS leg works for Cyrillic (MATCH returns rows; the API «деньги» query returned 10 `match=fts`
hits). unicode61 has no stemming: `"золото"*` matches «золотой/золотом» but not «золота/золотых»,
and `"gold"*` benefits from English derivation (golden/goldsmith).

### 2.3 API runs (`/api/search?q=…&k=20`; all six: `vector_enabled=true`, `model=intfloat/multilingual-e5-base`, `vector_error` absent, `took_ms` 15–21)

«золото» (took_ms 21) — 1 `both` + 19 `vector`:

```
 # kind         match   bm25    cos    title / source
 1 memory_fact  both    9.2345  0.8326 fact mnemopi:facts/055833d76fc9831b  ← единственный док со словом «золото»
 2 vault_note   vector    -     0.8359 билд   …/Бармэн/методы приготовления/билд.md      ← ПУСТОЙ файл
 3 vault_note   vector    -     0.8294 мадл   …/Бармэн/методы приготовления/мадл.md      ← ПУСТОЙ
 4 vault_note   vector    -     0.8261 свизл  …/Бармэн/методы приготовления/свизл.md     ← ПУСТОЙ
 5 vault_note   vector    -     0.8239 стир   …/Бармэн/методы приготовления/стир.md      ← ПУСТОЙ
 6 memory_fact  vector    -     0.8190 fact mnemopi:facts/0415f857a353c8d3  ← «Драг металлы (gold/…)»
 7 memory_fact  vector    -     0.8190 fact mnemopi:facts/a5ea03accc54d912  ← «Драг металлы (gold/…)»
 8 vault_note   vector    -     0.8189 стрейн …/методы приготовления/стрейн.md           ← ПУСТОЙ
 9 vault_note   vector    -     0.8178 человеческие ценности (Геймдизайн/эмоции)
10 vault_note   vector    -     0.8173 шейк   …/методы приготовления/шейк.md             ← ПУСТОЙ
11 memory_fact  vector    -     0.8138 fact «серебро 8836»
12 vault_note   vector    -     0.8108 лейринг ← ПУСТОЙ
13 memory_fact  vector    -     0.8101 fact «сеньор-уровня»
14 vault_note   vector    -     0.8077 что такое (IT/Go)
15 vault_note   vector    -     0.8070 domain (IT/Nginx)
16 vault_note   vector    -     0.8064 QO-protondb
17 memory_fact  vector    -     0.8049 fact (HR LinkedIn)
18 vault_note   vector    -     0.8048 Источник (Шаблоны)
19 vault_note   vector    -     0.8047 TODO (minecraft)
20 vault_note   vector    -     0.8042 сок лимона ← ПУСТОЙ (Бармэн/сок)
```

On the vector leg alone the ranking is: 1) «билд» (пустая заглушка) 0.8359, 2) fact со «золото»
0.8326, 3-5,8,10,12,20 — пустые коктейльные заглушки. **Пустая заглушка обошла док, буквально
содержащий «золото».**

«gold» (took_ms 15) — 3 `both` + 8 `fts` + 9 `vector`; top: fact «Драг металлы (gold/…)»
(bm25 6.79, cos 0.8159), fact (bm25 6.73, cos 0.8085), «Цитаты» (bm25 4.09, cos 0.7966;
«…the <b>goldsmith</b> … their <b>gold</b>…»), затем fts-хиты: fact c «golden» (7.47), «хочу
попробовать» (Golden tuck колода, 7.07), omp_session finance («bd_<b>gold</b>/BD_<b>GOLD</b>_01…»,
3.68), «опережающие индикаторы» (investing.com/<b>gold</b>-scoreboard, 1.13) и др.; векторные
хиты: minecraft «money» 0.8298, «билд» 0.8272, «domain» 0.8211, «iron» 0.8110, «vids» 0.8095…

«деньги» (took_ms 20) — 10 `fts` + 10 `vector`; fts: «2026-09-29» (6.22), «тренды» (1.90),
«Теория — четверг» (1.84), «Окинава-релокация» (1.76), …; vector top: minecraft «money» 0.8644,
«билд» 0.8431, «мадл» 0.8308…

«money» (took_ms 17) — 4 `both` + 6 `fts` + 10 `vector`; top: minecraft «money»
(bm25 9.61, cos **0.8945**), fact (8.52/0.8399), «trade» (11.12/0.8180), «Цитаты»
(5.09/0.8140). Единственный запрос с выраженным разделением — там, где лексика и вектора согласны.

«коктейль» (took_ms 18) — 1 `both` + 1 `fts` + 18 `vector`; топ вектора: «свизл» 0.8275, «шейк»
0.8265, «коктейли» 0.8258, «билд» 0.8207 — т.е. даже для релевантного запроса косинусы тех же
заглушек лежат в том же диапазоне 0.80–0.83.

**Контроль `zzzqqq`** (took_ms 16) — бессмысленный запрос, 20 хитов, все `match=vector`,
cos 0.8054–0.8168: 6 фактов «Zed is starting», «шейк», «драй шейк», «свизл», youtube-ссылки,
«Uzum 826213»… Ни одного содержательно связанного дока — и при этом полная выдача.
Сырой ответ (сокращено):

```json
{"took_ms":16,"vector_enabled":true,"model":"intfloat/multilingual-e5-base",
 "hits":[{"kind":"memory_fact","title":"fact","snippet":"fact entity Uzum 826213: remote Global…","match":"vector","bm25":null,"cos":0.8168,"score":0.016393},
         {"kind":"vault_note","title":"шейк","snippet":"","source_ref":"/home/gwushor/vault/Ресурсы/Бармэн/методы приготовления/шейк.md","match":"vector","bm25":null,"cos":0.8151},
         {"kind":"vault_note","title":"драй шейк","snippet":"","match":"vector","bm25":null,"cos":0.8124}, "…ещё 17 в том же духе"]}
```

### 2.4 Divergence «золото» vs «gold»

- Пересечение top-20 по `source_ref`: **6 из 20** (3 факта про драг-металлы + «билд» + «мадл» + «domain») — списки действительно «очень разные».
- FTS-ноги разные по построению: «золото» → 1 док; «gold» → 11 (корпус двуязычно расщеплён: русские заметки не пишут слово «золото», финансовая лексика — в английских доках).
- Векторная нога для обоих запросов выдаёт один и тот же пул заглушек (билд/мадл/шейк/…) в диапазоне 0.80–0.836.
- Косинусные размахи top-20: золото 0.804–0.836 (spread 0.032), gold 0.797–0.830 (0.033), деньги 0.816–0.864 (0.048), money 0.811–0.895 (0.083), коктейль 0.806–0.828 (0.021), **zzzqqq 0.805–0.817 (0.011)**. Диапазон мусорного контроля перекрывает диапазоны осмысленных запросов.

### 2.5 Snippets

- FTS-хиты: честное окно вокруг совпадения с подсветкой — `snippet(fts_docs, 1, '<b>', '</b>', '…', 14)` (http.rs:651). Пример: `…Драг металлы (<b>gold</b>/platinum/silver/palladium), Сырьё…`; `…Then the <b>goldsmith</b> realizes that not everybody will come to claim their <b>gold</b>…`.
- Vector-only хиты: `plain_snippet` = первые 180 символов body (embed.rs:308-311), без подсветки. Пример с термином отсутствует по построению (лексического совпадения нет): «что такое» → `# горутина`, «domain» → `![[Pasted image 20250201020801.png]]`.
- Для пустых заглушек snippet — **пустая строка `""`** (билд, мадл, свизл, стир, стрейн, шейк, лейринг, сок лимона — все `""`): интерфейс показывает хит, по которому невозможно понять, почему он нашёлся — потому что объяснить и нечем.

## 3. Verdicts (по вопросам 1–5)

1. **FTS с кириллицей работает** — `MATCH 'золото'` → 1 строка, `"коктейл"*` → 3, «деньги» → 10 FTS-хитов в API; векторная нога одна «золото» не вывозит: верх вектора — пустые коктейльные заглушки, а не контент про золото.
2. **Расхождение объясняется (i) расщеплением корпуса** (1 русский док против 11 английских по `gold*`) **плюс плоской векторной ногой**, чей порядок внутри диапазона 0.03 — шум; гипотеза (ii) про префиксы e5 опровергнута чтением кода и crate-исходников (префиксы `passage:`/`query:` применены ровно один раз).
3. **Коктейльные хиты — это 9 нулевых .md-заглушек, чей единственный контент — заголовок**: e5-косинус «золото»↔«билд» (0.8359) выше, чем «золото»↔док со словом «золото» (0.8326); это не баг кода, а недискриминативность e5-base на коротких текстах, подтверждённая контрольным запросом `zzzqqq` (cos 0.805–0.817, 20/20 хитов).
4. **Snippet'ы FTS-хитов — релевантные окна с подсветкой; vector-only — просто первые 180 символов без подсветки, а для пустых заглушек — пустая строка**, так что «почему сматчилось» видно ровно там, где матч честный.
5. **В индексе 3289 доков (memory_fact 2880, vault_note 380, omp_session 27, board_item 2), 100% с эмбеддингами, единственная строка модели `intfloat/multilingual-e5-base` (768); 151/380 vault-заметок и 21/27 сессий длиннее 2000-символьного окна эмбеддинга, выше импортного капа 8192 символов не превышает никто** (т.е. усечение для эмбеддинга реально, для хранения — нет; TOKEN-level обрезка e5 на 512 токенов срезает ещё раньше — [INFERENCE], без токенайзера не измерялось).

## 4. Ranked probable causes

1. **Пустые title-only заметки выигрывают векторную ногу** (подтверждено): 31 заметка с пустым телом (+45 короче 50 символов); у single-word passage косинус с single-word query аномально высок (0.80–0.836), они заняли 8 из 20 позиций выдачи «золото» и 3 верхние позиции `zzzqqq`. Заглушка обошла релевантный док.
2. **e5-base недискриминативен на этой гранулярности** (подтверждено): все top-20 косинусы лежат в 0.796–0.895, мусорный контроль `zzzqqq` — в 0.805–0.817; порога/отсечки по косинусу в коде нет (embed.rs:256-289 просто top-depth). Исключение — «money» (0.8945), где лексика и семантика согласны: пайплайн работает, когда есть лексический якорь.
3. **Двуязычное расщепление корпуса** (подтверждено): `LIKE '%золот%'` → 2 дока; `"gold"*` → 11. Русскоязычных заметок о золоте в vault нет — ожидание пользователя не может быть выполнено ни одной ногой.
4. **RRF без учёта «массы» ног** (вклад): при 1 FTS-хите 19 из 20 слотов отдаются векторному шуму с равным весом (rrf одинаковый для обоих списков, http.rs:710); никакого понижения веса vector-only хитов нет.
5. **Нет стемминга** (вклад): unicode61 без стеммера — «золото\»\* не ловит «золота/золотых»; «gold\»\* ловит golden/goldsmith, создавая асимметрию языков.
6. **Окно эмбеддинга 2000 символов (+512 токенов модели)** (вклад, вторично): 40% vault-заметок эмбеддятся по обрезку; длинные осмысленные тексты «размываются» против коротких заглушек — эффект в ту же сторону, что и п.1.
7. **Сломанные префиксы e5** (опровергнуто): embed.rs:88-89 (`passage: {title}\n{body}`), embed.rs:135 (`query: {q}`), fastembed 7.1.0 `embed()` префиксов не добавляет (src/text_embedding/impl.rs:379).
8. **Поломка FTS на кириллице** (опровергнуто): MATCH возвращает строки; unicode61 корректно токенизирует кириллицу; «деньги» даёт 10 fts-хитов.

## 5. Candidate fixes (NOT implemented)

| Fix | Expected effect | Risk |
|---|---|---|
| Исключить из векторной ноги (или из индекса) доки с пустым/почти пустым телом (напр. `length(body) < 20–50`), либо хранить их только в FTS | Убирает главный источник мусора: 31+45 заглушек перестают занимать слоты; коктейльный шум на «золото» исчезает | Минимальный; важно не выкинуть короткие, но содержательные факты — порог подобрать по распределению длин |
| Отсечение по косинусу для vector-only хитов (абсолютный порог ~0.85 по текущим измерениям, или относительный `cos < max_cos − 0.05`) | Vector-only выдача перестаёт быть шумом: `zzzqqq`/«коктейль на золото» опустеют | Средний: порог зависит от запроса и модели; нужно калибровать и, возможно, показывать «уверенность» в UI |
| Смена/апгрейд модели (e5-large, bge-m3) для лучшего разделения на коротких текстах | Более чёткий разброс косинусов, лучше межъязыковое слияние «золото»↔gold | Модель в 3–10× тяжелее/медленнее; полный ре-индекс (hash учитывает имя модели — embed.rs:75-85, механизм уже есть) |
| Стемминг/морфология в FTS (porter для en, snowball-ru или ytrigram для подстрок) | «золото» найдёт «золота/золотых», выравнивая языки | FTS5 из коробки имеет только porter (en); для ru — кастомный токенайзер или предобработка запроса/документов; средняя сложность |
| Взвешенный RRF или штраф для vector-only хитов, когда FTS-нога маломассивна; показывать причину матча в UI для vector-only (сейчас пустой snippet выглядит как глюк) | Выдача при редком русском термине перестаёт заполняться шумом; пользователь видит, почему хит попал | Низкий; UI-правки ортогональны качеству |
| Поднимать MAX_BODY_CHARS к реальному окну модели и/или эмбеддить title отдельным весом | Длинные заметки перестают обрезаться на 2000 символах | Время ре-индекса растёт; эффект вторичен по сравнению с п.1–2 |

## Appendix: pipeline files touched by this report

- `src-tauri/src/http.rs` — /api/search, RRF, snippets (602, 605, 607-611, 615, 629, 644, 650-659, 675-682, 689, 710, 722-748, 759, 765-770)
- `src-tauri/src/embed.rs` — model, prefixes, caps, brute-force search, plain_snippet (17-26, 75-85, 87-100, 134-140, 198, 231-233, 256-289, 308-311)
- `src-tauri/src/importers.rs` — caps, walk_vault_notes, upsert (14, 16, 656, 686-690, 728-760, 751, 816, 889-891)
- `src-tauri/schema.sql` — fts_docs FTS5 external-content, default tokenizer (59-65)
- `src-tauri/src/util.rs` — cap_chars (139-145)
- fastembed 7.1.0 crate — no automatic prefixes (src/text_embedding/impl.rs:379)
