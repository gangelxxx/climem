# Рецепт: добавить формат документа для `import`

Пример: новый тип файла рядом с `.md`/`.txt`/`.html`/`.pdf`. Слои: `import` (копия оригинала
в `imports/` + диспетч по расширению в `chunks_for`) → `chunk` (нарезка) → `import::index_import`
→ `store.upsert_note` (индексация контент-адресных чанков). Оригинал в `imports/` — правда;
чанки производные, пересобираемые `reindex`.

## Шаги

1. **Извлечение текста.** Реши, как получить из файла плоский текст. Если есть структура
   (заголовки) — можно дать свой разбор; если нет — сведи к строке и используй
   `chunk::text`. Пример лёгкого извлечения без депов — `import::html_to_text`.
2. **Ветка в `import::chunks_for`** по `ext` (диспетч расширения, общий для `import` и
   `reindex`): добавь рукав в `match ext.as_str()`, верни `Vec<chunk::Chunk>`. Передавай
   `cfg.chunking.max_tokens` и `cfg.chunking.overlap`.
   - Markdown-подобное (есть заголовки) → `chunk::markdown(text, max, overlap)`.
   - Плоский текст → `chunk::text(text, max, overlap)`.
3. **`origin` чанка** — структурная ссылка на место в источнике (`chunk::markdown` ставит
   `› Заголовок`, `chunk::text` — `#N`). Сохрани осмысленный `origin`: он попадает в `notes`,
   в FTS и в вывод `recall`/`get`.
4. **Тяжёлый парсер — за фичей** cargo (как `pdf`): объяви фичу, под `#[cfg(not(feature=…))]`
   дай заглушку с `AppError::with_hint` (пример — `import::pdf_chunks`).
5. **Индексация уже общая**: `import::index_import` эмбеддит каждый чанк (`embedder.embed`) и
   пишет `kind="chunk"` с контент-адресным id (`content_hash(file_hash#n)`), `source` = путь
   копии в `imports/`, `origin`; `import_file` сверху копирует оригинал + сайдкар + `record_import`.
   Новый рукав `chunks_for` только возвращает чанки — остальное (включая `reindex`) не трогай.
6. **Обнови контракт**: список форматов в `help::HELP` (команда `import`) и, если для
   пользователя, `../README.md`. Поддерживаемые форматы упомяни в [../structure.md](../structure.md).

## Проверка

```bash
cm import ./sample.<ext> --tags test --dir ./.testmem/m
cm log --imports --dir ./.testmem/m   # увидеть число чанков
cm recall "фраза из файла" --dir ./.testmem/m
```
