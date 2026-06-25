# Using `.\cm.exe` — project memory & code map

`.\cm.exe` is this project's memory tool: a small command-line program that stores
notes and documents on disk and searches them, and also holds a structural map of
the source code. The project's real documentation lives inside it — the
instruction files (CLAUDE.md / AGENTS.md / …) only point here.

**You (the coding agent) are the intended user.** This guide is everything you
need; you do not have to run `.\cm.exe help` to get started.

## The two rules that matter most

1. **Every command prints JSONL** — one JSON object per line on stdout. Parse it;
do not guess. Errors and hints go to stderr with one correct example to copy.
2. **Text you save comes from STDIN, never an argument.** Pipe it in:
`echo "a fact" | .\cm.exe remember`  ✅   ·   `.\cm.exe remember "a fact"`  ❌ (ignored).

## Memory: the everyday read/write loop

**Recall before you answer.** Search notes and imported docs by topic:
```
.\cm.exe recall "how does authentication work" --limit 5
→ {"id":"0a1b2c3d","kind":"note","body":"Auth uses JWT; refresh lasts 30 days."}
```
Each line is one match, best first. Useful flags: `--limit N` (how many),
`--tag T` (only that tag), `--fields id,body` (only these fields).

**Remember after you decide.** Save a fact for next time (body on stdin):
```
echo "Decided: store config as JSON next to the binary." | .\cm.exe remember --tags decision
→ {"id":"7f3e9a01"}
```
The id names the note's file (`notes/7f3e9a01.md`). Get one back in full with
`.\cm.exe get 7f3e9a01`; list the newest with `.\cm.exe list`.

## Code map: structural questions about the source

A SEPARATE graph of the code (it never mixes into recall): which symbols exist,
where each is defined, and which uses which. Indexed automatically at `.\cm.exe init`;
after you edit code, refresh it (incremental): `.\cm.exe map <path>`.

Prefer it over grep for "where / who / what" questions about a symbol — it knows
symbol boundaries, so it never matches a name inside a string, a comment, or a
longer word, and it tells a definition apart from a use. Each mode prints one JSON
object per line:

| Question | Command | Output fields |
|---|---|---|
| Where is X defined? | `.\cm.exe map --query X` | `name, kind, path, line, signature` |
| Any symbol like …? | `.\cm.exe map --like part` | `name, kind, path, line, signature` |
| What does file F define? | `.\cm.exe map --defines F` | `name, kind, path, line, signature` |
| Who calls / uses X? | `.\cm.exe map --uses X` | `name, kind, path, line, def_line` |
| What does X depend on? | `.\cm.exe map --calls X` | `calls, line, resolved` |

Worked examples with real output:
```
.\cm.exe map --query upsert_note
→ {"name":"upsert_note","kind":"method","path":"store.rs","line":268,"signature":"pub fn upsert_note("}

.\cm.exe map --uses recall_with        # who calls it, attributed to the caller fn
→ {"name":"run","kind":"function","path":"commands.rs","line":142,"def_line":120}

.\cm.exe map --calls recall_with       # its in-project dependencies
→ {"calls":"fts_search","line":196,"resolved":true}
```
For symbol listings, `--kind function|method|class|…` narrows by kind, and
`--tests` opts test code back in (hidden by default).

**When to use grep instead:** free text (a string, an error message, a TODO, a
config key); very common/overloaded names (`new`, `get`, `run` — the map merges
same-named symbols and may misattribute); or code you just edited but haven't
re-mapped. Rule of thumb: *unique-ish name + structural question → `.\cm.exe map`;
common name, free text, or just-edited code → grep.*

## Feedback: help improve `.\cm.exe`

You are the agent who actually USES this tool, so you see its rough edges before
anyone else. When `.\cm.exe` is missing a command or flag, returns output that's
awkward to act on, or behaves surprisingly, record it — the text comes from STDIN,
exactly like `remember`:
```
echo "cm map --uses misses trait impls; a --kind filter would help" | .\cm.exe feedback
→ {"id":"7f3e9a01"}
```
It is saved into THIS project's memory as a note tagged `cm-feedback` (stamped
with the cm version), so it never gets lost. Add `--tags <area>` to mark what it's
about (e.g. `--tags map`). Whoever maintains cm reviews it later with
`.\cm.exe feedback --list` (or `.\cm.exe recall "<topic>" --tag cm-feedback`). Short,
concrete notes about what would have helped are exactly what makes the tool
better — don't hold back.

## Where things live

At the project root: `.\cm.exe` (the binary) and `config.json` (settings). The data —
`notes/` and `imports/` (the real memory) plus `store.db` (a rebuildable search
index) — sits in the data folder named by `config.json`. You don't manage these by
hand; use the commands above.

## Full command reference

This file covers the everyday loops. For every command, flag, and output field —
including `import`, `export`, `related`/`backlinks` (the note graph), `reindex`,
`config`, and `deinit` — run `.\cm.exe help`. It prints the complete contract that
ships inside the binary, always in sync with this version.
