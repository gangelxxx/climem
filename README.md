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
```

The result is one self-contained executable. Drop it wherever you like.

---

## Quickstart

```bash
# 1. Scaffold a memory folder (copies the binary + notes/ + imports/ + store.db + config.json)
cm init ./ --name project-memory

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

> **Folder-of-docs shortcut:** if the target directory (or any subfolder, recursively) contains
> `.md` files, `init` offers (y/N) to import them all at once, then offers to delete the originals
> (copies stay in `imports/`). Put `cm` next to a pile of docs, run `cm init ./`, and the whole
> tree is absorbed. The freshly-created memory folder itself is skipped, so its `imports/` copies
> aren't re-ingested.

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
| `cm init <path> [--name N] [--provider local\|api] [--model M] [--dimension D] [--endpoint U]` | Scaffold a self-contained memory folder (`notes/` + `imports/` + `store.db` + `config.json` + a copy of the binary). Won't touch an existing folder. |
| `… \| cm remember [--tags a,b] [--source S] [--slug S] [--relations "p:t,p:t"]` | Write a note. Body from **stdin** → `notes/<id>.md`, then index. `--slug`/`--relations` feed the graph. Prints `{"id":"<hex>"}`. |
| `cm recall "<query>" [--limit N] [--explain] [--fields …] [--tag T] [--origin-prefix F] [--min-score X] [--related <id>]` | Hybrid search (FTS5 + vector, fused via RRF). Returns lean JSONL sorted by relevance. |
| `cm get <id>` | Fetch one record in full. |
| `cm list [--recent N]` | List the most recent records (body shown as a preview). |
| `cm related <id> [--depth D] [--predicate P] [--limit N] [--fields …]` | Graph neighbours (frontmatter `relations` + `[[wikilinks]]`). Dangling targets come back as `{"dangling":true,…}`. |
| `cm forget <id>` | Delete `notes/<id>.md` and its index row → `{"deleted":bool,"id":"<hex>"}`. (Imported chunks can't be deleted this way — edit `imports/` and `reindex`.) |
| `cm import <file> [--tags a,b]` | Copy the original into `imports/`, split it into chunks, index them → `{"imported":"…","chunks":N}`. |
| `cm reindex [--all]` | Rebuild `store.db` from `notes/` + `imports/`, incrementally (by content hash). `--all` forces a full re-embed → `{"indexed":N,"changed":M}`. |
| `cm export <md\|json\|jsonl> [--out F] [--query Q]` | Export memory (all, or filtered by `--query`). Without `--out`, prints to stdout. |
| `cm log [--imports] [--recent N]` | Operation history, or the list of import sources. |
| `cm config [get <key> \| set <key> <value>]` | Show or edit `config.json` (secrets are masked). |
| `cm help` | The full contract — it lives inside the binary, so it never drifts from the build. |

Both call styles work: subcommands (`cm recall …`) and a Windows-flavoured leading slash
(`cm /recall …`).

---

## Lean recall output

By default `recall` prints a **lean** line — just what a consumer usually needs: `id`, `kind`,
`body`, plus `tags`/`origin`/`source` when they're set (empty/null fields are dropped). The
debug relevance numbers are hidden, which saves tokens without hurting the results.

```bash
cm recall "auth" --limit 5             # lean default (N defaults to 5)
cm recall "auth" --explain             # + score/fts/vector/graph (each channel's RRF contribution)
cm recall "auth" --fields id,body      # exactly these fields
cm recall "schema" --tag spec          # only notes carrying a tag
cm recall "schema" --origin-prefix arch.md   # only chunks from a given file
cm recall "schema" --min-score 0.01    # drop weak candidates
```

`--fields` understands: `id,kind,body,tags,origin,source,score,fts,vector,graph,created_at,preview`.
The full record is always one `cm get <id>` away.

---

## The knowledge graph

Notes can link to each other, and `cm related` walks those links.

- In a note's **frontmatter**: an optional `slug` (a human-friendly name others link to) and
  `relations` (a list of `predicate: target` edges).
- In the **body**: `[[wikilinks]]`.

A link target is resolved by `slug`; prefix it with `id:` to address by id instead
(`id:0a1b2c3d`). An unresolved target is kept as a first-class **dangling** edge — it comes to
life automatically on the next `reindex`, once the target note exists. The graph itself is
derived: it's stored in `store.db`, computed from the Markdown, and rebuilt by `reindex`.

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

A memory folder sits next to the binary and contains:

```text
project-memory/
├── cm(.exe)        # a copy of the binary, so the folder is self-contained
├── notes/          # ← source of truth: one <id>.md per note          (commit this)
├── imports/        # ← source of truth: original imported docs + meta (commit this)
├── store.db        # derived index (FTS5 + vectors + graph)          (gitignore-able)
└── config.json     # provider, search weights, chunking…
```

**Commit `notes/` and `imports/`.** `store.db` is derived — ignore it and rebuild with
`cm reindex`. To point `cm` at a different folder, use `--dir <path>` or set the `MEMORY_DIR`
environment variable.

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
├── graph.rs      — graph from Markdown: slug normalization, [[wikilinks]], target resolution
├── chunk.rs      — structure-aware splitting with overlap
├── import.rs     — copy original into imports/ + sidecar, chunk it (+pdf behind a feature)
├── export.rs     — md / json / jsonl (+pdf behind a feature)
├── output.rs     — JSON shaping for output (recall/related get lean projections)
└── init.rs       — scaffold the memory folder, self-copy the binary, print the pointer
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

## License

MIT.
