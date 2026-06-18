//! `help`: the contract lives right here inside the binary (desc.md §7). We update
//! it in lockstep with the behavior, so it can't drift the way a separate doc would.

pub const HELP: &str = r#"cm — a memory tool for an AI agent. It stores notes on disk and searches them.

================================================================================
READ THIS FIRST (the 6 rules that matter most)
================================================================================
1. WRITE the note text on STDIN, never as an argument.
     RIGHT:  echo "your text here" | cm remember
     WRONG:  cm remember "your text here"     <- this fails, text is ignored.
2. Each command PRINTS one JSON object per line (this format is called JSONL).
   Read that JSON to get the result. Parse it; do not guess.
3. A note's id is a short hex string like 0a1b2c3d (only 0-9 and a-f).
   The same id is the file name: notes/0a1b2c3d.md.
4. To SAVE a fact, use `remember`. To FIND a fact, use `recall`. That is the
   main loop: recall before you answer, remember after you decide something.
5. If a command fails, it prints `error: ...` and one correct example to stderr.
   Read the example, copy its shape, run again. One fix is usually enough.
6. Run any command with no setup? No. First run `cm init <path>` ONCE to create
   the memory folder. After that all other commands work.

================================================================================
WHICH COMMAND DO I USE? (pick by what you want to do)
================================================================================
  I want to save a fact / decision ............... remember   (text on stdin)
  I want to find notes about a topic ............. recall "<topic>"
  I have an id, give me the whole note ........... get <id>
  Show me the newest notes ....................... list
  What does this note link to? ................... related <id>
  What links TO this note? (backlinks) ........... backlinks <id>
  Delete a note .................................. forget <id>
  Load a whole document (md/txt/html/pdf) ........ import <file>
  Rebuild search after editing files by hand ..... reindex
  Save/print a backup of everything .............. export <format>
  See what happened recently ..................... log
  Read or change settings ........................ config
  Set up a brand-new memory folder ............... init <path>

================================================================================
HOW TO CALL IT
================================================================================
  Normal style:    cm <command> [arguments]      e.g.  cm recall "auth"
  Windows style:   cm /recall "auth"             (a leading / also works)
  Both styles are equal. Use whichever you like.

  THE STORE LOCATION. Every command needs to know which memory folder to use.
  It is chosen in this order:
    1. --dir <path>      flag on the command line (highest priority)
    2. MEMORY_DIR        environment variable
    3. otherwise, the folder where the cm binary itself sits.
  Example: cm recall "auth" --dir ./project-memory

================================================================================
COMMANDS (full reference)
================================================================================

  init  — create a new memory folder. Run this once before anything else.
  ----
    cm init <path> [--name <name>] [--provider local|api] [--model <m>]
                   [--dimension <n>] [--endpoint <url>]
    Creates <path>/ with: a copy of the cm binary, an empty notes/ folder, an
    empty imports/ folder, an empty store.db index, and a config.json.
    If the folder already exists it is LEFT ALONE and you get
      {"status":"already_exists"}.
    If <path> (or any subfolder) already holds .md files, init offers (y/N) to
    import the whole tree at once, then to delete the originals.
    It also wires up the agent's instruction files if any are present
    (CLAUDE.md, AGENTS.md, AGENT.md, GEMINI.md, .cursorrules,
    .github/copilot-instructions.md): a short pointer block is APPENDED to each,
    telling the model to fetch project docs via `cm recall` instead of reading
    them whole. Re-running is safe: an identical block is left untouched, and a
    stale one (e.g. a re-init under a new --name) is refreshed in place — never
    duplicated. init never creates a file that isn't already there.
    Example:  cm init ./project-memory --name project-memory

  remember  — save one note. THE TEXT COMES FROM STDIN.
  --------
    echo "<your text>" | cm remember [--tags a,b,c] [--source <s>]
                                     [--slug <name>] [--relations "p:t, p:t"]
    Writes the note to notes/<id>.md, then indexes it for search.
    --tags a,b,c   comma-separated labels you can later filter on.
    --source <s>   where the fact came from (free text), kept with the note.
    --slug <name>  a human name OTHER notes can link to instead of the hex id.
    --relations    graph links, as "predicate:target, predicate:target".
                   The target is a slug, or id:<hex> to point at an exact id.
    Inside the text you may write [[name]] to link to another note (name is its
    slug, or id:<hex>).
    PRINTS:  {"id":"<hex>"}   <- save this id; it names the note's file.
    Example:
      echo "Decided: auth uses JWT, refresh token lasts 30 days." \
        | cm remember --tags auth,decision --slug jwt-auth

  recall  — search notes by topic. THIS IS THE MAIN READ COMMAND.
  ------
    cm recall "<query>" [--limit N] [--explain] [--fields a,b,c]
                        [--tag T] [--origin-prefix F] [--min-score X]
                        [--related <id>]
    Searches by keywords AND by meaning at once, blends the two, and prints the
    best matches as JSONL, best first. You do NOT need to understand the
    blending; just read the printed lines.
    --limit N        how many results (default 5).
    --fields a,b,c   print ONLY these fields. Valid names:
                       id, kind, body, tags, origin, source,
                       score, fts, vector, graph, created_at, preview
    --explain        also print the relevance numbers (score/fts/vector/graph).
    --tag T          only notes carrying tag T.
    --origin-prefix F  only chunks whose source file path starts with F.
    --min-score X    drop matches weaker than X (the scores are small numbers).
    --related <id>   also favor notes near <id> in the graph. OFF BY DEFAULT:
                     it does nothing unless the graph channel has weight. Turn it
                     on once with:
                       cm config set search.hybrid_weights.graph 0.3
                     How far it reaches (default 2 hops) and an optional predicate
                     filter live in config too (search.graph_depth,
                     search.graph_predicate).
    DEFAULT OUTPUT per line: {"id","kind","body"} plus tags/origin/source when
    the note has them. Empty fields are omitted to save space.
    Example:  cm recall "how does authentication work" --limit 5

  get  — fetch ONE note in full, by id.
  ---
    cm get <id>
    PRINTS the whole record: {"id","kind","body","tags","source","origin",
    "created_at"}. If the id does not exist: {"found":false,"id":"<id>"}.
    Example:  cm get 0a1b2c3d

  list  — show the most recent notes (bodies shown as short previews).
  ----
    cm list [--recent N]          (default N = 20)
    Example:  cm list --recent 10

  related  — walk the graph: which notes link to/from this one.
  -------
    cm related <id> [--depth D] [--predicate P] [--limit N] [--fields a,b,c]
    Follows the note's relations and [[links]] outward.
    --depth D      how many hops to follow (default 1).
    --predicate P  only follow links whose predicate is P.
    --limit N      max neighbors to return (default 5).
    A link to a note that does not exist yet comes back as a "dangling" target:
      {"dangling":true,"name":"<target>","predicate":"...","distance":N}
    It has no id. It will resolve on its own once that note gets written and you
    run `reindex`.
    Example:  cm related 0a1b2c3d --depth 2

  backlinks  — the reverse of related: which notes link TO this one.
  ---------
    cm backlinks <id> [--predicate P] [--limit N]
    The graph is directed: `related <id>` shows what <id> points at; `backlinks
    <id>` shows who points at <id>. One hop. Each line:
      {"id","kind","predicate","preview"}.
    --predicate P  only count links made with predicate P.
    --limit N      max backlinks to return (default 5).
    Example:  cm backlinks 0a1b2c3d

  forget  — delete one note (the file and its search entry).
  ------
    cm forget <id>
    Deletes notes/<id>.md and removes it from search.
    PRINTS: {"deleted":true|false,"id":"<id>"}.
    NOTE: you cannot forget a piece of an imported document (kind="chunk").
    To remove those, edit/delete the file under imports/, then run `reindex`.
    Example:  cm forget 0a1b2c3d

  import  — load a whole document into memory, split into searchable pieces.
  ------
    cm import <file> [--tags a,b]
    Copies the original file into imports/ (kept as the source of truth), then
    splits it into chunks and indexes each chunk.
    Supported: .md (split by heading), .txt and .html (split by paragraph),
    .pdf (only if cm was built with the pdf feature).
    PRINTS: {"imported":"<file>","chunks":N}.
    Example:  cm import ./docs/architecture.md --tags spec,architecture

  reindex  — rebuild the search index from the files on disk.
  -------
    cm reindex [--all]
    Use this after you EDIT notes/*.md or imports/* by hand, or if store.db was
    deleted or corrupted. Normally it only re-processes changed files.
    --all   throw the whole index away and rebuild everything from scratch.
    PRINTS: {"indexed":N,"changed":M}.
    The files in notes/ and imports/ are the real memory; store.db is just a
    rebuildable index, so this command can always restore search from the files.
    Example:  cm reindex

  export  — write out a copy of the memory (for backup or sharing).
  ------
    cm export <md|json|jsonl> [--out <file>] [--query "<topic>"]
    Pick a format. With --out it writes a file; without --out it prints to the
    screen. With --query it exports only notes matching that topic.
    Example:  cm export md --out memory-dump.md

  log  — show recent activity, or the list of imported documents.
  ---
    cm log [--recent N]      recent operations (remember/recall/import/...).
    cm log --imports         the documents you imported (source, tags, chunks).
    Example:  cm log --recent 20

  config  — read or change settings in config.json.
  ------
    cm config                          show the whole config (secrets hidden).
    cm config get <key>                show one value.
    cm config set <key> <value>        change one value.
    Keys use dots, e.g.:  embedding.provider , embedding.model ,
    embedding.endpoint , search.hybrid_weights.fts ,
    search.hybrid_weights.graph , search.graph_depth , search.graph_predicate
    Example:  cm config set embedding.provider api

  help  — print this text.
  ----
    cm help

================================================================================
THE KNOWLEDGE GRAPH (how notes link together)
================================================================================
  A note can point at other notes. There are two ways to make a link:
    * In the body text:           write [[name]]   (name = a slug, or id:<hex>)
    * In the note's settings:      a slug (its own name) and relations
                                   (a list of "predicate: target").
  A target written as plain text is matched against note slugs. Prefix it with
  id: to point at an exact id, e.g. id:0a1b2c3d.
  If the target does not exist yet, the link is kept as "dangling" and starts
  working automatically once that note is written and you run `reindex`.
  Links are DIRECTED (A -> B means "A points at B"). Walk them forward with
  `cm related <id>`, and backward (who points at this note?) with
  `cm backlinks <id>`. Deleting a note turns links that pointed at it back into
  dangling ones (the link is still authored in the other note's md).

================================================================================
EMBEDDINGS (how meaning-based search is computed; in config.json)
================================================================================
  config key embedding.provider chooses the engine:
    "local"  (default) — works offline, no downloads, no network. Good enough
                         for keyword-and-shape matching.
    "api"    — calls a neural model over HTTP (OpenAI-compatible, or Ollama).
               The API key is read from an ENVIRONMENT VARIABLE; only the NAME
               of that variable is stored in config (key embedding.api_key_env).
               The key itself is never written to disk.
  If you switch provider or model on a store that already has notes, run
  `cm reindex --all` so all notes are re-embedded the same way.

================================================================================
WHERE FILES LIVE
================================================================================
  A memory folder contains:
    cm(.exe)      a copy of this binary (so the folder is self-contained)
    notes/        one <id>.md file per note   <- REAL MEMORY, keep it
    imports/      copies of imported documents <- REAL MEMORY, keep it
    store.db      the search index             <- rebuildable, can be deleted
    config.json   settings
  Commit notes/ and imports/ to git. store.db can be ignored and rebuilt with
  `cm reindex`. Point cm at a folder with --dir <path> or the MEMORY_DIR env var.

================================================================================
A TYPICAL SESSION, START TO FINISH
================================================================================
  cm init ./project-memory --name project-memory
  echo "Decided: auth uses JWT, refresh token lasts 30 days." \
    | cm remember --tags auth,decision --slug jwt-auth
  cm recall "how does authentication work" --limit 5
  cm get 0a1b2c3d
  cm related 0a1b2c3d --depth 2
  cm import ./docs/architecture.md --tags spec,architecture
  cm reindex
  cm export md --out memory-dump.md
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
            "init",
            "remember",
            "recall",
            "get",
            "list",
            "related",
            "backlinks",
            "forget",
            "import",
            "export",
            "reindex",
            "log",
            "config",
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
