---
id: eefaa916
created: 2026-06-18T17:28:53Z
tags: init,docs,decision
slug: init-docs-selection
---
`cm init` — выбор доков для импорта (2026-06-18). Авто-скан (без флага) пропускает КОРНЕВЫЕ entry-point файлы (README.md + ENTRY_POINT_NAMES: CLAUDE/AGENTS/AGENT/GEMINI/.cursorrules/copilot-instructions) — они привязываются pointer-блоком, не импортируются; skip только на верхнем уровне target (walk_md получил `root`, скип при dir==root, глубже README берётся). Затем показывает список ПАПОК с числом .md и ОДИН y/N. Флаг `--docs <p1,p2,...>` (новый VALUE_FLAG) — явные папки/файлы, импорт ровно их без скана и без y/N (walk_md_all — без skip корневых, README названной папки берётся). TUI-крейты (dialoguer/inquire) ОТВЕРГНУТЫ: десятки транзитивных деп + требуют TTY (ломаются в пайпе) — нарушают инвариант «минимум зависимостей»; ввод остаётся построчным prompt_yes_no. Связано: [[init-no-arg-defaults]], [[init-wire-entry-points]].
