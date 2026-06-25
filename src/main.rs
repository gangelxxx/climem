//! `cm`, the active-memory tool. This is the entry point: parse the args, work
//! out which memory folder we're using, run one command, print JSONL, and exit.
//! Every call is its own short-lived process, by design (desc.md §9).

mod chunk;
mod cli;
mod code;
mod commands;
mod config;
mod deinit;
mod doctor;
mod embed;
mod export;
mod graph;
mod help;
mod import;
mod init;
mod note;
mod output;
mod search;
mod store;
mod util;

use cli::Parsed;
use commands::Ctx;
use std::path::{Path, PathBuf};
use util::{AppError, Result};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = Parsed::parse(&args);
    if let Err(e) = run(&parsed) {
        eprintln!("error: {}", e.msg);
        if let Some(hint) = &e.hint {
            eprintln!("\nexample:\n  {hint}");
        }
        eprintln!("\nFull contract: `cm help`.");
        std::process::exit(1);
    }
}

fn run(p: &Parsed) -> Result<()> {
    let cmd = match p.command() {
        None => "help", // nothing given, or just -h/--help
        Some(c) => c,
    };

    match cmd {
        "help" | "--help" => {
            print!("{}", help::HELP);
            Ok(())
        }
        "init" => init::run(p),
        "deinit" => deinit::run(p),
        other => {
            let dir = resolve_dir(p)?;
            let ctx = Ctx::new(&dir);
            dispatch(other, p, &ctx)
        }
    }
}

fn dispatch(cmd: &str, p: &Parsed, ctx: &Ctx) -> Result<()> {
    match cmd {
        "remember" => commands::remember(p, ctx),
        "feedback" => commands::feedback(p, ctx),
        "recall" => commands::recall(p, ctx),
        "get" => commands::get(p, ctx),
        "list" => commands::list(p, ctx),
        "related" => commands::related(p, ctx),
        "backlinks" => commands::backlinks(p, ctx),
        "forget" => commands::forget(p, ctx),
        "import" => commands::import(p, ctx),
        "export" => commands::export(p, ctx),
        "reindex" => commands::reindex(p, ctx),
        "map" => commands::map(p, ctx),
        "log" => commands::log(p, ctx),
        "config" => commands::config(p, ctx),
        "doctor" => doctor::run(p, ctx),
        unknown => Err(AppError::with_hint(
            format!("unknown command '{unknown}'"),
            "command list: `cm help`.",
        )),
    }
}

/// Pick the memory folder: --dir if given, else MEMORY_DIR, else wherever the
/// binary itself lives.
fn resolve_dir(p: &Parsed) -> Result<PathBuf> {
    if let Some(d) = p.value("dir") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    if let Ok(d) = std::env::var("MEMORY_DIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    let exe = std::env::current_exe()?;
    Ok(exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Parsed {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn resolve_dir_prefers_dir_flag() {
        // A non-empty --dir wins outright; we never look at the env here.
        let p = parsed(&["config", "--dir", "/explicit/path"]);
        assert_eq!(resolve_dir(&p).unwrap(), PathBuf::from("/explicit/path"));
    }

    #[test]
    fn resolve_dir_empty_dir_falls_back_to_env() {
        // An empty --dir falls through to MEMORY_DIR, which is the only thing read here.
        std::env::set_var("MEMORY_DIR", "/from/env");
        let got = resolve_dir(&parsed(&["config", "--dir", ""])).unwrap();
        std::env::remove_var("MEMORY_DIR");
        assert_eq!(got, PathBuf::from("/from/env"));
    }
}
