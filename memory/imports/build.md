# Сборка, фичи, тулчейн, дистрибуция

Версии crate'ов/депов — в [overview.md](overview.md). Здесь — команды и процессы.

## Тулчейн

- **Rust stable** + C-тулчейн (MSVC на Windows, gcc на Linux) — нужен `rusqlite`/`bundled`
  (компилирует SQLite из исходников). Проверено на **cargo 1.91.1**.
- ⚠️ **`cargo`/`rustc` могут быть не на PATH.** Перед сборкой добавь
  `~/.cargo/bin` в `PATH` (bash: `export PATH="$HOME/.cargo/bin:$PATH"`). Это уже зафиксировано
  в авто-памяти пользователя (`rust-toolchain-path`).

## Команды

```bash
cargo build --release                  # → target/release/cm(.exe), фича api включена
cargo build --release --features pdf   # + импорт/экспорт PDF
cargo build                            # debug
cargo run -- recall "тема" --dir ./.testmem   # запуск без установки
cargo test                             # все тесты (unit в бинарнике + e2e в tests/)
cargo clippy --all-targets             # линт (нужен компонент: rustup component add clippy)
cargo fmt                              # форматирование (rustup component add rustfmt)
```

Бинарник называется `cm`, не `climem` (см. `[[bin]]` в `Cargo.toml`).

⚠️ Компоненты `clippy`/`rustfmt` могут быть **не установлены** в тулчейне —
`rustup component add clippy rustfmt`.

## Тесты

Покрытие есть (заведено 2026-06-17). Две раскладки — см. [structure.md](structure.md) «Тесты»:

```bash
cargo test                              # дефолт (api on): unit + e2e, кроме pdf и api-сети*
cargo test --bin cm                     # только #[cfg(test)] внутри бинарника (быстро)
cargo test --no-default-features        # ветки «без api»; api_provider пропущен
cargo test --all-features               # + pdf (import_pdf_disabled выключается, появляется enabled)
cargo clippy --all-targets --all-features -- -D warnings   # должно быть чисто
```

После миграции md-as-truth (2026-06-17) объём — **249 unit/integration в бинарнике + 45 e2e**
(зелено на default/`--no-default-features`/`--all-features`, clippy чисто). Новые группы:
**fs-roundtrip** заметок (`note.rs::render_parse_round_trip`), **граф** (`graph.rs`:
`normalize_slug`/`resolve_target`/`scan_wikilinks` + `commands::reindex_derives_graph_edges…`,
`related_traverses…`), **`reindex`** (инкремент/прун/`--all`/legacy-guard) и **recoverability**
(`commands::reindex_*_after_store_deleted`, `import_embedder_error_preserves_original_for_reindex`).

\* `tests/api_provider.rs` гейтится `required-features = ["api"]` (в `Cargo.toml`) —
запускается только когда фича `api` собрана; мок-эндпоинт **только `http://`** (ureq +
native-tls лезет в TLS на `https://`).

### Dev-зависимости (`Cargo.toml [dev-dependencies]`)

| Crate | Назначение |
|-------|-----------|
| `tempfile` 3 | изолированные авто-удаляемые папки под `store.db`/`config.json` (e2e + FS-unit) |
| `assert_cmd` 2 | запуск бинарника `cm`, stdin, exit-код, stdout/stderr |
| `predicates` 3 | читаемые ассерты на вывод (`contains`/regex) |
| `httpmock` 0.7 | локальный HTTP-мок для провайдера `api` (Cargo не умеет гейтить dev-dep фичей — лежит безусловно, цель `api_provider` под `required-features`) |

## Бенч токен-эффективности (не часть `cargo test`)

```bash
cargo build --release
python scripts/bench.py            # импортирует ./memory, гоняет канонические запросы
python scripts/bench.py --kb ./memory --limit 5 --bin target/release/cm.exe
```

[scripts/bench.py](../scripts/bench.py) меряет токены вывода `recall` (lean vs `--explain`,
`cl100k_base` через `tiktoken`) и **recall@k-страж** (ожидаемый `origin` в top-k). tiktoken
опционален: без него колонки токенов = `n/a`, страж recall@k всё равно работает. Гонять
**до/после** изменения; гейт — recall@k не должен падать (план §5,
[docs/token-efficiency-plan.md](../docs/token-efficiency-plan.md)).

## Фичи cargo (`Cargo.toml [features]`)

| Фича | По умолчанию | Включает | Эффект |
|------|:---:|----------|--------|
| `api` | **да** (`default = ["api"]`) | `dep:ureq` | Провайдер `api` (нейро-эмбеддинги по HTTP). Включён в дефолт, чтобы переключение на нейромодель было **рантайм-конфигом**, без пересборки. |
| `pdf` | нет | `dep:pdf-extract` | Импорт/экспорт PDF. Выключен — тяжёлое дерево зависимостей. |
| `code` | нет | `dep:tree-sitter`, `dep:tree-sitter-tags` + 11 грамматик (`tree-sitter-rust/python/javascript/typescript/go/java/c/cpp/c-sharp/ruby/php`) | Граф кода (команда `cm map`): парсинг исходников tree-sitter'ом, извлечение defines/uses через `tags.scm` каждой грамматики. Выключен — тяжёлое дерево + вес бинаря (грамматики статически линкуются). |

Без фичи `api` провайдер `api` даёт понятную ошибку с подсказкой пересобрать (`embed::build`).
Без `pdf` — то же для PDF (`import::pdf_chunks`, `export::render`).
Без `code` — `cm map` даёт ту же self-heal ошибку (`code::parse` fallback под `#[cfg(not(feature="code"))]`).

**Совместимость версий tree-sitter:** ядро `tree-sitter` 0.26, грамматики — 0.23–0.25; они
не конфликтуют по ABI, потому что мост — общий крейт `tree-sitter-language` (грамматика
экспортирует `LANGUAGE: LanguageFn` + `TAGS_QUERY: &str`, ядро принимает через `.into()`).
Сборка с `code`: `cargo build --release --features code`. **Bash сознательно НЕ включён** —
у грамматики нет `tags.scm` (нет графа символов); css/html — разметка, тоже исключены.
**Добавить язык** = крейт-грамматика в `Cargo.toml` + строка в `code::LANGUAGES` (код
извлечения не меняется — поток тегов унифицирован).

## Профиль release (`Cargo.toml [profile.release]`)

`opt-level = 2`, `lto = false`, `codegen-units = 16`, `strip = true` — баланс «быстро
собирается / маленький бинарник» для короткоживущего CLI, где старт важнее пиковой
производительности.

## Дистрибуция (вместо деплоя)

У climem **нет серверного деплоя**. «Доставка» = самодостаточная папка памяти:

```bash
cm init ./ --name project-memory   # бинарник + notes/ + imports/ + models/ + store.db + config.json + .gitignore
```

`init::run` создаёт каталоги-правду `notes/` + `imports/` (+ `models/`), пустую схему
(`Store::create`, `config.version = 2`), делает self-copy
(`std::env::current_exe()` → `<folder>/cm(.exe)`), пишет **`.gitignore` В саму папку**
(игнорирует `store.db`/`*.db-wal`/`*.db-shm` + `models/`, но НЕ `notes/`/`imports/` — коммитится
правда, не индекс) и печатает готовый **указатель** для системного промпта/`CLAUDE.md`
(`help::pointer`). Success-JSON содержит ключи `notes`/`imports` (пути каталогов-правды).
Папку можно целиком скопировать или закоммитить. Существующую папку `init` не трогает
(`{"status":"already_exists"}`).

## Локальные хранилища для тестов

`.gitignore` исключает `/.testmem/` и `/.apimem/` — это scratch-папки памяти для ручных
проверок (`--dir ./.testmem`). Также игнорируются `/target`, `config.local.json`,
`*.db-wal`, `*.db-shm`.
