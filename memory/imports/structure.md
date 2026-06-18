# Структура: модули, модель данных, потоки

Карта кода `src/`, схема хранилища и ключевые потоки. Версии/депы — в [overview.md](overview.md).
Ссылки на код — по **модулю + имени символа** (строки уезжают).

## Карта модулей `src/`

| Модуль | Ответственность | Ключевые символы |
|--------|-----------------|------------------|
| `main.rs` | Точка входа, диспетчер команд, разрешение папки памяти | `run`, `dispatch`, `resolve_dir` |
| `cli.rs` | Свой парсер аргументов: подкоманды + Windows `/флаг` | `Parsed::parse`, `VALUE_FLAGS`, `arg`/`value`/`has` |
| `commands.rs` | Хендлеры всех команд; открывают store/config, печатают JSONL | `Ctx` (+`notes_dir`/`imports_dir`/`note_path`), `remember`/`recall`/`get`/`list`/`related`/`forget`/`import`/`reindex`/`export`/`log`/`config`, `parse_id`, `index_note_best_effort`, `index_note_edges`, `warn_on_drift` |
| `config.rs` | `config.json`: типизированный `Config` + raw get/set, маскирование секретов | `Config`, `Embedding`, `Search`, `Chunking`, `default_version` (=2), `load_raw`/`save_raw`, `get_path`/`set_path`, `mask_secrets` |
| `note.rs` | Формат md-заметки (источник правды): ручной рендер/парс `---`-frontmatter + тело, **без serde_yaml** | `Note`, `render`, `parse` |
| `store.rs` | SQLite (производный индекс): схема, upsert по hex-id, FTS5, векторы, журнал, импорты, meta, **sync** (хэши файлов), **edges** (граф), `slug` | `Store`, `SCHEMA`, `fresh_id`/`mint_hex`, `upsert_note`, `fts_search`, `all_embeddings`, `ids_matching` (pre-filter), `set_note_slug`/`note_slugs`/`note_ids`, `file_state_*` (sync), `insert_edge`/`edges_from`/`delete_edges_from`, `wipe_derived`, `record_import`/`delete_chunks_for_source`, `meta_get`/`meta_set` |
| `graph.rs` | Вывод графа знаний из md (чистые функции; обвязка store — в `commands`) | `note_edges`, `scan_wikilinks`, `normalize_slug`, `resolve_target`, `build_slug_map` |
| `search.rs` | Гибридный recall: слияние FTS↔вектор через **RRF**, pre-filter, adaptive-k | `recall`/`recall_with`, `RecallOpts`, `fts_match_expr` |
| `embed/mod.rs` | Интерфейс эмбеддера, выбор провайдера, косинус, кодек blob | `trait Embedder`, `build`, `cosine`, `encode`/`decode` |
| `embed/hashing.rs` | Офлайн-эмбеддер (word + char-3gram хеширование) — провайдер `local` | `HashingEmbedder`, `tokenize`, `fnv1a` |
| `embed/api.rs` | Нейро-эмбеддер по HTTP (фича `api`): OpenAI / Ollama | `ApiEmbedder::from_config`, `request`, `extract_vector` |
| `chunk.rs` | Нарезка по структуре + overlap | `Chunk`, `markdown`, `text`, `window` |
| `import.rs` | Копия оригинала в `imports/` (правда) + `.meta.json`-сайдкар, нарезка чанков, индексация | `import_file`, `index_import`, `read_sidecar`/`is_sidecar`, `html_to_text`, `pdf_chunks` (за фичей) |
| `export.rs` | Рендер md/json/jsonl (pdf за фичей) | `render`, `to_markdown` |
| `output.rs` | Формирование JSON для вывода (lean-проекции `recall`/`related`) | `note_value`, `recall_value`, `RECALL_FIELDS`, `related_value`, `RELATED_FIELDS`, `round4`, `note_preview_value`, `split_tags`, `print_line` |
| `init.rs` | Разворачивание папки памяти (`notes/` + `imports/` + `models/` + `.gitignore`), self-copy бинарника, печать указателя; опциональный bulk-импорт `.md` из target **рекурсивно** (вложенные папки тоже), с пропуском только что созданной папки памяти; **авто-привязка файлов-инструкций агента** (CLAUDE.md/AGENTS.md/AGENT.md/GEMINI.md/.cursorrules/.github/copilot-instructions.md) — дописывает в каждый найденный pointer-блок «бери доки через `cm recall`»: блока нет → дописать; есть идентичный → пропустить молча; есть устаревший (другой exe-путь при re-init с новым `--name`) → заменить блок на месте; файлы не создаёт | `run`, `import_existing_md`, `collect_md_files`/`walk_md`/`is_md`, `wire_entry_points`/`entry_point_block`/`replace_block` (+`ENTRY_POINT_NAMES`/`WIRE_BEGIN`/`WIRE_END`), `prompt_yes_no`/`is_yes`, `display_path` |
| `util.rs` | Самоисцеляющийся тип ошибки + UTC-время без зависимостей | `AppError` (+`with_hint`), `Result`, `now`, `iso_utc`, `civil_from_days`, `preview` |
| `help.rs` | Контракт (текст help) + одностроковый указатель | `HELP`, `pointer` |

## Поток выполнения (короткоживущий процесс)

`main` → `Parsed::parse(args)` → `run`:
- `help`/нет команды → печать `help::HELP`;
- `init` → `init::run` (не открывает существующий store);
- иначе → `resolve_dir` → `Ctx::new(dir)` → `dispatch` → `commands::*`.

Ошибка любой команды печатается в stderr как `error: …` + (если есть) `пример:` с подсказкой
+ «Полный контракт: `cm help`», процесс выходит с кодом 1 (`util::AppError`).

### Разрешение папки памяти (`resolve_dir`)

Приоритет: `--dir <путь>` → переменная окружения `MEMORY_DIR` → папка самого бинарника
(`current_exe().parent()`). В папке ожидаются `store.db` + `config.json` + каталоги-правда
`notes/` и `imports/` (`Ctx::new` хранит `notes_dir`/`imports_dir`, `note_path(id)` =
`notes/<id>.md`).

## Модель данных: md — правда, `store.db` — производный индекс

`desc.md §3`. **Источник правды — файлы на диске**; `store.db` целиком пересобирается из них
командой `reindex` (см. ниже). Решение и инварианты — [notes/decisions.md](notes/decisions.md).

### Источник правды (файлы)

- **`notes/<id>.md`** — по файлу на заметку. Ручной `---`-frontmatter (плоский `key: value`,
  **не YAML**, без serde_yaml) + тело; рендер/парс — `note::render`/`note::parse` с
  фиксированным порядком ключей (байт-стабильно). Frontmatter несёт `id` (= имя файла),
  `created`, `tags`, `source`, `slug`, блок `relations` (`предикат: цель`); тело может
  содержать `[[вики-ссылки]]`. **Всё, что знает индекс/граф, выразимо здесь** (плюс сайдкары
  импортов) — иначе БД перестанет быть одноразовой.
- **`imports/<name>`** — оригиналы импортированных документов (правда), рядом
  **`<name>.meta.json`**-сайдкар (`{orig, tags}`) — несёт исходное имя и теги, которые из
  файла не вывести (`import::read_sidecar`/`is_sidecar`).

### Производное (`store.db`, `store::SCHEMA`)

Один файл, rollback-журнал (не WAL) — чтобы оставался **одним** файлом (теперь одноразовым;
см. [notes/decisions.md](notes/decisions.md)). Таблицы делятся на производные (стираются и
пересобираются `wipe_derived`) и переживающие (`oplog`/`meta`).

- **`notes`** *(производное)* — заметки и чанки: `id` (TEXT, hex), `body, tags, source,
  origin, kind, slug, created_at, created_iso, embedding BLOB`. `kind` = `note` (из
  `notes/<id>.md`) или `chunk` (из `imports/`). `id` авторится в md (`store::fresh_id`/
  `mint_hex` — 8-hex через FNV-fold time+pid+counter, с проверкой коллизий); внутренний
  INTEGER `rowid` — ключ join'а с FTS. `slug` — производная колонка для графа.
  `origin` — структурная ссылка (`файл › Заголовок`, `файл #3`). `embedding` —
  little-endian f32-blob (`embed::encode`/`decode`).
- **`notes_fts`** *(производное)* — `FTS5(body, tags, origin)`, токенайзер
  `unicode61 remove_diacritics 2`. **Не `content=`-таблица** — синхронизируется вручную:
  `upsert_note` делает явный DELETE+INSERT (rowid не повисает), `forget`/`delete_chunks_*` —
  удаление. `fts_search` JOIN'ит `notes` по rowid, чтобы вернуть публичный hex-id.
- **`edges`** *(производное, граф знаний)* — `src_id, predicate, dst_id (NULLABLE),
  dst_raw, source CHECK(relation|wikilink)`, PK `(src_id,predicate,dst_raw,source)`.
  Выводится из md (см. «Граф знаний» ниже). `dst_id NULL` = висячее ребро, `dst_raw` хранит
  дословную цель, поэтому wipe+rebuild даёт идентичную таблицу.
- **`sync`** *(производное)* — change-detection для инкрементального `reindex`: по строке на
  файл-правду (`path, kind, ref, content_hash, mtime, indexed_at`). `file_state_*`.
- **`imports`** *(truth-adjacent, переживает `wipe_derived`)* — реестр импортов
  (`source = imports/<name>, orig_name, tags, chunks, content_hash, created_*`),
  `record_import`. Несёт пользовательские теги, которые не вывести из файла, поэтому
  стирается не как чистое производное, а реконсилится `reindex` из сайдкаров.
- **`oplog`** *(переживает)* — журнал операций (`op, detail, created_*`), `log_op`.
- **`meta`** *(переживает)* — k/v; ключ `embedder_signature` для детекции дрейфа
  провайдера/dim. Не реконструируем из файлов — поэтому `wipe_derived` его не трогает.

## Гибридный recall (`search::recall_with`)

Слияние каналов — **Reciprocal Rank Fusion (RRF)**, заменил per-channel min-max нормировку
(плана R1; решение — в [notes/decisions.md](notes/decisions.md)).

1. **FTS-канал**: `fts_match_expr` дробит запрос на алфавитно-цифровые токены, заключает
   каждый в кавычки и `OR`-ит; `store.fts_search` отдаёт `(id, bm25)` для **широкого** пула
   `search.candidates.max(limit)` (плана R2) — пул лимит-независим, чтобы слиянию хватало
   хвоста. Ранг = позиция в сортировке по bm25 (1-based).
2. **Векторный канал**: `embedder.embed(query)`, `cosine` против всех `store.all_embeddings()`
   (brute-force); сортировка по косинусу убыв., tie-break по `id`. Ранг = позиция.
3. **Слияние**: для каждого кандидата `fts = w_fts/(k + rank_fts)` (0, если канал не нашёл),
   `vector = w_vec/(k + rank_vec)`; `score = fts + vector`. `k = config.search.rrf_k` (60),
   веса — `config.search.hybrid_weights` (0.5/0.5). Числа **мелкие** (~`w/(k+rank)`), НЕ [0,1].
4. **Pre-filter (R4)**: если задан `--tag`/`--origin-prefix`, кандидаты сужаются через
   `store.ids_matching` ДО скоринга.
5. **Сортировка** по score убыв., **tie-break по `id`** → детерминированный порядок (закрыл
   баг tie-order, см. [known-issues.md](known-issues.md)).
6. **Adaptive-k (K1)**: опц. `--min-score X` выкидывает слабых кандидатов; затем `truncate(limit)`
   (дефолт `limit = 5`).

Кандидаты = все заметки с эмбеддингом ∪ id, найденные FTS (`BTreeSet` → стабильный порядок
итерации). Гибридность скрыта за `recall` — контракт не меняется.

### Форма вывода `recall` (lean, плана E1)

`commands::recall` зовёт `output::recall_value`. По умолчанию печатает только `id, kind, body`
+ `tags`/`origin`/`source` (если не пусты/не null); отладочные `score/fts/vector` — под
`--explain`; `--fields a,b,c` отдаёт ровно перечисленные поля (whitelist `RECALL_FIELDS`).
Ключи в JSON — в алфавитном порядке (serde_json без `preserve_order`), байт-стабильно.
Полная запись — всегда `get <id>` (там форма не меняется).

## Пересборка индекса (`commands::reindex`)

`reindex [--all]` собирает `store.db` из правды (`notes/*.md` + `imports/`). `Store::open`
сам создаёт файл, если его удалили — отсюда recoverability. Поток:

1. **`--all`** → `store.wipe_derived()` (стирает `notes`, `notes_fts`, `sync`, `imports`,
   `edges`; `oplog`/`meta` остаются) и переэмбеддит всё. **Legacy-guard**: если в БД есть
   `note`-строки, а `notes/` пуст — `reindex --all` отказывается (затёр бы данные старой
   раскладки «БД-правда»), подсказывает сперва `export`.
2. **Эмбеддер** строится через `embed::build`; не построился (кривой провайдер) → **деградация
   до FTS-only**: keyword-индекс + sync пересобираются, векторы пропускаются (`embed_or_empty`),
   предупреждение в stderr. Так индекс восстановим даже без рабочего эмбеддера.
3. **Заметки** (`reindex_notes`): обход `notes/*.md` в детерминированном порядке;
   инкрементально — по `content_hash` через `sync` (неизменившийся файл пропускается).
   Изменившийся парсится (`note::parse`; `created` берётся из frontmatter, не `now()`),
   `upsert_note` + `set_note_slug` + `file_state_set`. Заметки, чей md исчез, **прунятся**
   (`forget` + `delete_edges_from` + `file_state_delete`).
4. **Импорты** (`reindex_imports`): обход `imports/` (сайдкары `*.meta.json` пропускаются),
   инкрементально по `content_hash` в реестре; изменившийся оригинал пере-нарезается
   (`index_import`), теги+orig name берутся из сайдкара. Реестры, чей оригинал исчез, прунятся.
5. **Граф — вторым проходом** (после полного набора заметок): для каждой изменившейся заметки
   `index_note_edges` против `note_ids()` + `build_slug_map(note_slugs())`. Поэтому **прямая
   ссылка** (B ссылается на A, написанные в один прогон) разрешается; неразрешённое остаётся
   висячим и оживает на следующем прогоне.

Вывод: `{"indexed": N, "changed": M}`. Чанки **контент-адресны** (`content_hash(file_hash#n)`)
→ их id стабильны между пересборками. `remember`/`import` тоже индексируют инкрементально (как
один файл), `reindex` — авторитетный полный проход.

## Граф знаний (`graph.rs` + `commands::index_note_edges`)

Рёбра выводятся из md (чистые функции `graph::*`; запись в `edges` — в `commands`):

- **Источники рёбер** (`note_edges`): frontmatter `relations` (`предикат: цель`, предикат
  в нижний регистр) + body `[[вики-ссылки]]` (`scan_wikilinks`, синт-предикат `links_to`).
- **Разрешение цели** (`resolve_target`): по умолчанию — по нормализованному `slug`; префикс
  `id:<hex>` форсирует адресацию по id (только если такой id есть). **Без hex-эвристики** —
  slug вида `db-schema` не спутается с id случайно.
- **`normalize_slug`**: trim + Unicode-lowercase + схлопывание ` `/`-`/`_` в один `-`.
  **Кириллица сохраняется** (контент мультиязычный — в отличие от FTS диакритику НЕ режем).
- **`build_slug_map`**: `(id, slug)` отсортированы по id возрастанию → при коллизии slug
  побеждает наименьший id (детерминизм, как tie-break в recall).
- **Висячее ребро — first-class**: неразрешённая цель пишется с `dst_id = NULL` и сохранённым
  `dst_raw`; оживает, когда цель появится на следующем `reindex`.

`commands::related` — детерминированный BFS по `edges_from`: глубина `--depth`, фильтр
`--predicate` на каждом шаге, лимит применяется последним (ближние выигрывают). Висячие цели
отдаются как `{"dangling":true,"name":"<raw>","predicate":…,"distance":N}` без id; каждая
строка несёт `dangling`/`distance`/`predicate` (whitelist `output::RELATED_FIELDS`).

## Эмбеддинги (`embed/`)

`Embedder` (`embed`, `dim`, `signature`) выбирается `embed::build(cfg)` по
`embedding.provider`:
- **`local`** (`HashingEmbedder`): word-фичи + char-3grams (padded `^слово$`), знаковое
  FNV-1a хеширование в `dim`, L2-нормировка. Char-n-grams ловят морфему — важно для русского
  (FTS5 не стеммит). Сигнатура `local:{model}:{dim}`.
- **`api`** (`ApiEmbedder`, фича `api`): POST на `embedding.endpoint`, тело по
  `embedding.api_format` (`openai`: `{model,input}` → `data[0].embedding`; `ollama`:
  `{model,prompt}` → `embedding`). Ключ — из env по имени `embedding.api_key_env`, не из
  конфига. Сигнатура `api:{model}:{format}`.

`cosine` возвращает 0 при несовпадении длины векторов — мягкая обработка дрейфа размерности.

## Конфиг (`config.rs`)

`config.json` рядом со store. Типизированный `Config` (`embedding`/`search`/`chunking`/
`version` + `name`) — для рантайма; `get`/`set`/show работают над **сырым** JSON
(`load_raw`/`save_raw`, `get_path`/`set_path` по dotted-ключу), чтобы неизвестные/будущие
ключи переживали правки. `set` валидирует, что результат всё ещё десериализуется в `Config`.
Секреты маскируются `mask_secrets` (ключи с `key`/`secret`/`token`/`password`, кроме
`api_key_env`).

## Импорт/экспорт

- `import::import_file` сперва **копирует оригинал в `imports/<name>`** (точка коммита) +
  пишет `<name>.meta.json`-сайдкар (orig name + теги) ДО эмбеддинга, затем `record_import`
  и `index_import`. Диспетч по расширению: `md`→`chunk::markdown` (по заголовкам),
  `txt`/`html`/без расширения→`chunk::text` (по абзацам + overlap, html через
  `html_to_text`), `pdf`→`pdf_chunks` (фича `pdf`). Чанки **производные**, контент-адресные
  (`content_hash(file_hash#n)`), `kind="chunk"`, `source` = путь копии в `imports/` (не путь
  вызова), `origin` — breadcrumb. Re-import идемпотентен; сбой эмбеддера не теряет оригинал —
  `reindex` восстановит чанки (см. [known-issues.md](known-issues.md)).
- `chunk::markdown` строит **полный breadcrumb** заголовков по уровневому стеку: `origin` =
  `файл › H1 › H2 › H3`, а путь предков **префиксуется в тело** чанка (вычитается из бюджета
  слов) — несколько слов контекста вместо более крупного чанка (плана R3). На `local`-эмбеддере
  выигрыш в основном лексический/char-gram; семантический — под `api`.
- `export::render` → `md` (читаемо), `json`/`jsonl` (бэкап/миграция); `pdf` за фичей.
  С `--query` экспортируется срез из `recall`, иначе `store.all()`.

## Тесты

Crate **binary-only** (нет `src/lib.rs`), поэтому две раскладки (команды — в
[build.md](build.md) «Тесты»):

- **Unit + integration** — `#[cfg(test)] mod tests` **внутри каждого `src/*.rs`**: только так
  тесты видят приватные функции (парсинг, нормировка, хеширование, нарезка, `note::render/parse`,
  `graph::*`) и крейт-внутренние типы (`Store`, `search::recall`, `import::import_file`).
  `store.rs` гоняется на `Store::open(Path::new(":memory:"))` (персистентность — на temp-файле
  через `tempfile`). После миграции добавлены группы: **fs-roundtrip** заметок (`note.rs`),
  **граф** (`graph.rs` + `commands::reindex_derives_graph_edges…`/`related_traverses…`),
  **`reindex`** (инкремент/прун/`--all`/legacy-guard) и **recoverability** (восстановление
  заметок и чанков+тегов из md/сайдкаров после удаления `store.db`).
  `search`/`import` используют `FakeEmbedder`/`embed::build`.
- **E2e (чёрный ящик)** — `tests/*.rs` через `assert_cmd` запускают бинарник `cm`: argv,
  stdin, JSONL, exit-коды, hint на stderr. Общая оснастка — `tests/common/mod.rs`
  (`run`/`run_raw`/`run_cwd`/`init`/`memory_dir`/`fixture`/`parse_jsonl`); фикстуры —
  `tests/fixtures/{sample.md,sample.txt,sample.html}` (кириллица, заголовки, script/style).
  `tests/api_provider.rs` — провайдер `api` против `httpmock` (`#![cfg(feature = "api")]`).

Сознательно **без `lib.rs`**: внутреннюю логику покрывают in-module unit'ы, контракт — e2e.
Детерминизм держится на `local`-эмбеддере; время сверяется по формату (не значению); порядок
`recall` при равных score не определён — ассертим множество/верхний хит, не порядок.
