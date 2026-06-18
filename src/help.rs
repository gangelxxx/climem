//! `help`: the contract lives right here inside the binary (desc.md §7). We update
//! it in lockstep with the behavior, so it can't drift the way a separate doc would.

pub const HELP: &str = r#"cm — активная память: короткоживущий CLI над хранилищем с поиском и графом.

ИДЕЯ
  Источник правды — md-файлы: notes/<id>.md (по файлу на заметку) + imports/
  (оригиналы импортов). store.db — ПРОИЗВОДНЫЙ, пересобираемый индекс (FTS5 +
  векторы + граф знаний). Заметки человекочитаемы и git-дружелюбны; БД одноразова —
  удали её и собери заново через `cm reindex` (потерять память через сбой БД нельзя).

ВЫЗОВ
  Стиль подкоманд:        cm <команда> [аргументы]
  Windows-стиль флага:    cm /recall "тема"      (ведущий / = подкоманда)
  Тело заметки — ВСЕГДА через stdin, не аргументом (снимает экранирование кавычек).
  Вывод — построчный JSON (JSONL): удобно парсить и цеплять.
  id заметки — короткий hex (например 0a1b2c3d), он же имя файла notes/<id>.md.
  Любая ошибка печатает короткую подсказку с верным примером — чинишься за шаг.

ГДЕ ХРАНИЛИЩЕ
  Папка памяти рядом с бинарником: cm + notes/ + imports/ + store.db + config.json.
  Коммить notes/ + imports/ (правда); store.db можно игнорировать (производное).
  Переопределить расположение: флаг --dir <путь> или переменная окружения MEMORY_DIR.

КОМАНДЫ
  cm init <путь> [--name <имя>] [--model M] [--provider local|api] [--dimension N]
      Развернуть самодостаточную папку памяти: копию бинарника, notes/, imports/,
      пустую store.db, config.json с дефолтами, models/. Печатает путь и указатель.
      Если папка уже существует — НЕ трогает её, печатает {"status":"already_exists"}.

  echo "<текст>" | cm remember [--tags a,b,c] [--source S] [--slug S]
                                    [--relations "предикат:цель, предикат:цель"]
      Записать заметку: пишет notes/<id>.md (точка коммита), затем индексирует.
      В теле можно ставить [[вики-ссылки]] на другие заметки. --slug задаёт
      человеко-имя, по которому на эту заметку ссылаются; --relations — рёбра графа
      (цель — slug или id:<hex>). Вывод: {"id":"<hex>"}.

  cm recall "<запрос>" [--limit N] [--explain] [--fields a,b,c]
                           [--tag T] [--origin-prefix F] [--min-score X] [--related <id>]
      Гибридный поиск (ключевые слова FTS5 + семантика-вектор, слиты через RRF).
      JSONL отсортирован по релевантности. По умолчанию N=5 и строка «худая»:
      id, kind, body + (если заданы) tags/origin/source — пустые/null опускаются.
      --explain      добавить отладочные числа score/fts/vector/graph (вклад каналов RRF).
      --fields a,b,c вернуть ровно эти поля (id,kind,body,tags,origin,source,
                     score,fts,vector,graph,created_at,preview).
      --tag T        только заметки с тегом T; --origin-prefix F — только из файла F.
      --min-score X  отбросить кандидатов со слитым score < X (RRF-числа мелкие).
      --related <id> подмешать третий канал «близость по графу» к <id> (BFS по рёбрам).
                     Включается весом search.hybrid_weights.graph (по умолчанию 0 = выкл).

  cm get <id>
      Вернуть запись целиком (JSON).

  cm list [--recent N]
      Перечислить последние записи (JSONL, body показан превью). По умолчанию 20.

  cm related <id> [--depth D] [--predicate P] [--limit N] [--fields a,b,c]
      Соседи заметки по графу (relations во frontmatter + [[вики-ссылки]] в теле).
      Обход в ширину; по умолчанию D=1, N=5. Несуществующая цель отдаётся как
      «висячая»: {"dangling":true,"name":"<цель>","predicate":...} без id.
      --predicate P  только рёбра с этим предикатом (на каждом шаге).

  cm forget <id>
      Удалить заметку: удаляет notes/<id>.md и строку индекса. Чанки импорта
      (kind=chunk) так удалить нельзя — правь imports/ и зови reindex.
      Вывод: {"deleted": true|false, "id":"<hex>"}.

  cm import <файл> [--tags a,b]
      Импорт документа: КОПИРУЕТ оригинал в imports/ (правда) + .meta.json с тегами,
      режет на чанки по структуре, индексирует. Чанки — производные, пересобираются.
      Форматы: .md (по заголовкам), .txt/.html (по абзацам + overlap), .pdf (фича pdf).
      Вывод: {"imported": "путь", "chunks": N}.

  cm reindex [--all]
      Пересобрать store.db из notes/ + imports/ (по хэшу содержимого, инкрементально).
      --all — полная пересборка: стирает производное и переэмбеддит всё.
      Вывод: {"indexed": N, "changed": M}. Чини им индекс после правок md или потери БД.

  cm export <md|json|jsonl> [--out файл] [--query "тема"]
      Выгрузить память (всю или по фильтру --query) для бэкапа/ревью/шаринга.
      Без --out печатает в stdout.

  cm log [--imports] [--recent N]
      История операций (remember/recall/import/reindex/...). С --imports — список
      импортов (источник imports/<файл>, оригинал, теги, число чанков, время).

  cm config [get <ключ> | set <ключ> <значение>]
      Без аргументов — показать config.json (секреты замаскированы).
      Ключи через точку: embedding.model, embedding.provider, search.hybrid_weights.fts ...

  cm help
      Этот текст.

ГРАФ ЗНАНИЙ (выводится из md, пересобирается в reindex)
  Во frontmatter заметки — необязательные slug (человеко-имя, по которому на неё
  ссылаются) и relations (список `предикат: цель`). В теле — [[вики-ссылки]].
  Цель ссылки разрешается по slug; префикс id: форсирует адресацию по id (id:0a1b2c3d).
  Неразрешённая цель остаётся «висячей» и оживает, когда цель появится (на reindex).

ПРИМЕРЫ
  cm init ./ --name project-memory
  echo "Решили: авторизация на JWT, refresh — 30 дней" | cm remember --tags auth,decision
  cm recall "как устроена авторизация" --limit 5
  cm related 0a1b2c3d --depth 2
  cm import ./docs/architecture.md --tags spec,architecture
  cm reindex --all
  cm export md --out memory-dump.md
  cm config set embedding.provider api
  cm config set embedding.endpoint http://localhost:11434/api/embeddings
  cm config set embedding.api_format ollama

ЭМБЕДДИНГИ (config.json → embedding)
  provider "local"  — офлайн, детерминированный (по умолчанию, без скачивания).
  provider "api"    — нейросетевой по HTTP (OpenAI-совместимый или Ollama).
                      Ключ — в переменной окружения с именем embedding.api_key_env,
                      сам ключ в конфиге не хранится.
"#;

/// The little pointer you paste into a system prompt or CLAUDE.md (desc.md §8).
pub fn pointer(exe_display: &str) -> String {
    format!(
        "В проекте есть инструмент памяти `{exe}`. Перед ответом по проекту сначала\n\
         выполняй `{exe} recall \"<тема>\"`. После значимых решений — `{exe} remember`\n\
         (тело через stdin). Полный контракт: `{exe} help`.",
        exe = exe_display
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_substitutes_exe_and_mentions_commands() {
        let p = pointer("MEMEXE");
        assert!(
            p.matches("MEMEXE").count() >= 3,
            "exe path should be substituted repeatedly"
        );
        assert!(p.contains("recall"));
        assert!(p.contains("remember"));
        assert!(p.contains("help"));
    }

    #[test]
    fn help_const_mentions_all_commands() {
        for cmd in [
            "init", "remember", "recall", "get", "list", "related", "forget", "import", "export",
            "reindex", "log", "config",
        ] {
            assert!(HELP.contains(cmd), "HELP should mention `{cmd}`");
        }
        assert!(HELP.contains("MEMORY_DIR"));
        assert!(HELP.contains("--dir"));
        // Storage-model contract: md is truth, store.db derived/rebuildable.
        assert!(HELP.contains("notes/") && HELP.contains("imports/"));
        assert!(HELP.contains("reindex"));
    }
}
