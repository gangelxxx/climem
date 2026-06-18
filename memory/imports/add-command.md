# Рецепт: добавить подкоманду CLI

Пример: команда уровня `remember`/`recall`. Слои: `cli` → `main` (dispatch) → `commands` →
`store`/`output` → `help`.

## Шаги

1. **Хендлер в `commands.rs`.** Функция `pub fn <cmd>(p: &Parsed, ctx: &Ctx) -> Result<()>`.
   Открой что нужно: `let (store, cfg) = ctx.open()?;` (или только `ctx.require_store()?` для
   операций без эмбеддера). Читай аргументы через `p.arg(i)` / `p.value("flag")` / `p.has`.
   Тело произвольного текста — **через stdin** (`read_stdin`), не аргументом.
2. **Вывод — JSONL** через `output::print_line(&json!({…}))`. **Заметка пишется сперва в md**
   (`notes/<id>.md` — точка коммита, как `commands::remember`: минти id `store.fresh_id()`,
   `note::render`, запись файла), и только потом индексируется в `store.db` best-effort
   (`store.upsert_note`, пишет и в `notes_fts`; `store::insert_note` — тест-хелпер). Путь
   удаления тоже должен трогать FTS и md (`store.forget` + remove файла, как `commands::forget`).
   `store.db` производный — всё, что пишешь в индекс, должно быть выразимо в md (см.
   [../notes/decisions.md](../notes/decisions.md)).
3. **Журналируй**: `store.log_op("<cmd>", Some(detail))?;` — единообразно со всеми командами.
4. **Зарегистрируй в диспетчере** `main::dispatch`: ветка `"<cmd>" => commands::<cmd>(p, ctx)`.
   Если команда не требует существующего хранилища (как `init`) — добавляй в `main::run` до
   `resolve_dir`, иначе в `dispatch`.
5. **Value-флаги** новой команды добавь в `cli::VALUE_FLAGS`, иначе `--flag value` распарсится
   неверно (см. [../conventions.md](../conventions.md)).
6. **Обнови контракт** `help::HELP` (блок КОМАНДЫ + при необходимости ПРИМЕРЫ) — в том же
   изменении (инвариант «help = контракт», [../notes/decisions.md](../notes/decisions.md)).
7. **Ошибки** ввода давай через `AppError::with_hint(msg, "верный пример")` — самоисцеление.
8. Прогон: `cargo build --release` и ручная проверка на `--dir ./.testmem`.

## Не забыть

- JSONL в stdout, нарратив/подсказки — в stderr.
- Поддержи и `/`-стиль вызова — он работает автоматически через `Parsed`, специальных правок
  не требует.
- Обнови [../structure.md](../structure.md) (таблица символов модуля `commands.rs`) при
  заметном расширении контракта.
