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
6. Run any command with no setup? No. First run `cm init` ONCE (from the project
   root) to create the memory folder. After that all other commands work.

================================================================================
WHICH COMMAND DO I USE? (pick by what you want to do)
================================================================================
  I want to save a fact / decision ............... remember   (text on stdin)
  cm is missing something / could be better ...... feedback   (text on stdin)
  Show me the feedback collected so far .......... feedback --list
  I want to find notes about a topic ............. recall "<topic>"
  I have an id, give me the whole note ........... get <id>
  Show me the newest notes ....................... list
  What does this note link to? ................... related <id>
  What links TO this note? (backlinks) ........... backlinks <id>
  Index source code into a dependency graph ...... map <path>
  Where is symbol X defined? ..................... map --query <name>
  Find symbols by partial name ................... map --like <substr>
  Top hub symbols (the architecture skeleton) .... map --list   (--all = every symbol)
  Who uses symbol X? ............................. map --uses <name>
  What does symbol X depend on? .................. map --calls <name>
  What does file F define? ....................... map --defines <file>
  Delete a note .................................. forget <id>
  Load a whole document (md/txt/html/pdf) ........ import <file>
  Rebuild search after editing files by hand ..... reindex
  Check & repair the memory ...................... doctor [--fix]
  Save/print a backup of everything .............. export <format>
  See what happened recently ..................... log
  Read or change settings ........................ config
  Set up a brand-new memory folder ............... init   (bare; defaults to here)
  Uninstall: restore docs, remove memory ......... deinit <path>

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
    cm init [<path>] [--name <name>] [--docs <p1,p2,...>] [--provider local|api]
            [--model <m>] [--dimension <n>] [--endpoint <url>] [--no-code]
    With NO arguments, init does the sensible default: in the current working
    directory (the project root you ran it from, no matter where the cm binary
    itself sits) it leaves cm + config.json AT THE ROOT and puts the data in a
    `memory/` subfolder, gathers the project's docs, and indexes its source code.
    So the whole setup is just:  cm init
    <path> overrides the target directory; --name overrides the data folder name
    (default `memory`).
    LAYOUT. At the root: cm(.exe) + config.json (config records data_dir=<name>, so
    the binary finds its data). In <path>/<name>/: notes/, imports/, store.db,
    models/. cm reads config.json next to itself to locate the data — no --dir
    needed once it's at the root. init also drops a snapshot manifest
    (<name>/.init-manifest.json) recording what it changed — the root .gitignore it
    touched, an AGENTS.md it created, and each imported doc's original path — so
    `deinit` can roll the project back exactly.
    IDEMPOTENT. init brings the project to a fully-initialized state and touches only
    what is MISSING. A partial layout (e.g. an empty `memory/`, or one whose derived
    store.db was deleted) is COMPLETED in place — each piece is created only if absent,
    so a good store.db and a user-edited config.json are never overwritten ({"status":
    "repaired"}). If store.db was missing and the md truth (notes/ + imports/) is still
    there, init also REBUILDS the document index from it automatically, so `recall` works
    right away without a manual `cm reindex`. A FRESH layout gives {"status":"created"} — and since `deinit` leaves
    config.json behind, re-running init after a deinit is a fresh "created", not a
    repair. Only when EVERYTHING is already present and current is it a true no-op:
    {"status":"already_exists"} (pass --docs <paths> to import more, delete the folder to
    start over, or `cm reindex` to rebuild just the index). An EXPLICIT --docs is honored
    even on a complete layout — it imports those paths into the existing store instead of
    short-circuiting. An existing config.json is always kept verbatim — re-running with
    --model/--provider/etc. on top of one leaves it unchanged (those flags only apply to
    a fresh config).
    DOCS. By default init auto-scans the target for .md files and, if it finds any,
    lists the folders they live in and asks ONE y/N before importing the lot. The
    project's ROOT entry-point files (README.md and the agent-instruction docs
    below) are NOT imported — they get a pointer wired in instead. To pick the docs
    yourself and skip the scan + prompt, pass --docs with a comma-separated list of
    folders and/or files (folders are walked recursively; a named folder's README
    IS included — you asked for it):
      cm init --docs docs,notes/spec.md
    After a successful import init also offers (y/N) to delete the originals
    (recorded in the manifest, so `deinit` puts them back).
    It also wires up the agent's instruction files if any are present
    (CLAUDE.md, AGENTS.md, AGENT.md, GEMINI.md, .cursorrules,
    .github/copilot-instructions.md): a short pointer block is APPENDED to each,
    telling the model to fetch project docs via `cm recall` instead of reading
    them whole. Re-running is safe: an identical block is left untouched, and a
    stale one (e.g. a re-init under a new --name) is refreshed in place — never
    duplicated. If NONE of those files exist, init CREATES an AGENTS.md (a short
    header + the pointer block) so a model has an entry point. An existing file is
    only edited, never overwritten; the manifest records an AGENTS.md init created
    so `deinit` removes exactly that one.
    GUIDE. init also drops a standalone CM_GUIDE.md at the root: a full, self-contained
    manual for cm (with the exact JSON each command returns) that the wired pointer
    blocks link to, so any coding agent can read it as a plain file instead of spending
    a turn on `cm help`. An existing CM_GUIDE.md is left untouched; one init created is
    recorded in the manifest and removed by `deinit`.
    CODE INDEX (on by default): init also maps the project's source into the code
    graph (same as running `cm map <path>` after init), so `cm map --query/--uses/
    --defines` work immediately. Adds {"code":{"files","symbols","edges"}} to the
    output. Needs a binary built with the `code` feature; without it, init warns
    and still scaffolds. --no-code skips this step (a docs-only setup).
    Example:  cm init                 (scaffold here + gather docs + map source)
    Example:  cm init --docs docs,notes/spec.md   (import exactly these, no prompt)
    Example:  cm init ./project-memory --name project-memory
    Example:  cm init --no-code       (docs-only, skip the code map)

  deinit  — full rollback of init: restore your project, uninstall cm.
  ------
    cm deinit [<path>] [--name <name>] [--yes]
    With NO arguments it acts on the current directory — the mirror of a bare
    `cm init`. <path> overrides the target (the same one you gave init); the data
    folder is found from config.json, so --name is only a fallback. It undoes init
    exactly, leaving only cm(.exe) and config.json:
      * strips the pointer blocks from CLAUDE.md / AGENTS.md / etc., and removes an
        AGENTS.md (and the CM_GUIDE.md) that init itself created;
      * restores every imported doc — to its original path if that spot is free,
        else under <dir>/climem/<file> (so your newer file is never overwritten);
        docs added later via `cm import` go to docs/climem/;
      * restores the root .gitignore to its pre-init bytes, or deletes it if init
        created it;
      * removes the data folder (memory/) ENTIRELY — store.db, notes/, imports/,
        models/, the manifest.
    The cm binary stays (a running .exe can't delete itself on Windows; remove it
    by hand). With no manifest (a store from before this existed) it falls back:
    imported copies are restored by name into docs/climem/, and only cm's own block
    is stripped from the root .gitignore.
    Asks for confirmation first; --yes (or --force) skips the prompt. A piped/
    non-interactive stdin declines safely (nothing is touched without a yes).
    PRINTS: {"deinit":"<folder>","unwired_files":N,"restored_docs":[…],
             "gitignore":"restored|deleted|untouched","folder_removed":bool,
             "manifest":bool}.
    Example:  cm deinit ./project-memory --yes

  remember  — save one note. THE TEXT COMES FROM STDIN.
  --------
    echo "<your text>" | cm remember [--tags a,b,c] [--source <s>]
                                     [--slug <name>] [--relations "p:t, p:t"]
                                     [--code-refs "sym, sym"]
    Writes the note to notes/<id>.md, then indexes it for search.
    --tags a,b,c   comma-separated labels you can later filter on.
    --source <s>   where the fact came from (free text), kept with the note.
    --slug <name>  a human name OTHER notes can link to instead of the hex id.
    --relations    graph links, as "predicate:target, predicate:target".
                   The target is a slug, or id:<hex> to point at an exact id.
    --code-refs    names of source-code symbols this note documents, comma-
                   separated (e.g. "validate_token, JwtAuth"). They anchor the note
                   to the code graph: `recall` resolves each against `cm map` and
                   returns it with its live path/line, or flags it resolved:false
                   when the symbol is gone — so a doc can show whether its code still
                   exists. Anchored by NAME, so it survives line edits.
    Inside the text you may write [[name]] to link to another note (name is its
    slug, or id:<hex>).
    PRINTS:  {"id":"<hex>"}   <- save this id; it names the note's file.
    Example:
      echo "Decided: auth uses JWT, refresh token lasts 30 days." \
        | cm remember --tags auth,decision --slug jwt-auth

  feedback  — tell cm's maintainer what the tool is missing or could do better.
  --------
    echo "<what's missing / to improve>" | cm feedback [--tags a,b]
    cm feedback --list [--recent N]
    Use this whenever cm ITSELF gets in your way: a flag you wished existed,
    output that was awkward to parse, a command that did the wrong thing, a gap
    in this help. THE TEXT COMES FROM STDIN, exactly like remember. Your note is
    saved into THIS project's memory as an ordinary note tagged "cm-feedback"
    (and stamped with the cm version), so a maintainer can find it later with
    `cm recall "<topic>" --tag cm-feedback` or `cm feedback --list`. Add your own
    --tags to mark the area it's about, e.g. --tags map.
    --list   instead of writing, print the feedback gathered so far, newest
             first, as compact previews. --recent N caps how many (default 50).
    PRINTS (when writing):  {"id":"<hex>"}   (the note id, same as remember).
    Example:
      echo "cm map --uses misses trait impls; a --kind filter would help" \
        | cm feedback --tags map
      cm feedback --list

  recall  — search notes by topic. THIS IS THE MAIN READ COMMAND.
  ------
    cm recall "<query>" [--limit N] [--budget C | --full] [--explain]
                        [--fields a,b,c] [--tag T] [--origin-prefix F]
                        [--min-score X] [--related <id>]
    Searches by keywords AND by meaning at once, blends the two, and prints the
    best matches as JSONL, best first. You do NOT need to understand the
    blending; just read the printed lines.
    --limit N        how many results (default 5).
    --budget C       cap each result's body at C characters (default 500). A short
                     note prints whole as "body"; a longer one comes back as
                     "preview" (the first C chars) plus "chars" (its full length) —
                     fetch the rest with `cm get <id>`. This preview-first default
                     keeps the read path lean; tune it with
                       cm config set search.recall_body_chars C
    --full           print whole bodies, no preview (same as --budget 0).
    --fields a,b,c   print ONLY these fields (overrides --budget/--full; "body"
                     means the whole body). Valid names:
                       id, kind, body, tags, origin, source,
                       score, fts, vector, graph, created_at, preview, chars
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
    DEFAULT OUTPUT per line: {"id","kind","body"} for a short note, or
    {"id","kind","preview","chars"} when the body is over budget; plus
    tags/origin/source when the note has them. Empty fields are omitted to save
    space. When you see "preview"+"chars", `cm get <id>` returns the full body.
    If a note has code anchors (see `remember --code-refs`), the row also carries
    "code_refs": [{"name","resolved",...}] — each resolved one adds its live
    "path"/"line"; "resolved":false means that documented symbol is gone (stale).
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

  doctor  — check the memory's health, show stats, and (with --fix) repair it.
  ------
    cm doctor [--fix] [--text]
    A read-only health check by default: it verifies the layout and the derived
    store.db against the md source of truth, prints project statistics, and for every
    problem prints the exact command that fixes it. It reads nothing and changes
    nothing unless you pass --fix. Run it after a move or a lost index, or any time
    you want a one-glance status.
    --text prints a READABLE report (aligned ✓/⚠/✗ checks + a formatted stats block)
    instead of the default JSONL — use it when a human is reading; keep the JSONL
    default for scripts/agents.
    CHECKS include: config.json parses; config's data_dir still resolves (catches a
    RENAMED/moved memory folder); store.db is present and opens (catches a DELETED
    index); the hand-kept notes<->search-index sync; embedder signature/dimension
    drift (vectors that no longer compare); store-vs-disk drift (a note in the index
    with no notes/<id>.md, or an unindexed file); slug collisions; the code graph; note
    text corrupted by a non-UTF-8 shell (non-ASCII lost to '?' before cm saw it — this
    is report-only, the loss is irreversible); and that the agent instruction files
    (CLAUDE.md/AGENTS.md/…) carry a current pointer to a cm binary that actually EXISTS
    at the project root.
    --fix applies ONLY the safe, idempotent repairs, by delegating to existing
    commands: it creates any missing data dirs and rebuilds the index with `reindex`
    (or `reindex --all` when an embedder change means everything must be re-embedded),
    and, when a renamed data folder is unambiguous, points config.data_dir back at it.
    It never rewrites your tuned config otherwise, never deletes a note, and never
    touches your instruction files — run `cm init` for the scaffold. Each finding is
    one JSON line, e.g.:
      {"check":"store_present","status":"error","fixable":true,"fix":"cm reindex"}
    followed by statistics ({"stat":"notes","value":42}) and a summary
    ({"doctor":"<dir>","errors":1,"warnings":0,"info":1,"fixed":0}). Findings are data,
    so doctor still exits 0 when it finds problems — read the summary counts to react.
    Example:  cm doctor
    Example:  cm doctor --fix     (rebuild a lost store.db, or adopt a renamed folder)

  map  — build & query a SEPARATE knowledge graph of your SOURCE CODE.
  ---
    This graph is NOT your notes. It is a code map: which symbols (functions,
    types, classes...) exist, where each is defined, and which symbol uses which.
    It lives in its own tables and never mixes into recall/related. Use it to
    answer structural questions about a codebase precisely, in one query.
    (Only works if cm was built with the `code` feature; otherwise it tells you
    how to rebuild. Languages: Rust, Python, JS, TS, Go, Java, C, C++, C#, Ruby,
    PHP — picked by file extension.)

    INDEX a source tree (run this first, re-run after edits — it's incremental):
      cm map <path> [--lang <name>] [--exclude <substr>]
      --lang <name>     index only this language (e.g. rust, python, typescript).
      --exclude <substr>  skip any path containing this substring.
      Skips build/vendor/generated dirs (target, node_modules, .git, dist, build,
      vendor, __pycache__, .NET obj/ and bin/, ...) automatically. Source files are
      NOT copied anywhere; the graph is a rebuildable cache over your working tree.
      PRINTS: {"mapped","scanned","changed","files","symbols","edges"}.
      cm remembers this path: if a later query finds NOTHING, cm silently re-maps
      this same tree (it may be stale after edits) and retries before answering —
      so you don't have to re-map by hand after editing. A note on stderr tells you
      when it did.

    QUERY the graph (read-only). Symbol-listing modes accept --kind <k> to keep
    only one kind (function, method, class, interface, module). Test code is HIDDEN
    by default (symbols in tests/ files or inside an inline test module); pass
    --tests to include it:
      cm map --query <name>     where a symbol is DEFINED (exact name).
        -> one line per definition: {"name","kind","path","line","signature"}.
      cm map --like <substr>    symbols whose NAME contains <substr> (the structural
        "show me everything called *config*"). Same output shape as --query.
      cm map --list             the graph's HUB symbols — ranked by how connected
        they are (`degree`), capped to the top 30. This is the architecture's
        skeleton: read these first. -> {"name","kind","path","line","signature",
        "degree"}. --limit N resizes the cap; --all lists EVERY symbol (a full table
        of contents, no ranking); --kind narrows (e.g. list the top types).
      cm map --uses <name>      which symbols USE this one (its callers).
        -> {"name","kind","path","line","def_line"}  (line = the use site).
      cm map --calls <name>     what this symbol DEPENDS ON (its outgoing calls).
        -> {"calls","line","resolved"}. By default only in-project calls are shown
        (resolved=true); add --external to also list stdlib/3rd-party names.
      cm map --defines <file>   which symbols a file defines (path matched
        leniently: `code.rs` and `src/code.rs` both work).
        -> same shape as --query.
    NOTE: --uses/--calls match by NAME (no scope resolution), so two unrelated
    symbols sharing a name are merged, and a same-named call may be a false hit.
    Ubiquitous stdlib method names (map, filter, unwrap, get, clone, ...) are NOT
    resolved to same-named project symbols, so they don't show up as fake deps.
    Examples:
      cm map ./src --lang rust
      cm map --query upsert_note
      cm map --like code --kind function
      cm map --list                   (top 30 hub symbols, most-connected first)
      cm map --list --all --kind class   (every type; --tests to include test code)
      cm map --uses Store
      cm map --calls reindex_notes

  export  — write out a copy of the memory (for backup or sharing).
  ------
    cm export <md|json|jsonl> [--out <file>] [--query "<topic>"]
              [--limit N] [--tag T] [--origin-prefix P] [--min-score N]
    Pick a format. With --out it writes a file; without --out it prints to the
    screen. With --query it exports only notes matching that topic; the same
    pre-filters as `recall` apply (--tag/--origin-prefix/--min-score), and
    --limit caps the result (default 50).
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
WHEN TO USE THE CODE MAP (read this — it saves you many tool calls)
================================================================================
  `cm map` answers STRUCTURAL questions about source code precisely, in ONE call,
  where a text search (grep) would take several tries and still be noisy. The
  project is indexed by `cm init` (or re-run `cm map <path>` after edits); prefer
  the map for:

    "Where is X defined?"            cm map --query X        (1 exact answer, not
                                                              200 substring hits)
    "What's in this file / module?"  cm map --defines F      (a clean API list;
                                                              test code is hidden)
    "Who calls / uses X?"            cm map --uses X          (each caller named,
                                                              attributed to its fn)
    "What does X depend on?"         cm map --calls X         (its real in-project
                                                              calls; stdlib hidden)
    "Is there a symbol like ...?"    cm map --like part       (fuzzy name search)

  WHY it beats grep for these: it knows symbol boundaries, so it never matches a
  name inside a string, a comment, or a longer word; it tells definitions apart
  from uses and gives you the direction (who-calls vs what-it-calls); and it
  attributes each use to the enclosing function. One `--uses`/`--calls` replaces
  reading scattered grep lines and scrolling to see which function each is in.

  WHEN TO STILL USE GREP (be honest about the limits):
    * Free text, not a symbol: a string literal, an error message, a TODO, a
      config key, a value in .md/.json/.toml. The map only knows code symbols.
    * Symbols with very common / overloaded names (new, get, run, handle, or a
      name defined in several files). The map resolves uses BY NAME with no scope
      analysis, so it merges same-named symbols and can miss or misattribute —
      grep is more trustworthy there.
  Staleness is NO LONGER a reason to skip the map: a query that hits nothing
  triggers an automatic re-map of the indexed tree and a retry, so just-edited
  code is picked up for you (a stderr note says so). An empty answer after that
  means the symbol really isn't there — trust it.

  RULE OF THUMB: unique-ish symbol name + structural question -> `cm map` (it
  self-refreshes). Common name or free text -> grep.

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
  `cm init` lays out two places. At the PROJECT ROOT (where you put the binary):
    cm(.exe)      this binary, stays where you dropped it
    config.json   settings — includes data_dir, the link to the data folder below
  And in the DATA folder (memory/ by default, set by config's data_dir):
    notes/        one <id>.md file per note   <- REAL MEMORY, keep it
    imports/      copies of imported documents <- REAL MEMORY, keep it
    store.db      the search index             <- rebuildable, can be deleted
    models/       embedding weights            <- re-downloadable
  Commit config.json + the data folder's notes/ and imports/ to git; store.db is
  ignored and rebuilt with `cm reindex`. cm finds its data by reading config.json
  next to the binary (data_dir points at the data folder); --dir <path> or
  MEMORY_DIR override the location of the folder that holds config.json.
  (Older single-folder stores still work: a config without data_dir means
  everything sits in one folder, as before.)

================================================================================
A TYPICAL SESSION, START TO FINISH
================================================================================
  cm init                  (from the project root: scaffold + docs + code map)
  echo "Decided: auth uses JWT, refresh token lasts 30 days." \
    | cm remember --tags auth,decision --slug jwt-auth
  cm recall "how does authentication work" --limit 5
  cm get 0a1b2c3d
  cm related 0a1b2c3d --depth 2
  cm import ./docs/architecture.md --tags spec,architecture
  cm reindex
  cm export md --out memory-dump.md
  echo "wish cm recall had a --since <date> filter" | cm feedback
"#;

/// The little pointer you paste into a system prompt or CLAUDE.md (desc.md §8).
/// Covers the two everyday loops a model needs: memory (recall/remember) and the
/// code map for structural navigation (init indexes the source tree by default). It
/// shows the JSON each call returns, so a model can act without first running `cm
/// help`; the full manual is the generated `CM_GUIDE.md`.
pub fn pointer(exe_display: &str) -> String {
    format!(
        "This project has `{exe}` (memory + code map). Each call prints JSONL — read it.\n\
         • MEMORY: before answering a project question, `{exe} recall \"<topic>\"` first\n\
         (→ {{\"id\",\"kind\",\"body\"}} per match); after a decision —\n\
         `echo \"<fact>\" | {exe} remember` (text via stdin → {{\"id\"}}).\n\
         • CODE (sources indexed; self-refreshes on a stale hit — no manual re-map): for\n\
         STRUCTURAL questions use it instead of grep — `{exe} map --query <name>`\n\
         (where defined → {{name,kind,path,line,signature}}), `--uses <name>` (who calls\n\
         it → {{…,def_line}}), `--calls <name>` (deps → {{calls,line,resolved}}),\n\
         `--defines <file>` (a file's API). Precise for unique names; common names/text — grep.\n\
         • FEEDBACK: if `{exe}` is missing something or gets in your way, say so — it's saved\n\
         for the maintainer: `echo \"<what's missing / to improve>\" | {exe} feedback` (→ {{\"id\"}}).\n\
         Full manual: open `CM_GUIDE.md` (or `{exe} help`).",
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
        assert!(p.contains("map")); // also points at the code map
        assert!(p.contains("feedback")); // and at the feedback channel
        assert!(p.contains("help"));
    }

    #[test]
    fn help_const_mentions_all_commands() {
        for cmd in [
            "init",
            "deinit",
            "remember",
            "feedback",
            "recall",
            "get",
            "list",
            "related",
            "backlinks",
            "forget",
            "import",
            "export",
            "reindex",
            "doctor",
            "map",
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
