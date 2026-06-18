# Рецепт: добавить провайдер эмбеддингов

Пример: локальная нейромодель ONNX рядом с `local` (hashing) и `api` (HTTP). Интерфейс уже
заложен (`desc.md §3`), добавление не трогает остальную систему.

## Шаги

1. **Новый модуль** `src/embed/<name>.rs`, тип, реализующий `embed::Embedder`:
   - `fn embed(&self, text: &str) -> Result<Vec<f32>>` — **детерминированный** на провайдер.
   - `fn dim(&self) -> usize`.
   - `fn signature(&self) -> String` — уникальная строка вида `<name>:{model}:{dim}` (важно
     для детекции дрейфа, см. ниже).
2. **Подключи модуль** в `embed/mod.rs` (`mod <name>;`; если за фичей — `#[cfg(feature=…)]`).
3. **Ветка выбора** в `embed::build(cfg)`: `"<name>" => Ok(Box::new(<name>::Embedder::new(...)))`.
   Конфиг-параметры бери из `cfg.embedding` (есть `model`, `dimension`, `weights_path`,
   `endpoint`, `api_format`, `api_key_env`). Если нужны новые поля — добавь их в
   `config::Embedding` с `#[serde(default = …)]`, чтобы старые конфиги не ломались.
4. **Тяжёлые зависимости — за фичей** cargo (как `api`/`pdf`): объяви в `[features]` и под
   `#[cfg(not(feature=…))]` дай в `build` ошибку с подсказкой пересобрать.
5. **Сигнатура и дрейф.** Сигнатура должна меняться при смене модели/размерности — иначе
   `commands::warn_on_drift` не заметит, что векторы стали несравнимыми.
6. **Размерность.** `embed::cosine` возвращает 0 при разной длине — несовпадение не уронит
   поиск, но релевантность деградирует; задокументируй ожидаемый `dimension`.
7. **Обнови** `help::HELP` (блок ЭМБЕДДИНГИ), `../README.md` (если для пользователя),
   [../structure.md](../structure.md) и [../roadmap.md](../roadmap.md) (если это была
   запланированная точка расширения — перенеси из планов в реализованное).

## Проверка

```bash
cargo build --release --features <name>
cm init ./.testmem --name m --provider <name>
echo "тест" | cm remember --dir ./.testmem/m
cm recall "тест" --dir ./.testmem/m
```
