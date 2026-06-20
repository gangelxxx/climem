# Обзор и текущее состояние

> Обновляй при бампе версии crate, изменении версии схемы (`config.version` / `SCHEMA`),
> смене активной задачи. Последняя ревизия: **2026-06-17**.

## Что за проект

**climem** — инструмент **активной памяти** для LLM: один Rust-бинарник `cm(.exe)`,
короткоживущий CLI поверх SQLite-хранилища. Данные лежат на диске, вне окна контекста;
модель *дотягивается* до них по запросу (write / search / import / export).

Концепция и дизайн — в [`../desc.md`](../desc.md) (первоисточник «почему так»).
Пользовательская документация (контракт команд, быстрый старт) — в [`../README.md`](../README.md).

Ключевое:

- **Один бинарник, без рантайма.** Каждый вызов — открыл хранилище, сделал операцию, вышел.
- **Источник правды — md-файлы** (`desc.md §3`): `notes/<id>.md` (1 файл = 1 заметка,
  ручной `---`-frontmatter + тело) + `imports/` (оригиналы импортов + `.meta.json`-сайдкары
  с тегами). Эти каталоги коммитятся; всё остальное — производное.
- **`store.db` — производный, пересобираемый индекс** (`rusqlite` + `bundled`, встроена в
  бинарник): **FTS5** (полнотекстовый) + **векторный индекс** (эмбеддинги f32-blob, косинус)
  + **граф знаний** (таблица `edges`). БД одноразова: удали — `cm reindex` соберёт заново.
- **Гибридный `recall`** сливает оба канала через **RRF** (Reciprocal Rank Fusion) по
  весам из конфига; выдача — «худой» JSONL (см. [conventions.md](conventions.md)).
- **Граф знаний `related`**: рёбра из frontmatter `relations` + `[[вики-ссылок]]` в теле,
  адресация по `slug`/`id:`, висячие цели — first-class (см. [structure.md](structure.md)).
- **Сменный провайдер эмбеддингов** (`config.json → embedding.provider`): `local` (офлайн,
  детерминированный, по умолчанию) или `api` (нейросеть по HTTP, OpenAI/Ollama).
- **Самодостаточная папка памяти**: `init` копирует бинарник + `notes/` + `imports/` +
  `store.db` + `config.json` + `.gitignore` в одну папку; коммитится правда, не индекс.
- **`help` = контракт**, живёт в бинарнике; ошибки печатают подсказку с верным примером.

Карта модулей и модель данных — в [structure.md](structure.md).

## Идентификация и версии

| Что | Значение |
|-----|----------|
| Crate (`[package].name`) | `climem` |
| Версия crate | **0.1.0** |
| Имя бинарника (`[[bin]].name`) | `cm` (→ `cm.exe` на Windows) |
| Rust edition | 2021 |
| Версия схемы хранилища (`config.version`, дефолт) | **2** (md-as-truth, см. ниже) |
| Версия эмбеддера по умолчанию (`embedding.model`) | `hash-ngram-v1`, dim **384** |

### Зависимости (`Cargo.toml`)

| Crate | Версия | Назначение |
|-------|--------|-----------|
| `rusqlite` | 0.32 (`bundled`) | SQLite в бинарнике, FTS5 |
| `serde` / `serde_json` | 1 | конфиг и JSONL-вывод |
| `ureq` | 2 (опц., фича `api`) | HTTP к нейро-эмбеддеру; `native-tls` — без ring/nasm на Windows |
| `pdf-extract` | 0.7 (опц., фича `pdf`) | импорт PDF |

Фичи cargo, профиль release и тулчейн — в [build.md](build.md).

### Версия схемы 2 (md-as-truth) — что изменилось

`config.version` бампнут **1 → 2** (`config::default_version`) — маркер раскладки «md —
правда». На уровне `store::SCHEMA` добавлены два модуля и две таблицы (детали — в
[structure.md](structure.md), решение и инварианты — в [notes/decisions.md](notes/decisions.md)):

- **новые модули**: `note.rs` (ручной рендер/парс frontmatter, без serde_yaml) и `graph.rs`
  (вывод графа из md);
- **новые таблицы `store.db`**: `edges` (граф знаний, выводится из md) и `sync` (хэши+mtime
  файлов для инкрементального `reindex`); `notes` получила производную колонку `slug`;
- **id заметки — короткий hex** (`store::fresh_id`/`mint_hex`), весь `i64 → String`.

## Состояние

- **Сборка зелёная** на 2026-06-17: `cargo build --release` собирает `cm.exe`
  (`target/release/`); фича `api` включена по умолчанию.
- **Тестовое покрытие** (после миграции md-as-truth, 2026-06-17): **249 unit/integration**
  внутри бинарника + **45 e2e** в `tests/` (`assert_cmd`), включая мок-HTTP для провайдера
  `api`, fs-roundtrip заметок (`note.rs`), граф (`graph.rs`), `reindex`/`related` и
  recoverability (восстановление из md после потери `store.db`). Зелено на `default` /
  `--no-default-features` / `--all-features`; `cargo clippy -D warnings` чисто.
  Раскладка и команды — в [structure.md](structure.md) «Тесты» и [build.md](build.md).
- **Не под git** на момент заведения памяти (`git status` → не репозиторий).
- **Реализовано** всё ядро контракта (`init/remember/recall/get/list/related/forget/import/
  reindex/export/log/config/help`), оба провайдера эмбеддингов, импорт md/txt/html, граф знаний.
- **Точки расширения, ещё не сделанные** (см. [roadmap.md](roadmap.md)): локальная
  нейромодель в процессе (слот под ONNX в `embed/`); PDF импорт/экспорт — за фичей `pdf`
  (по умолчанию выключена). **MCP-режим сознательно вырезан** (2026-06-18) — climem остаётся
  чистым CLI, см. упрощения в [roadmap.md](roadmap.md).

### Дефолты, заданные конфигом (`config.rs`)

| Ключ | Дефолт | Заметка |
|------|:------:|---------|
| `chunking.max_tokens` | **200** (слов!) | размер чанка считается в **словах**, не BPE-токенах; ~256 токенов |
| `chunking.overlap` | **32** | ~15 % overlap |
| `search.candidates` | **50** | глубина кандидатского пула FTS до слияния (вектор берёт все) |
| `search.rrf_k` | **60** | константа RRF (плана R1) |
| `search.hybrid_weights` | 0.5 / 0.5 | веса FTS / вектор в RRF |

`recall` по умолчанию `--limit 5` (`commands::recall`). Подробности — [structure.md](structure.md).

## Активная задача

- **2026-06-17** — заведена папка `memory/` (эта система) по образцу
  [HOWTO-BUILD-MEMORY.md](HOWTO-BUILD-MEMORY.md). См. [journal.md](journal.md).
- **2026-06-17** — реализован [docs/testing-plan.md](../docs/testing-plan.md): с нуля заведено
  тестовое покрытие (unit в `src/*.rs` + e2e в `tests/`). См. [journal.md](journal.md).
- **2026-06-18** — реализован [docs/token-efficiency-plan.md](../docs/token-efficiency-plan.md)
  (Фазы 0–3): lean-вывод `recall`, мельче чанк, RRF вместо min-max, глубокий пул, breadcrumb
  в чанках, pre-filter по тегам/origin, adaptive-k. Фаза 4 (X1–X5) — на будущее. См.
  [journal.md](journal.md).
- **2026-06-17** — миграция **«store.db = правда» → «md-файлы = правда, store.db =
  производный пересобираемый индекс + граф»** (`desc.md §3`). `notes/<id>.md` + `imports/` —
  источник правды; новые команды `reindex [--all]` и `related`; граф знаний (`edges`),
  инкрементальный sync, id-хэши, `config.version=2`. Решение и инварианты —
  [notes/decisions.md](notes/decisions.md). См. [journal.md](journal.md).
