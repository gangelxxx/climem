# climem — active memory for LLMs

![Rust](https://img.shields.io/badge/Rust-2021-orange?logo=rust&logoColor=white)
![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)
![Status](https://img.shields.io/badge/version-0.1.0-brightgreen)
![Single binary](https://img.shields.io/badge/runtime-none-lightgrey)

**climem** gives a language model a memory it can actually *reach for*. It's a single small
Rust binary — `cm` (`cm.exe` on Windows) — that keeps notes as plain Markdown files on disk
and lets a model (or you) **write** to them, **search** them, and **walk the links** between
them, all from the command line.

The problem it solves: an LLM's context window is short and forgetful. climem keeps the
durable stuff — decisions, facts, imported docs — *outside* the window, on disk, and the
model pulls in only what's relevant to the question in front of it.

```text
              ┌─────────────────────────────┐
   model  ──▶ │  cm recall "how does auth…"  │ ──▶  the 5 most relevant notes (JSONL)
              └─────────────────────────────┘
              ┌─────────────────────────────┐
   model  ──▶ │  cm remember  (body ⇠ stdin) │ ──▶  notes/0a1b2c3d.md  +  index
              └─────────────────────────────┘
```

There's no server and no daemon. **Every command is a fresh, short-lived process**: open the
store, do one thing, print a line of JSON, exit. Nothing keeps running in the background, so
there's nothing to crash mid-session.

---

## The big idea: Markdown is the truth, the database is disposable

This is the part worth understanding before anything else.

- **Your notes are plain `.md` files.** One note = one file, `notes/<id>.md`, with a small
  YAML-ish frontmatter block and a Markdown body. Imported documents are copied verbatim into
  `imports/`. These two folders are the **source of truth** — human-readable, diff-able, and
  git-friendly. Commit them.
- **`store.db` (SQLite) is a derived index.** It's built *from* the Markdown and exists only
  to make search fast. It holds three things: a full-text index (FTS5), a vector index
  (embeddings as f32 blobs, compared by cosine similarity), and a knowledge graph (edges
  between notes).
- **The database is throw-away.** Delete `store.db`, run `cm reindex`, and it rebuilds itself
  from the Markdown. You can `.gitignore` it. **You cannot lose your memory to a database
  failure** — the truth was always the files.

This inversion (files are canonical, the DB is a cache) is what makes climem safe to trust
and easy to version.

---

## How search works

`cm recall "<query>"` runs **two searches at once** and blends them:

1. **Keywords** — FTS5 full-text search (BM25 ranking) for exact term matches.
2. **Meaning / shape** — brute-force cosine similarity over embedding vectors, for things
   that are *similar* even when the words don't match exactly.

The two ranked lists are fused with **RRF (Reciprocal Rank Fusion)** using weights from your
config. You never see the machinery — `recall` just returns the best results, sorted, as a
lean line of JSON per hit. The hybridity is deliberately hidden behind one command.

> **Why bother with the vector half?** Take Russian (or any inflected language). FTS5 does no
> stemming, so a query for *авториз**ация*** won't match *авториз**ацию***. The vector channel
> uses character 3-grams, catches the shared morpheme, and floats the right note to the top.
> Exact hits come from FTS; "close in meaning or form" comes from the vector. Together they
> cover what either misses.

---

## Embeddings: offline by default, neural when you want it

The embedding provider is swappable via `config.json` (`embedding.provider`):

- **`local`** (default) — a deterministic, offline embedder built from word + character-3gram
  hashing. No downloads, no network, works anywhere. It captures lexical and morphological
  similarity rather than deep semantics — and the char-grams are what make it handle inflected
  languages gracefully.
- **`api`** — calls an OpenAI-compatible or Ollama HTTP endpoint for real neural embeddings.
  The API key is read **from an environment variable**; only the *name* of that variable lives
  in your config, never the key itself.

Switching between them is a config change, not a rebuild — the `api` feature is compiled in by
default.

---

## Install / build

You need **Rust (stable)** and a **C toolchain** (MSVC on Windows, gcc on Linux) — SQLite is
compiled from source and bundled straight into the binary, so there's no separate SQLite to
install.

```bash
cargo build --release                  # → target/release/cm(.exe)   (api feature on by default)
cargo build --release --features pdf   # + PDF import/export
cargo build --release --features code  # + source-code graph (`cm map`, 11 languages)
```

The result is one self-contained executable. Drop it wherever you like.

---

## Quickstart

```bash
# 1. Scaffold. With NO arguments, init defaults to: keep cm + config.json at the project
#    root, put the data in a `memory/` subfolder, gather the project's docs, and index its
#    source code. config.json's data_dir links the two, so the binary finds its data.
cm init

# 2. Remember something — the note BODY comes from stdin; prints {"id":"<hex>"}
echo "Decided: auth via JWT, refresh tokens last 30 days. See [[db-schema]]." \
  | cm remember --tags auth,decision

# 3. Recall it (hybrid keyword + semantic search)
cm recall "how does authentication work" --limit 5

# 4. Walk the knowledge graph
cm related 0a1b2c3d --depth 2

# 5. Import a document (the original is copied into imports/ as the source of truth)
cm import ./docs/architecture.md --tags spec,architecture

# 6. Rebuild the index from Markdown (after editing files, or if store.db is gone)
cm reindex

# 7. Dump everything back out
cm export md --out memory-dump.md
```

`cm init` prints a ready-to-paste **pointer** — a one-line instruction you drop into a system
prompt or `CLAUDE.md` so the model knows the tool exists and calls `recall` before answering /
`remember` after decisions.

> **`init` is idempotent — it completes, never clobbers.** Re-running brings the project to a
> fully-initialized state and touches only what's **missing**: a partial layout (an empty
> `memory/`, or one whose derived `store.db` was deleted) is finished in place
> (`{"status":"repaired"}`) — and if `store.db` was gone but the md truth (`notes/` + `imports/`) is
> still there, init **rebuilds the document index from it automatically**, so `recall` works at once
> without a manual `cm reindex`. A fresh one gives `{"status":"created"}`, and a layout that's already
> complete is a true no-op (`{"status":"already_exists"}` — delete the folder to start over, or run
> `cm reindex` to rebuild just the index). A good `store.db` and an **edited `config.json` are kept
> verbatim** (re-running with `--model`/`--provider`/… on top of an existing config leaves it
> unchanged; those flags apply only to a fresh config). Because `deinit` leaves `config.json` behind,
> re-running `init` after a `deinit` is a fresh `created`, not a repair. An **explicit `--docs <paths>`
> is honored even on a complete layout** — it imports those paths into the existing store instead of
> short-circuiting (a bare re-`init` with no `--docs` still no-ops).

> `cm init` narrates its work as numbered steps on **stderr** (`[1/4] Создаю папку памяти…`,
> `[4/4] Индексирую исходный код… 302 символа в 57 файлах [C# 57]`) and ends with a one-glance
> summary; the machine-readable JSONL stays on **stdout**.
>
> **Folder-of-docs shortcut:** if the target directory (or any subfolder, recursively) contains
> `.md` files, `init` lists the folders they live in and asks **one** y/N before importing the lot,
> then asks whether to delete the **originals** — and only the imported `.md` (their copies stay in
> `imports/`); **source code is never touched**. The project's **root entry-point files** —
> `README.md` plus the agent-instruction docs (`CLAUDE.md`/`AGENTS.md`/…) — are **not** imported;
> they get a pointer wired in instead (see below). Put `cm` next to a pile of docs, run `cm init`,
> and the doc tree is absorbed. The freshly-created memory folder itself is skipped, so its
> `imports/` copies aren't re-ingested.
>
> **Pick the docs yourself:** pass `--docs <p1,p2,...>` (comma-separated folders and/or files) to
> import exactly those, skipping the auto-scan and the prompt. Folders are walked recursively, and
> a folder you name explicitly keeps its `README.md` (you asked for it). E.g.
> `cm init --docs docs,notes/spec.md`.

> **Auto-wiring agent instructions:** `init` also looks for an agent's instruction files in the
> target — `CLAUDE.md`, `AGENTS.md`, `AGENT.md`, `GEMINI.md`, `.cursorrules`,
> `.github/copilot-instructions.md` — and **appends** a small pointer block to each one it finds,
> telling the model to reach for project docs via `cm recall` rather than reading them whole. The
> block is bracketed by `<!-- BEGIN cm memory pointer -->` markers, so re-running is safe: an
> identical block is left untouched, while a stale one (e.g. you re-`init` under a different
> `--name`, so the path to the binary changed) is **refreshed in place** — never duplicated.
> **If none of those files exist, `init` creates an `AGENTS.md`** (a short `# AGENTS` header plus
> the pointer block) so a fresh project still gives a model an entry point. An existing file is only
> edited, never overwritten; the snapshot manifest records an `AGENTS.md` that `init` itself
> created, so `deinit` removes exactly that one — and `deinit` strips the block back out of files it
> only edited (round-tripping a fresh project back to nothing).
>
> The wired blocks are kept short on purpose; they show the JSON each call returns (so a model
> acts on the first try, no `cm help` round-trip) and link to **`CM_GUIDE.md`**, a standalone full
> manual `init` drops at the project root. Written agent-agnostically (Claude Code, Codex, Cursor,
> Copilot, Kimi, …) with worked `command → output` examples, it's a plain file any model can read.
> An existing `CM_GUIDE.md` is left untouched; one `init` created is recorded in the manifest and
> removed by `deinit`. The binary is spelled relative-and-runnable (`.\cm.exe`) throughout.

---

## Two things to keep in mind

- **The note body always comes from `stdin`, never an argument.** This sidesteps every shell
  quoting and newline headache. `echo "…" | cm remember`, not `cm remember "…"`.
- **Data output is JSONL on `stdout`**; narration, warnings, hints, and errors go to `stderr`.
  That makes the data trivial to pipe and parse. (A few human-facing commands — `config`,
  `export` without `--out`, `help` — print plain text instead.)

A note's **id is a short hex string** (e.g. `0a1b2c3d`) — and it's also the filename,
`notes/0a1b2c3d.md`.

---

## Command reference

| Command | What it does |
|---|---|
| `cm init [<path>] [--name N] [--docs P1,P2,…] [--provider local\|api] [--model M] [--dimension D] [--endpoint U] [--no-code]` | Scaffold the split layout: `cm` + `config.json` at the **root**, data (`notes/` + `imports/` + `store.db` + `models/`) in a `memory/` subfolder (config's `data_dir` links them). **No args** = do this in the current directory, gather docs (auto-scan + one y/N; root `README`/`CLAUDE`/… are wired, not imported), and map the source tree. `--docs` imports exactly the listed folders/files (no prompt). Won't touch an existing data folder. Path/name override the target (default data folder `memory`). Code map on by default (feature `code`; warns and still scaffolds without it); `--no-code` skips it. Also drops a standalone `CM_GUIDE.md` (full manual the wired pointers link to). Writes a snapshot manifest `memory/.init-manifest.json` (the root `.gitignore` it changed, an `AGENTS.md`/`CM_GUIDE.md` it created, each imported doc's original path) so `deinit` is an **exact rollback**. |
| `cm deinit <path> [--name N] [--yes]` | **Full rollback of `init`** — leaves only `cm` + `config.json`. Strips the pointer blocks from `CLAUDE.md`/`AGENTS.md`/… (and removes an `AGENTS.md`/`CM_GUIDE.md` init created); **restores every imported doc** to its original path if free, else under `<dir>/climem/<file>` (docs added later via `cm import` go to `docs/climem/`); restores the root `.gitignore` to its pre-init bytes (or deletes it if init created it); removes the data folder (`memory/`) **entirely**. Manifest-driven; with no manifest it falls back to the `imports/` sidecars (restoring by name into `docs/climem/`) and strips only its own `.gitignore` block. Finds the data folder via config's `data_dir` (or `--name`). Asks first; `--yes` skips. |
| `… \| cm remember [--tags a,b] [--source S] [--slug S] [--relations "p:t,p:t"] [--code-refs "sym,sym"]` | Write a note. Body from **stdin** → `notes/<id>.md`, then index. `--slug`/`--relations` feed the notes graph; `--code-refs` anchors the note to code symbols (resolved live at `recall`). Prints `{"id":"<hex>"}`. |
| `… \| cm feedback [--tags a,b]` · `cm feedback --list [--recent N]` | Let the agent that *uses* cm report what's missing or could be better. Body from **stdin** (like `remember`) → an ordinary note tagged `cm-feedback`, stamped with the cm version, saved into **this project's** memory; prints `{"id":"<hex>"}`. `--list` prints the feedback gathered so far (newest first). Review anytime with `cm feedback --list` or `cm recall "<topic>" --tag cm-feedback`. |
| `cm recall "<query>" [--limit N] [--budget C \| --full] [--explain] [--fields …] [--tag T] [--origin-prefix F] [--min-score X] [--related <id>]` | Hybrid search (FTS5 + vector, plus optional graph proximity via `--related <id>`, fused via RRF). Returns lean JSONL sorted by relevance, **preview-first**: a long body comes back as `preview`+`chars` (cap `search.recall_body_chars`, default 500) unless `--full`/`--budget 0`. `--related` is off unless `search.hybrid_weights.graph` is non-zero. |
| `cm get <id>` | Fetch one record in full. |
| `cm list [--recent N]` | List the most recent records (body shown as a preview). |
| `cm related <id> [--depth D] [--predicate P] [--limit N] [--fields …]` | Graph neighbours this note points at (frontmatter `relations` + `[[wikilinks]]`). Dangling targets come back as `{"dangling":true,…}`. |
| `cm backlinks <id> [--predicate P] [--limit N]` · `cm backlinks --symbol <name> [--limit N]` | The reverse of `related`: notes that point **at** `<id>` → `{"id","kind","predicate","preview"}`. With `--symbol`, the doc↔code mirror: notes that **document** a code symbol → `{"id","kind","documents","preview"}`. |
| `cm forget <id>` | Delete `notes/<id>.md` and its index row → `{"deleted":bool,"id":"<hex>"}`. (Imported chunks can't be deleted this way — edit `imports/` and `reindex`.) |
| `cm import <file> [--tags a,b]` | Copy the original into `imports/`, split it into chunks, index them → `{"imported":"…","chunks":N}`. |
| `cm reindex [--all]` | Rebuild `store.db` from `notes/` + `imports/`, incrementally (by content hash). `--all` forces a full re-embed → `{"indexed":N,"changed":M}`. |
| `cm doctor [--fix] [--text]` | Health-check the layout + derived `store.db` against the md truth, print stats, and for every problem print the exact fix. Also verifies the **agent wiring** — that the instruction files (`CLAUDE.md`/`AGENTS.md`/…) point at a cm binary that actually **exists** at the project root — and flags **encoding-corrupted** notes (non-ASCII text lost to `?` by a non-UTF-8 shell; report-only, the loss is irreversible). Read-only by default; `--fix` applies only safe, delegated repairs — rebuild a **deleted `store.db`** via `reindex`, create missing dirs, or repoint `data_dir` at a **renamed** data folder (wiring is left to `cm init`). Default output is one JSON line per check `{"check","status","fixable","fix"}`, then `{"stat",…}` lines and a `{"doctor","errors","warnings","info","fixed"}` summary (always exits 0); `--text` prints a readable report instead. |
| `cm map <path> [--lang L] [--exclude S]` | **(feature `code`)** Build a *separate* dependency graph of your **source code** — symbols, where each is defined, which uses which. Incremental by content hash; source files aren't copied → `{"mapped","scanned","changed","files","symbols","edges"}`. |
| `cm map --query <name> \| --like <substr> \| --list [--all] [--limit N] \| --uses <name> \| --calls <name> \| --defines <file>` | Query the code graph (read-only JSONL): where a symbol is **defined** (`--query`), symbols whose name **contains** a substring (`--like`), the **hub symbols** ranked by graph `degree` — top 30, the architecture skeleton (`--list`; `--all` lists every symbol, `--limit N` resizes the cap), which symbols **use** a symbol (`--uses`), what a symbol **depends on** (`--calls`, in-project only unless `--external`), or what a **file defines** (`--defines`). Symbol listings accept `--kind function\|method\|class\|…`. |
| `cm export <md\|json\|jsonl> [--out F] [--query Q] [--limit N] [--tag T] [--origin-prefix P] [--min-score N]` | Export memory (all, or filtered by `--query`; same pre-filters as `recall`, `--limit` defaults to 50). Without `--out`, prints to stdout. |
| `cm log [--imports] [--recent N]` | Operation history, or the list of import sources. |
| `cm config [get <key> \| set <key> <value>]` | Show or edit `config.json` (secrets are masked). |
| `cm help` | The full contract — it lives inside the binary, so it never drifts from the build. |

Both call styles work: subcommands (`cm recall …`) and a Windows-flavoured leading slash
(`cm /recall …`).

---

## Lean recall output

By default `recall` prints a **lean** line — just what a consumer usually needs: `id`, `kind`,
the body, plus `tags`/`origin`/`source` when they're set (empty/null fields are dropped). The
debug relevance numbers are hidden, which saves tokens without hurting the results.

**Preview-first body.** A short note prints its whole `body`; a long one (an imported doc chunk,
say) comes back as `preview` (the first 500 chars) plus `chars` (its full length) *instead of*
`body` — so the everyday read path stays small and you only pay for the long tail when you ask
for it. The cap is `search.recall_body_chars` (default 500); `--budget C` overrides it per call,
and `--full` (or `--budget 0`) prints whole bodies. When you see `preview`+`chars`, the full
text is one `cm get <id>` away.

```bash
cm recall "auth" --limit 5             # lean, preview-first default (N defaults to 5)
cm recall "auth" --budget 300          # cap each body at 300 chars
cm recall "auth" --full                # whole bodies, no preview
cm recall "auth" --explain             # + score/fts/vector/graph (each channel's RRF contribution)
cm recall "auth" --fields id,body      # exactly these fields (bypasses the budget)
cm recall "schema" --tag spec          # only notes carrying a tag
cm recall "schema" --origin-prefix arch.md   # only chunks from a given file
cm recall "schema" --min-score 0.01    # drop weak candidates
```

`--fields` understands: `id,kind,body,tags,origin,source,score,fts,vector,graph,created_at,preview,chars`.
The full record is always one `cm get <id>` away.

---

## The knowledge graph

Notes link to each other, and the links are **directed**: `A → B` means "A points at B".
`cm related <id>` walks them forward (what does this note point at?); `cm backlinks <id>`
walks them backward (what points at this note?).

- In a note's **frontmatter**: an optional `slug` (a human-friendly name others link to) and
  `relations` (a list of `predicate: target` edges).
- In the **body**: `[[wikilinks]]` (they get the synthetic predicate `links_to`).

A link target is resolved by `slug`; prefix it with `id:` to address by id instead
(`id:0a1b2c3d`). Predicates are normalized (`depends_on`, `depends-on`, `Depends On` all
collapse to `depends_on`), so `--predicate` matching is forgiving. An unresolved target is kept
as a first-class **dangling** edge — it comes to life automatically on the next `reindex`, once
the target note exists, *even if the linking note itself never changed*. Symmetrically,
deleting a note turns links that pointed at it back into dangling ones (the link is still
authored in the other note's Markdown). The graph itself is derived: it's stored in `store.db`,
computed from the Markdown, and rebuilt by `reindex`.

**Graph-aware recall.** `cm recall "<q>" --related <id>` adds a third channel that favours notes
near `<id>` in the graph. It's **off by default** — turn it on once with
`cm config set search.hybrid_weights.graph 0.3`. Its reach (`search.graph_depth`, default 2
hops) and an optional predicate filter (`search.graph_predicate`) live in config.

```markdown
---
id: 0a1b2c3d
created: 2026-06-17T10:00:00Z
tags: auth, decision
slug: jwt-auth
relations:
  - depends_on: db-schema
---
Decided: authentication is JWT-based. Details live in [[db-schema]].
```

---

## The code graph (a separate map of your source)

The knowledge graph above is about your **notes**. There's a second, **completely separate**
graph — of your **source code** — behind the optional `code` feature. It never mixes into
`recall`/`related`; it lives in its own tables and is reached only through `cm map`. The point
is to answer *structural* questions about a codebase precisely, in one query, instead of a pile
of greps: **where is symbol X defined**, **who uses X**, **what does this file define**.

```bash
cm map ./src                 # index the tree (incremental; self-refreshes on a stale query)
cm map --query upsert_note   # -> {"name","kind","path","line","signature"}   (where defined)
cm map --like config         # -> every symbol whose name contains "config"
cm map --list                # -> top 30 hub symbols (most-connected first) + degree
cm map --list --all --kind class  # -> every type (full list, no ranking)
cm map --uses Store          # -> who uses it, and from which symbol/line
cm map --calls reindex_notes # -> what it depends on (in-project; add --external for stdlib)
cm map --defines src/store.rs# -> every symbol that file defines
```

It parses with [tree-sitter](https://tree-sitter.github.io/), using each grammar's own
`tags.scm` query (the same "tagging" mechanism that powers go-to-definition), so one engine
covers every language: **Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, C#, Ruby, PHP**
(picked by file extension). Test code (in `tests/` files or inline test modules) is **hidden by
default** in listings so you see a module's real API — pass `--tests` to include it. And because
`--uses`/`--calls` resolve by name, ubiquitous stdlib methods (`map`, `unwrap`, `get`, …) are
deliberately **not** linked to same-named project symbols, so they don't masquerade as deps. Nodes are files and symbols; edges are `defines` (file → symbol) and
`uses` (symbol → symbol, resolved by name — an unresolved reference is kept **dangling** and
revives on a later `map`, exactly like the notes graph). Build/vendor/generated dirs (`target`,
`node_modules`, `.git`, .NET `obj/` and `bin/`, …) are skipped automatically. Your source files
are **not** copied anywhere — the graph is a rebuildable cache over your working tree.

**Self-refreshing.** `cm map <path>` remembers the tree it indexed. If a later query
(`--query`/`--uses`/`--defines`/…) finds **nothing**, `cm` silently re-maps that same tree —
which may have gone stale after edits — and retries once before answering, printing a short note
on stderr when it does. So an empty result is trustworthy ("the symbol really isn't there"), and
you don't have to re-map by hand after editing. (A genuinely-absent symbol re-maps once, then
returns empty — no retry loop.)

Needs a binary built with the feature (`cargo build --release --features code`); without it,
`cm map` tells you exactly that.

**One-step setup.** Drop the binary at a project root and run a bare `cm init` — it scaffolds
the memory folder, gathers docs, *and* maps the source tree in one go (the counts come back as
`{"code":{"files","symbols","edges"}}` in init's output). The code map is on by default; pass
`--no-code` for a docs-only setup. Without the feature, init warns and still scaffolds normally.

### Doc ↔ code anchors (a note that knows whether its code still exists)

A note can anchor itself to the code it documents. Author the symbol names with
`cm remember --code-refs "validate_token, JwtAuth"`, or inline in the body as
`[[code:validate_token]]` (either way they're stored as the note's `code:` field). They're not
searched as text — they're **anchors by name** into the code graph, resolved **live at `recall`**:

```bash
echo "Auth issues a JWT; refresh lasts 30 days." | cm remember --code-refs "validate_token"
cm recall "how does auth work"
# → {"id":"…","kind":"note","body":"Auth issues a JWT…",
#    "code_refs":[{"name":"validate_token","resolved":true,"path":"src/auth.rs","line":42,
#                  "signature":"fn validate_token("}]}
```

When the symbol still exists you get its **current** `path`/`line`/`signature`; when it's gone the
anchor comes back `"resolved":false` — a built-in **staleness signal** telling you the doc now
describes code that no longer exists. Anchored by name (not `path:line`), so ordinary edits that
shift lines don't break it. The notes graph and the code graph stay separate stores — `recall` just
federates the lookup; nothing is mixed or duplicated. (Anchors resolve against whatever `cm map`
last indexed; in a store with no code graph they read `resolved:false` with a one-line hint to run
`cm map`.)

The mirror direction — *from a symbol, find the docs that explain it* — is `cm backlinks --symbol
validate_token`, which lists the notes that document that symbol. So the link reads both ways: a
note knows the code it covers, and a symbol knows the notes that cover it.

---

## Using a neural embedding provider

```bash
# OpenAI-compatible
cm config set embedding.provider api
cm config set embedding.endpoint https://api.openai.com/v1/embeddings
cm config set embedding.model text-embedding-3-small
cm config set embedding.dimension 1536
export MEMORY_EMBED_API_KEY=sk-...        # the var name is stored in embedding.api_key_env

# Ollama (local, offline neural model)
cm config set embedding.provider api
cm config set embedding.api_format ollama
cm config set embedding.endpoint http://localhost:11434/api/embeddings
cm config set embedding.model nomic-embed-text
cm config set embedding.dimension 768
```

> ⚠️ Switching provider/model on a non-empty store can leave old vectors with a mismatched
> dimension — the tool will warn you. Recreate the store or re-import after the switch.

---

## Where everything lives

`cm init` lays out two places. The binary and `config.json` stay at the **project root**
(where you dropped `cm`); the data lives in a **`memory/` subfolder**:

```text
project-root/
├── cm(.exe)        # the binary, where you put it
├── config.json     # provider, search weights, chunking… + data_dir → the folder below
└── memory/         # the data folder (name from config's data_dir; default "memory")
    ├── notes/      # ← source of truth: one <id>.md per note          (commit this)
    ├── imports/    # ← source of truth: original imported docs + meta (commit this)
    ├── store.db    # derived index (FTS5 + vectors + graph)          (gitignore-able)
    └── models/     # embedding weights                               (re-downloadable)
```

`config.json` records `data_dir` (default `"memory"`), so the binary at the root finds its
data without a flag. **Commit `config.json` + the data folder's `notes/` and `imports/`.**
`store.db` is derived — ignore it and rebuild with `cm reindex`. To point `cm` at a different
location, use `--dir <path>` (the folder holding `config.json`) or set `MEMORY_DIR`.

> **Backward compatible:** a `config.json` without `data_dir` keeps the old single-folder layout
> (everything in one folder). An absolute `data_dir` is honored as-is.

---

## Source layout

```text
src/
├── main.rs       — entry point, dispatcher, memory-folder resolution
├── cli.rs        — argument parsing (subcommands + /flags)
├── commands.rs   — one function per command (incl. reindex)
├── help.rs       — the contract (help text) + the pointer
├── config.rs     — config.json (typed + raw get/set, secret masking)
├── note.rs       — the .md note format: hand-rolled frontmatter parse/render
├── store.rs      — SQLite (the derived index): notes, FTS5, vectors, graph edges, sync
├── embed/        — embedding providers behind one trait
│   ├── mod.rs    — trait Embedder, provider selection, cosine, encode/decode
│   ├── hashing.rs— the offline embedder (word + char-3gram hashing) — the default
│   └── api.rs    — HTTP neural embedder (feature `api`)
├── search.rs     — hybrid recall (fuse FTS ↔ vector via RRF)
├── graph.rs      — notes graph from Markdown: slug normalization, [[wikilinks]], target resolution
├── code.rs       — source-code graph (feature `code`): tree-sitter tags → defines/uses, 11 languages
├── chunk.rs      — structure-aware splitting with overlap
├── import.rs     — copy original into imports/ + sidecar, chunk it (+pdf behind a feature)
├── export.rs     — md / json / jsonl (+pdf behind a feature)
├── output.rs     — JSON shaping for output (recall/related get lean projections)
├── init.rs       — scaffold the memory folder, self-copy the binary, wire CLAUDE.md, print the pointer
└── deinit.rs     — full rollback of init: restore docs, unwire, remove the data folder (manifest-driven)
```

---

## Design principles

A few load-bearing choices, in case you extend it:

- **One small binary, no runtime.** Each call opens the store, does one thing, and exits.
- **Markdown is canonical; the database is a rebuildable cache.** A wiped `store.db` is a
  non-event.
- **The body always comes from stdin** — no shell-quoting bugs, ever.
- **Data is JSONL on stdout; everything else is stderr.**
- **`help` is the contract**, baked into the binary so it can't go stale.
- **Errors are self-healing** — every input error prints a short hint with a correct example,
  so the caller fixes itself in one step.
- **A lean dependency tree** — heavy features (`api`, `pdf`) are opt-in behind cargo features
  and fail with a clear "rebuild with `--features …`" message when absent.

---

## How climem compares

"Markdown memory for AI agents" is a real and crowded space now. climem isn't the only
take on it — here's where it sits relative to the closest tools, so you can pick honestly.

| | climem | [Engram](https://github.com/Gentleman-Programming/engram) | [basic-memory](https://github.com/basicmachines-co/basic-memory) | [memweave](https://github.com/sachinsharma9780/memweave) |
|---|---|---|---|---|
| **Form factor** | single Rust binary | single Go binary | Python app | Python library |
| **Transport** | **CLI only** | CLI + HTTP + MCP + TUI | MCP-first (+CLI/HTTP) | embedded in your code |
| **Search** | **FTS5 + vector (hybrid)** | FTS5 only | FTS5 + vector (hybrid) | BM25 + vector (hybrid) |
| **Offline embeddings** | **built-in (char-3gram hash)** | n/a (no vectors) | yes (FastEmbed) | needs an embedder |
| **Source of truth** | **Markdown files** | the database | Markdown files | Markdown files |
| **DB is disposable** | **yes (`reindex` rebuilds)** | no (DB is canonical) | index is derived | index is derived |
| **Knowledge graph** | yes (`relations` + `[[links]]`) | no | yes (typed relations) | no |
| **Doc import / chunking** | yes (md/txt/html/pdf) | JSON import only | — | — |

What makes climem its own point in this space:

- **CLI-only, on purpose.** No MCP, no daemon, no HTTP server. Every command is a fresh
  short-lived process the agent calls like any other shell tool. This sidesteps MCP's
  cross-client instability and needs nothing more than a shell — see the rationale in
  [desc.md §11](desc.md).
- **Hybrid search with a zero-download offline embedder.** Engram is keyword-only;
  basic-memory and memweave have vectors but pull in a neural embedder. climem ships a
  deterministic char-3gram embedder that needs no model and no network, and its character
  grams handle **inflected languages** (e.g. Russian) that bare FTS5 stemming misses.
- **Markdown is the truth, the DB is a cache.** Same stance as basic-memory/memweave, the
  opposite of Engram (where the SQLite DB is canonical). Delete `store.db`, run `reindex`,
  nothing is lost.
- **Smallest footprint.** One static binary, no Python/Node runtime, no server to keep
  alive — the whole memory folder is self-contained and git-friendly.

Where the others are ahead: basic-memory has native **Obsidian** sync and a more mature
typed-graph model; Engram offers more **transports** out of the box (HTTP/MCP/TUI);
memweave adds **temporal decay** and **MMR re-ranking** for result diversity. If you want
those today, they're the better pick — climem trades them for being the minimal, runtime-free
CLI option.

---

## License

MIT.
