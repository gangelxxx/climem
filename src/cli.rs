//! A tiny argument parser we wrote ourselves, so we don't drag in clap & friends.
//! It understands both `cm recall ...` and the Windows `cm /recall ...` style.

use std::collections::{HashMap, HashSet};

/// Flags that expect a value (`--flag value` or `--flag=value`). Anything else
/// starting with `--` is just a boolean flag. Add new value-flags here, or
/// `--flag value` parses as a bool flag plus a stray positional.
const VALUE_FLAGS: &[&str] = &[
    "tags",
    "limit",
    "name",
    "out",
    "query",
    "recent",
    "model",
    "provider",
    "endpoint",
    "dimension",
    "dir",
    "source",
    "format",
    // recall projection / pre-filter / adaptive-k (token-efficiency-plan E1/R4/K1)
    "fields",
    "tag",
    "origin-prefix",
    "min-score",
    // recall preview-first body budget: cap each hit's body at N chars
    // (`--full` is boolean and stays out of VALUE_FLAGS).
    "budget",
    // graph traversal (`related`): --depth N, --predicate P (--all is boolean)
    "depth",
    "predicate",
    // graph authoring on `remember`: --slug S, --relations "p:t,p:t",
    // --code-refs "sym,sym" (anchors into the code graph, resolved at recall)
    "slug",
    "relations",
    "code-refs",
    // graph re-rank channel for `recall`: --related <id>
    "related",
    // code graph (`map`): index with --lang/--exclude; query with
    // --query <name> | --uses <name> | --defines <path> | --like <substr> |
    // --calls <name> | --kind <k> (--list and --external are boolean)
    "lang",
    "exclude",
    "uses",
    "defines",
    "like",
    "calls",
    "kind",
    // init: explicit doc paths to import (comma-separated dirs and/or files),
    // bypassing the auto-scan + y/N prompt.
    "docs",
    // NOTE: `doctor` adds no value-flag — its flags `--fix` and `--text` are both
    // boolean (parsed via `has(...)`), so they intentionally need no entry here.
];

#[derive(Debug, Default)]
pub struct Parsed {
    /// Positional args; `positionals[0]` is the command.
    pub positionals: Vec<String>,
    pub values: HashMap<String, String>,
    pub bools: HashSet<String>,
}

impl Parsed {
    pub fn parse(args: &[String]) -> Parsed {
        let mut p = Parsed::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(rest) = arg.strip_prefix("--") {
                Self::handle_flag(&mut p, rest, args, &mut i);
            } else if arg == "-h" {
                p.bools.insert("help".into());
            } else if let Some(rest) = arg.strip_prefix('/') {
                // Windows style: the first /token is the command, any later /x are flags.
                if p.positionals.is_empty() {
                    p.positionals.push(rest.to_string());
                } else {
                    Self::handle_flag(&mut p, rest, args, &mut i);
                }
            } else {
                p.positionals.push(arg.clone());
            }
            i += 1;
        }
        p
    }

    fn handle_flag(p: &mut Parsed, rest: &str, args: &[String], i: &mut usize) {
        if let Some((k, v)) = rest.split_once('=') {
            p.values.insert(k.to_string(), v.to_string());
            return;
        }
        if VALUE_FLAGS.contains(&rest) {
            if *i + 1 < args.len() {
                *i += 1;
                p.values.insert(rest.to_string(), args[*i].clone());
            } else {
                // Nothing after the flag: store an empty string so the command
                // can complain with a useful hint instead of silently doing nothing.
                p.values.insert(rest.to_string(), String::new());
            }
        } else {
            p.bools.insert(rest.to_string());
        }
    }

    pub fn command(&self) -> Option<&str> {
        self.positionals.first().map(|s| s.as_str())
    }

    /// Positional after the command (index 0 == first arg after command).
    pub fn arg(&self, idx: usize) -> Option<&str> {
        self.positionals.get(idx + 1).map(|s| s.as_str())
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    pub fn has(&self, key: &str) -> bool {
        self.bools.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse from string-slice literals (the real API takes `&[String]`).
    fn p(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parse_empty_args_yields_no_command() {
        let parsed = p(&[]);
        assert_eq!(parsed.command(), None);
        assert_eq!(parsed.arg(0), None);
        assert_eq!(parsed.value("limit"), None);
        assert!(!parsed.has("json"));
    }

    #[test]
    fn parse_subcommand_and_positional_offset() {
        let parsed = p(&["get", "42"]);
        assert_eq!(parsed.command(), Some("get"));
        assert_eq!(parsed.arg(0), Some("42"));
        assert_eq!(parsed.arg(1), None);
    }

    #[test]
    fn parse_value_flag_space_form() {
        let parsed = p(&["recall", "--limit", "5"]);
        assert_eq!(parsed.value("limit"), Some("5"));
        // "5" was consumed by --limit, not left as a positional.
        assert_eq!(parsed.arg(0), None);
    }

    #[test]
    fn parse_value_flag_equals_form() {
        let parsed = p(&["recall", "--limit=5"]);
        assert_eq!(parsed.value("limit"), Some("5"));
    }

    #[test]
    fn parse_equals_form_overrides_value_flags_check() {
        // `=` form stores a value even for a flag not in VALUE_FLAGS.
        let parsed = p(&["recall", "--verbose=1"]);
        assert_eq!(parsed.value("verbose"), Some("1"));
    }

    #[test]
    fn parse_value_flag_missing_value_records_empty() {
        let parsed = p(&["recall", "--limit"]);
        assert_eq!(parsed.value("limit"), Some(""));
    }

    #[test]
    fn parse_bool_flag_not_in_value_flags() {
        let parsed = p(&["list", "--json", "x"]);
        assert!(parsed.has("json"));
        // --json is boolean, so the next token stays positional.
        assert_eq!(parsed.arg(0), Some("x"));
    }

    #[test]
    fn parse_dash_h_sets_help_bool() {
        let parsed = p(&["-h"]);
        assert!(parsed.has("help"));
        assert_eq!(parsed.command(), None);
    }

    #[test]
    fn parse_windows_first_slash_is_command() {
        let parsed = p(&["/recall", "/limit", "5"]);
        assert_eq!(parsed.command(), Some("recall"));
        assert_eq!(parsed.value("limit"), Some("5"));
    }

    #[test]
    fn parse_double_dash_help_sets_help_bool_not_command() {
        let parsed = p(&["--help"]);
        assert!(parsed.has("help"));
        assert_eq!(parsed.command(), None);
    }

    #[test]
    fn parse_equals_empty_value() {
        let parsed = p(&["--query="]);
        assert_eq!(parsed.value("query"), Some(""));
    }

    #[test]
    fn parse_equals_multiple_signs_splits_on_first() {
        let parsed = p(&["--source=a=b=c"]);
        assert_eq!(parsed.value("source"), Some("a=b=c"));
    }

    #[test]
    fn parse_windows_bool_slash_flag() {
        let parsed = p(&["/list", "/json"]);
        assert_eq!(parsed.command(), Some("list"));
        assert!(parsed.has("json"));
    }

    #[test]
    fn parse_mixed_styles() {
        let parsed = p(&["recall", "/limit", "5", "--json"]);
        assert_eq!(parsed.command(), Some("recall"));
        assert_eq!(parsed.value("limit"), Some("5"));
        assert!(parsed.has("json"));
    }

    #[test]
    fn parse_value_flag_consumes_next_even_if_flaglike() {
        let parsed = p(&["recall", "--limit", "--tags", "x"]);
        assert_eq!(parsed.value("limit"), Some("--tags"));
        assert_eq!(parsed.value("tags"), None);
        assert_eq!(parsed.arg(0), Some("x"));
    }

    #[test]
    fn parse_non_value_flag_leaves_next_as_positional() {
        let parsed = p(&["remember", "--pinned", "body"]);
        assert!(parsed.has("pinned"));
        assert_eq!(parsed.arg(0), Some("body"));
    }

    #[test]
    fn parse_all_value_flags_recognized() {
        for flag in VALUE_FLAGS {
            let parsed = p(&["cmd", &format!("--{flag}"), "VAL"]);
            assert_eq!(
                parsed.value(flag),
                Some("VAL"),
                "VALUE_FLAG {flag} should consume its value"
            );
        }
    }

    #[test]
    fn parse_duplicate_value_flag_last_wins() {
        let parsed = p(&["recall", "--limit", "5", "--limit", "9"]);
        assert_eq!(parsed.value("limit"), Some("9"));
    }

    #[test]
    fn parse_empty_slash_token_becomes_empty_command() {
        let parsed = p(&["/"]);
        assert_eq!(parsed.command(), Some(""));
    }

    #[test]
    fn parse_lone_double_dash_inserts_empty_bool() {
        let parsed = p(&["--"]);
        assert!(parsed.has(""));
        assert_eq!(parsed.command(), None);
    }

    #[test]
    fn parse_lone_dash_is_positional() {
        let parsed = p(&["-"]);
        assert_eq!(parsed.positionals, vec!["-".to_string()]);
        assert_eq!(parsed.command(), Some("-"));
    }

    #[test]
    fn parse_unicode_and_spaces_preserved_in_values() {
        let parsed = p(&["remember", "--tags", "привет мир"]);
        assert_eq!(parsed.value("tags"), Some("привет мир"));
    }
}
