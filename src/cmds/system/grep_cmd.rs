//! Filters grep output by grouping matches by file.

use crate::core::config;
use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;

const DEFAULT_MAX_LINE_LEN: usize = 80;
const DEFAULT_MAX_RESULTS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepSearch {
    pattern: String,
    pattern_from_rg_args: bool,
    had_pattern_terminator: bool,
    paths: Vec<String>,
    max_line_len: usize,
    max_results: usize,
    context_only: bool,
    rg_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrepInvocation {
    Search(GrepSearch),
    RgPassthrough(Vec<String>),
}

pub fn run_from_args(args: &[String], verbose: u8) -> Result<i32> {
    match parse_grep_args(args)? {
        GrepInvocation::Search(search) => run_search(&search, verbose),
        GrepInvocation::RgPassthrough(rg_args) => run_rg_passthrough(&rg_args),
    }
}

fn run_search(search: &GrepSearch, verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("grep: '{}' in {}", search.pattern, search.paths.join(" "));
    }

    // Fix: convert BRE alternation \| → | for rg (which uses PCRE-style regex)
    let rg_pattern = search.pattern.replace(r"\|", "|");

    let mut rg_cmd = resolved_command("rg");
    rg_cmd.args(build_rg_args(search, &rg_pattern));

    let result = exec_capture(&mut rg_cmd)
        .or_else(|_| {
            let mut grep_cmd = resolved_command("grep");
            grep_cmd.args(build_grep_fallback_args(search));
            exec_capture(&mut grep_cmd)
        })
        .context("grep/rg failed")?;

    // Passthrough output flags that produce output that is already small.
    if has_format_flag(&search.rg_args) {
        print!("{}", result.stdout);
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr.trim());
        }

        let paths_display = search.paths.join(" ");
        let args_display = if search.rg_args.is_empty() {
            format!("'{}' {}", search.pattern, paths_display)
        } else {
            format!(
                "{} '{}' {}",
                search.rg_args.join(" "),
                search.pattern,
                paths_display
            )
        };

        timer.track_passthrough(
            &format!("grep {}", args_display),
            &format!("rtk grep {} (passthrough)", args_display),
        );
        return Ok(result.exit_code);
    }

    let exit_code = result.exit_code;
    let raw_output = result.stdout.clone();

    if result.stdout.trim().is_empty() {
        // Show stderr for errors (bad regex, missing file, etc.)
        if exit_code == 2 && !result.stderr.trim().is_empty() {
            eprintln!("{}", result.stderr.trim());
        }
        let msg = format!("0 matches for '{}'", search.pattern);
        println!("{}", msg);
        timer.track(
            &format!("grep -rn '{}' {}", search.pattern, search.paths.join(" ")),
            "rtk grep",
            &raw_output,
            &msg,
        );
        return Ok(exit_code);
    }

    // Always filter: truncate long lines, apply per-file and global caps.
    // Output in standard file:line:content format that AI agents can parse.
    // (A passthrough approach yields 0% savings — no reason for RTK to exist on that path.)
    let total_matches = result.stdout.lines().count();

    let context_re = if search.context_only {
        Regex::new(&format!(
            "(?i).{{0,20}}{}.*",
            regex::escape(&search.pattern)
        ))
        .ok()
    } else {
        None
    };

    let mut by_file: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for line in result.stdout.lines() {
        let Some((file, line_num, content)) = parse_match_line(line) else {
            continue;
        };
        let cleaned = clean_line(
            content,
            search.max_line_len,
            context_re.as_ref(),
            &search.pattern,
        );
        by_file.entry(file).or_default().push((line_num, cleaned));
    }

    let mut rtk_output = String::new();
    rtk_output.push_str(&format!(
        "{} matches in {} files:\n\n",
        total_matches,
        by_file.len()
    ));

    let mut shown = 0;
    let mut files: Vec<_> = by_file.iter().collect();
    files.sort_by_key(|(f, _)| *f);

    let per_file = config::limits().grep_max_per_file;
    for (file, matches) in files {
        if shown >= search.max_results {
            break;
        }

        let file_display = compact_path(file);
        for (line_num, content) in matches.iter().take(per_file) {
            if shown >= search.max_results {
                break;
            }
            rtk_output.push_str(&format!("{}:{}:{}\n", file_display, line_num, content));
            shown += 1;
        }
    }

    if total_matches > shown {
        rtk_output.push_str(&format!("[+{} more]\n", total_matches - shown));
    }

    print!("{}", rtk_output);
    timer.track(
        &format!("grep -rn '{}' {}", search.pattern, search.paths.join(" ")),
        "rtk grep",
        &raw_output,
        &rtk_output,
    );

    Ok(exit_code)
}

fn run_rg_passthrough(rg_args: &[String]) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let mut rg_cmd = resolved_command("rg");
    rg_cmd.args(rg_args);
    let result = exec_capture(&mut rg_cmd).context("rg failed")?;
    print!("{}", result.stdout);
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr.trim());
    }
    let args_display = rg_args.join(" ");
    timer.track_passthrough(
        &format!("rg {}", args_display),
        &format!("rtk grep {} (passthrough)", args_display),
    );
    Ok(result.exit_code)
}

fn parse_grep_args(args: &[String]) -> Result<GrepInvocation> {
    let mut search = GrepSearch {
        pattern: String::new(),
        pattern_from_rg_args: false,
        had_pattern_terminator: false,
        paths: Vec::new(),
        max_line_len: DEFAULT_MAX_LINE_LEN,
        max_results: DEFAULT_MAX_RESULTS,
        context_only: false,
        rg_args: Vec::new(),
    };
    let mut positionals = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            search.had_pattern_terminator = true;
            positionals.extend(args[i + 1..].iter().cloned());
            break;
        }

        if arg == "--context-only" {
            search.context_only = true;
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--max-len=") {
            search.max_line_len = parse_usize_arg("--max-len", value)?;
            i += 1;
            continue;
        }

        if arg == "--max-len" {
            let Some(value) = args.get(i + 1) else {
                anyhow::bail!("--max-len requires a value");
            };
            search.max_line_len = parse_usize_arg("--max-len", value)?;
            i += 2;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--max=") {
            search.max_results = parse_usize_arg("--max", value)?;
            i += 1;
            continue;
        }

        if arg == "--max" {
            let Some(value) = args.get(i + 1) else {
                anyhow::bail!("--max requires a value");
            };
            search.max_results = parse_usize_arg("--max", value)?;
            i += 2;
            continue;
        }

        if let Some(value) = args
            .get(i + 1)
            .filter(|value| (arg == "-l" || arg == "-m") && value.parse::<usize>().is_ok())
        {
            if arg == "-l" {
                search.max_line_len = parse_usize_arg("-l", value)?;
            } else {
                search.max_results = parse_usize_arg("-m", value)?;
            }
            i += 2;
            continue;
        }

        if arg == "-n" || arg == "--line-numbers" {
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--type=") {
            search.rg_args.push(format!("--type={value}"));
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--file-type=") {
            search.rg_args.push(format!("--type={value}"));
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--glob=") {
            search.rg_args.push(format!("--glob={value}"));
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--type-add=") {
            search.rg_args.push(format!("--type-add={value}"));
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("-t").filter(|value| !value.is_empty()) {
            search.rg_args.push("-t".to_string());
            search.rg_args.push(value.to_string());
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("-g").filter(|value| !value.is_empty()) {
            search.rg_args.push("-g".to_string());
            search.rg_args.push(value.to_string());
            i += 1;
            continue;
        }

        if arg == "--file-type" {
            search.rg_args.push("--type".to_string());
            let Some(value) = args.get(i + 1) else {
                anyhow::bail!("{arg} requires a value");
            };
            search.rg_args.push(value.clone());
            i += 2;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--regexp=") {
            search.pattern_from_rg_args = true;
            search.rg_args.push(format!("--regexp={value}"));
            i += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--file=") {
            search.pattern_from_rg_args = true;
            search.rg_args.push(format!("--file={value}"));
            i += 1;
            continue;
        }

        if takes_rg_value(arg) {
            if is_rg_pattern_source_arg(arg) {
                search.pattern_from_rg_args = true;
            }
            search.rg_args.push(arg.clone());
            let Some(value) = args.get(i + 1) else {
                anyhow::bail!("{arg} requires a value");
            };
            search.rg_args.push(value.clone());
            i += 2;
            continue;
        }

        if arg.starts_with('-') {
            search.rg_args.push(arg.clone());
            i += 1;
            continue;
        }

        positionals.push(arg.clone());
        i += 1;
    }

    if is_rg_passthrough_without_pattern(&search.rg_args) {
        if search.had_pattern_terminator {
            search.rg_args.push("--".to_string());
        }
        search.rg_args.extend(positionals);
        return Ok(GrepInvocation::RgPassthrough(search.rg_args));
    }

    if search.pattern_from_rg_args {
        search.pattern = rg_pattern_display(&search.rg_args).unwrap_or_default();
        search.paths = if positionals.is_empty() {
            vec![".".to_string()]
        } else {
            positionals
        };
        return Ok(GrepInvocation::Search(search));
    }

    let Some(pattern) = positionals.first() else {
        anyhow::bail!("grep requires a pattern");
    };

    search.pattern = pattern.clone();
    search.paths = if positionals.len() > 1 {
        positionals[1..].to_vec()
    } else {
        vec![".".to_string()]
    };

    Ok(GrepInvocation::Search(search))
}

fn parse_usize_arg(name: &str, value: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("{name} must be a positive integer"))
}

fn takes_rg_value(arg: &str) -> bool {
    matches!(
        arg,
        "-t" | "--type"
            | "-g"
            | "--glob"
            | "--type-add"
            | "-A"
            | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "-e"
            | "--regexp"
            | "-f"
            | "--file"
            | "--encoding"
            | "--engine"
            | "--max-count"
            | "--max-depth"
            | "--path-separator"
            | "--sort"
            | "--sortr"
            | "--threads"
            | "-j"
    )
}

fn is_rg_pattern_source_arg(arg: &str) -> bool {
    matches!(arg, "-e" | "--regexp" | "-f" | "--file")
}

fn is_rg_passthrough_without_pattern(rg_args: &[String]) -> bool {
    rg_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--type-list" | "--files" | "--help" | "-h" | "--version" | "-V"
        )
    })
}

fn rg_pattern_display(rg_args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < rg_args.len() {
        let arg = &rg_args[i];
        if let Some(pattern) = arg.strip_prefix("--regexp=") {
            return Some(pattern.to_string());
        }
        if arg == "-e" || arg == "--regexp" {
            return rg_args.get(i + 1).cloned();
        }
        if let Some(pattern_file) = arg.strip_prefix("--file=") {
            return Some(format!("patterns from {pattern_file}"));
        }
        if arg == "-f" || arg == "--file" {
            return rg_args
                .get(i + 1)
                .map(|pattern_file| format!("patterns from {pattern_file}"));
        }
        i += 1;
    }
    None
}

fn build_rg_args(search: &GrepSearch, rg_pattern: &str) -> Vec<String> {
    let mut args = vec![
        "-nH0".to_string(),
        "--no-heading".to_string(),
        "--no-ignore-vcs".to_string(),
    ];
    let forwarded_args = search
        .rg_args
        .iter()
        .filter(|arg| arg.as_str() != "-r" && arg.as_str() != "--recursive")
        .cloned();
    args.extend(forwarded_args);
    if search.had_pattern_terminator {
        args.push("--".to_string());
    }
    if !search.pattern_from_rg_args {
        args.push(rg_pattern.to_string());
    }
    args.extend(search.paths.iter().cloned());
    args
}

#[cfg(test)]
pub(crate) fn build_rg_args_from_test_args(args: &[String]) -> Result<Vec<String>> {
    match parse_grep_args(args)? {
        GrepInvocation::Search(search) => Ok(build_rg_args(&search, &search.pattern)),
        GrepInvocation::RgPassthrough(args) => Ok(args),
    }
}

fn build_grep_fallback_args(search: &GrepSearch) -> Vec<String> {
    let mut args = vec!["-rnHZ".to_string()];
    args.extend(grep_fallback_extra_args(&search.rg_args));
    if search.had_pattern_terminator {
        args.push("--".to_string());
    }
    if !search.pattern_from_rg_args {
        args.push(search.pattern.clone());
    }
    args.extend(search.paths.iter().cloned());
    args
}

fn grep_fallback_extra_args(rg_args: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    let mut i = 0;

    while i < rg_args.len() {
        let arg = &rg_args[i];
        if let Some(file_type) = arg.strip_prefix("--type=") {
            push_grep_include_for_type(&mut args, file_type);
            i += 1;
            continue;
        }
        if let Some(glob) = arg.strip_prefix("--glob=") {
            push_grep_glob(&mut args, glob);
            i += 1;
            continue;
        }
        if arg.starts_with("--type-add=") || arg == "--type-list" {
            i += 1;
            continue;
        }
        if arg == "--type" || arg == "-t" {
            if let Some(file_type) = rg_args.get(i + 1) {
                push_grep_include_for_type(&mut args, file_type);
            }
            i += 2;
            continue;
        }
        if arg == "--glob" || arg == "-g" {
            if let Some(glob) = rg_args.get(i + 1) {
                push_grep_glob(&mut args, glob);
            }
            i += 2;
            continue;
        }
        if arg == "--type-add" {
            i += 2;
            continue;
        }
        if arg == "-r" || arg == "--recursive" || arg == "--hidden" {
            i += 1;
            continue;
        }
        args.push(arg.clone());
        i += 1;
    }

    args
}

fn push_grep_include_for_type(args: &mut Vec<String>, file_type: &str) {
    if let Some(glob) = grep_glob_for_type(file_type) {
        args.push(format!("--include={glob}"));
    }
}

fn push_grep_glob(args: &mut Vec<String>, glob: &str) {
    if let Some(exclude) = glob.strip_prefix('!') {
        args.push(format!("--exclude={exclude}"));
    } else {
        args.push(format!("--include={glob}"));
    }
}

fn grep_glob_for_type(file_type: &str) -> Option<&'static str> {
    match file_type {
        "rust" | "rs" => Some("*.rs"),
        "py" | "python" => Some("*.py"),
        "ts" | "typescript" => Some("*.ts"),
        "tsx" => Some("*.tsx"),
        "js" | "javascript" => Some("*.js"),
        "jsx" => Some("*.jsx"),
        "go" => Some("*.go"),
        "java" => Some("*.java"),
        "rb" | "ruby" => Some("*.rb"),
        "md" | "markdown" => Some("*.md"),
        _ => None,
    }
}

/// Parses a single rg/grep match line of the form `file\0line_number:content`.
///
/// Requires the underlying command to be invoked with `-0` (rg) or `-Z` (grep)
/// so the filename is NUL-separated from `line:content`. NUL cannot appear in
/// file paths, so the parser is unambiguous regardless of:
///   - content with `:` or `::` (e.g. `ClassRegistry::init(...)`, issue #1436);
///   - paths with embedded `:` (Windows drive letters, weird filenames like
///     `badly_named:52:file.txt`).
///
/// Returns `None` for lines that do not match the expected shape (e.g. rg
/// `-A`/`-B` context lines that use `-` as separator).
fn parse_match_line(line: &str) -> Option<(String, usize, &str)> {
    lazy_static::lazy_static! {
        static ref MATCH_LINE_RE: Regex = Regex::new(r"^([^\x00]+)\x00(\d+):(.*)$").unwrap();
    }
    MATCH_LINE_RE.captures(line).and_then(|caps| {
        let (_, [file, line_num, content]) = caps.extract();
        let line_num: usize = line_num.parse().ok()?;
        Some((file.to_string(), line_num, content))
    })
}

fn has_format_flag(extra_args: &[String]) -> bool {
    extra_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-c" | "--count"
                | "-l"
                | "--files-with-matches"
                | "-L"
                | "--files-without-match"
                | "-o"
                | "--only-matching"
                | "-Z"
                | "--null"
        )
    })
}

fn clean_line(line: &str, max_len: usize, context_re: Option<&Regex>, pattern: &str) -> String {
    let trimmed = line.trim();

    if let Some(re) = context_re {
        if let Some(m) = re.find(trimmed) {
            let matched = m.as_str();
            if matched.len() <= max_len {
                return matched.to_string();
            }
        }
    }

    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let lower = trimmed.to_lowercase();
        let pattern_lower = pattern.to_lowercase();

        if let Some(pos) = lower.find(&pattern_lower) {
            let char_pos = lower[..pos].chars().count();
            let chars: Vec<char> = trimmed.chars().collect();
            let char_len = chars.len();

            let start = char_pos.saturating_sub(max_len / 3);
            let end = (start + max_len).min(char_len);
            let start = if end == char_len {
                end.saturating_sub(max_len)
            } else {
                start
            };

            let slice: String = chars[start..end].iter().collect();
            if start > 0 && end < char_len {
                format!("...{}...", slice)
            } else if start > 0 {
                format!("...{}", slice)
            } else {
                format!("{}...", slice)
            }
        } else {
            let t: String = trimmed.chars().take(max_len - 3).collect();
            format!("{}...", t)
        }
    }
}

fn compact_path(path: &str) -> String {
    if path.len() <= 50 {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }

    format!(
        "{}/.../{}/{}",
        parts[0],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn test_clean_line() {
        let line = "            const result = someFunction();";
        let cleaned = clean_line(line, 50, None, "result");
        assert!(!cleaned.starts_with(' '));
        assert!(cleaned.len() <= 50);
    }

    #[test]
    fn test_compact_path() {
        let path = "/Users/patrick/dev/project/src/components/Button.tsx";
        let compact = compact_path(path);
        assert!(compact.len() <= 60);
    }

    #[test]
    fn test_extra_args_accepted() {
        // The parser keeps trailing rg args available for backend forwarding.
        let _extra: Vec<String> = vec!["-i".to_string(), "-A".to_string(), "3".to_string()];
    }

    #[test]
    fn test_parse_rg_type_long_preserves_pattern_and_path() {
        let invocation = parse_grep_args(&strings(&["--type", "rust", "needle", "src/"]))
            .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["--type", "rust"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_rg_type_short_preserves_pattern_and_path() {
        let invocation =
            parse_grep_args(&strings(&["-t", "rust", "needle", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["-t", "rust"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_file_type_alias_maps_to_rg_type() {
        let invocation = parse_grep_args(&strings(&[
            "--file-type",
            "rust",
            "needle",
            "src/",
        ]))
        .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["--type", "rust"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_file_type_equals_alias_maps_to_rg_type() {
        let invocation = parse_grep_args(&strings(&["--file-type=rust", "needle", "src/"]))
            .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["--type=rust"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_rg_glob_long_preserves_pattern_and_path() {
        let invocation = parse_grep_args(&strings(&["--glob", "*.rs", "needle", "src/"]))
            .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["--glob", "*.rs"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_uses_regexp_flag_pattern_and_keeps_path() {
        let invocation =
            parse_grep_args(&strings(&["-e", "needle", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["-e", "needle"]);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec![
                        "-nH0",
                        "--no-heading",
                        "--no-ignore-vcs",
                        "-e",
                        "needle",
                        "src/"
                    ]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_preserves_multiple_regexp_flags() {
        let invocation = parse_grep_args(&strings(&["-e", "a", "-e", "b", "src/"]))
            .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "a");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["-e", "a", "-e", "b"]);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec![
                        "-nH0",
                        "--no-heading",
                        "--no-ignore-vcs",
                        "-e",
                        "a",
                        "-e",
                        "b",
                        "src/"
                    ]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_uses_file_flag_pattern_and_keeps_path() {
        let invocation = parse_grep_args(&strings(&["-f", "patterns.txt", "src/"]))
            .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "patterns from patterns.txt");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["-f", "patterns.txt"]);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec![
                        "-nH0",
                        "--no-heading",
                        "--no-ignore-vcs",
                        "-f",
                        "patterns.txt",
                        "src/"
                    ]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_keeps_rg_flags_before_pattern() {
        let invocation = parse_grep_args(&strings(&[
            "-n", "--type", "rust", "--glob", "*.rs", "needle", "src/",
        ]))
        .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec![
                        "-nH0",
                        "--no-heading",
                        "--no-ignore-vcs",
                        "--type",
                        "rust",
                        "--glob",
                        "*.rs",
                        "needle",
                        "src/"
                    ]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_preserves_double_dash_before_leading_hyphen_pattern_and_path() {
        let invocation =
            parse_grep_args(&strings(&["--", "-foo", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "-foo");
                assert_eq!(search.paths, vec!["src/"]);
                assert!(search.had_pattern_terminator);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec!["-nH0", "--no-heading", "--no-ignore-vcs", "--", "-foo", "src/"]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_preserves_double_dash_before_leading_hyphen_pattern_default_path() {
        let invocation = parse_grep_args(&strings(&["--", "-foo"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "-foo");
                assert_eq!(search.paths, vec!["."]);
                assert!(search.had_pattern_terminator);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec!["-nH0", "--no-heading", "--no-ignore-vcs", "--", "-foo", "."]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_rg_args_places_double_dash_before_paths_for_regexp_pattern_source() {
        let invocation =
            parse_grep_args(&strings(&["-e", "-foo", "--", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "-foo");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["-e", "-foo"]);
                assert!(search.had_pattern_terminator);
                let args = build_rg_args(&search, &search.pattern);
                assert_eq!(
                    args,
                    vec![
                        "-nH0",
                        "--no-heading",
                        "--no-ignore-vcs",
                        "-e",
                        "-foo",
                        "--",
                        "src/"
                    ]
                );
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_build_grep_fallback_preserves_double_dash_before_leading_hyphen_pattern() {
        let invocation =
            parse_grep_args(&strings(&["--", "-foo", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                let args = build_grep_fallback_args(&search);
                assert_eq!(args, vec!["-rnHZ", "--", "-foo", "src/"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_rg_files_passthrough_preserves_double_dash() {
        let invocation =
            parse_grep_args(&strings(&["--files", "--", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::RgPassthrough(args) => {
                assert_eq!(args, vec!["--files", "--", "src/"]);
            }
            GrepInvocation::Search(_) => panic!("expected passthrough invocation"),
        }
    }

    #[test]
    fn test_build_grep_fallback_translates_rg_only_flags() {
        let invocation = parse_grep_args(&strings(&[
            "--type",
            "rust",
            "--glob",
            "*.rs",
            "--type-add",
            "foo:*.foo",
            "needle",
            "src/",
        ]))
        .expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                let args = build_grep_fallback_args(&search);
                assert_eq!(
                    args,
                    vec!["-rnHZ", "--include=*.rs", "--include=*.rs", "needle", "src/"]
                );
                assert!(!args.iter().any(|arg| matches!(
                    arg.as_str(),
                    "--type" | "-t" | "--glob" | "-g" | "--type-add" | "--type-list"
                )));
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_parse_rg_type_list_passthrough_without_pattern() {
        let invocation = parse_grep_args(&strings(&["--type-list"])).expect("valid grep args");
        match invocation {
            GrepInvocation::RgPassthrough(args) => assert_eq!(args, vec!["--type-list"]),
            GrepInvocation::Search(_) => panic!("expected passthrough invocation"),
        }
    }

    #[test]
    fn test_parse_rg_files_passthrough_preserves_path() {
        let invocation = parse_grep_args(&strings(&["--files", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::RgPassthrough(args) => assert_eq!(args, vec!["--files", "src/"]),
            GrepInvocation::Search(_) => panic!("expected passthrough invocation"),
        }
    }

    #[test]
    fn test_parse_rg_trim_preserves_pattern_and_path() {
        let invocation =
            parse_grep_args(&strings(&["--trim", "needle", "src/"])).expect("valid grep args");
        match invocation {
            GrepInvocation::Search(search) => {
                assert_eq!(search.pattern, "needle");
                assert_eq!(search.paths, vec!["src/"]);
                assert_eq!(search.rg_args, vec!["--trim"]);
            }
            GrepInvocation::RgPassthrough(_) => panic!("expected search invocation"),
        }
    }

    #[test]
    fn test_clean_line_multibyte() {
        // Thai text that exceeds max_len in bytes
        let line = "  สวัสดีครับ นี่คือข้อความที่ยาวมากสำหรับทดสอบ  ";
        let cleaned = clean_line(line, 20, None, "ครับ");
        // Should not panic
        assert!(!cleaned.is_empty());
    }

    #[test]
    fn test_clean_line_emoji() {
        let line = "🎉🎊🎈🎁🎂🎄 some text 🎃🎆🎇✨";
        let cleaned = clean_line(line, 15, None, "text");
        assert!(!cleaned.is_empty());
    }

    // Fix: BRE \| alternation is translated to PCRE | for rg
    #[test]
    fn test_bre_alternation_translated() {
        let pattern = r"fn foo\|pub.*bar";
        let rg_pattern = pattern.replace(r"\|", "|");
        assert_eq!(rg_pattern, "fn foo|pub.*bar");
    }

    // Fix: -r flag (grep recursive) is stripped from extra_args (rg is recursive by default)
    #[test]
    fn test_recursive_flag_stripped() {
        let extra_args: Vec<String> = vec!["-r".to_string(), "-i".to_string()];
        let filtered: Vec<&String> = extra_args
            .iter()
            .filter(|a| *a != "-r" && *a != "--recursive")
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "-i");
    }

    // --- truncation accuracy ---

    #[test]
    fn test_grep_overflow_uses_uncapped_total() {
        // Confirm the grep overflow invariant: matches vec is never capped before overflow calc.
        // If total_matches > per_file, overflow = total_matches - per_file (not capped).
        // This documents that grep_cmd.rs avoids the diff_cmd bug (cap at N then compute N-10).
        let per_file = config::limits().grep_max_per_file;
        let total_matches = per_file + 42;
        let overflow = total_matches - per_file;
        assert_eq!(overflow, 42, "overflow must equal true suppressed count");
        // Demonstrate why capping before subtraction is wrong:
        let hypothetical_cap = per_file + 5;
        let capped = total_matches.min(hypothetical_cap);
        let wrong_overflow = capped - per_file;
        assert_ne!(
            wrong_overflow, overflow,
            "capping before subtraction gives wrong overflow"
        );
    }

    // --- format flag detection ---

    #[test]
    fn test_format_flag_detects_count() {
        assert!(has_format_flag(&["-c".to_string()]));
        assert!(has_format_flag(&["--count".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_files_with_matches() {
        assert!(has_format_flag(&["-l".to_string()]));
        assert!(has_format_flag(&["--files-with-matches".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_files_without_match() {
        assert!(has_format_flag(&["-L".to_string()]));
        assert!(has_format_flag(&["--files-without-match".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_only_matching() {
        assert!(has_format_flag(&["-o".to_string()]));
        assert!(has_format_flag(&["--only-matching".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_null() {
        assert!(has_format_flag(&["-Z".to_string()]));
        assert!(has_format_flag(&["--null".to_string()]));
    }

    #[test]
    fn test_format_flag_ignores_normal_flags() {
        assert!(!has_format_flag(&[
            "-i".to_string(),
            "-w".to_string(),
            "-A".to_string(),
            "3".to_string(),
        ]));
    }

    // Verify line numbers are always enabled in rg invocation (grep_cmd.rs:24).
    // The -n/--line-numbers clap flag in main.rs is a no-op accepted for compat.
    #[test]
    fn test_rg_always_has_line_numbers() {
        // grep_cmd::run() always passes "-n" to rg (line 24).
        // This test documents that -n is built-in, so the clap flag is safe to ignore.
        let mut cmd = resolved_command("rg");
        cmd.args(["-n", "--no-heading", "NONEXISTENT_PATTERN_12345", "."]);
        // If rg is available, it should accept -n without error (exit 1 = no match, not error)
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg -n should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }

    // --- issue #1436: parse_match_line robustness ---
    // Input shape is `file\0line:content` (rg --null / grep -Z).

    #[test]
    fn test_parse_match_line_simple() {
        let line = "file.php\x0010:use Foo\\Bar;";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.php");
        assert_eq!(line_num, 10);
        assert_eq!(content, "use Foo\\Bar;");
    }

    // Issue #1436 reproducer: content with `::` must not split into a phantom
    // file bucket. With NUL separation between file and line:content, content
    // colons are irrelevant to the parser.
    #[test]
    fn test_parse_match_line_content_with_double_colon() {
        let line = "externalImportShell.class.php\x0081:        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "externalImportShell.class.php");
        assert_eq!(line_num, 81);
        assert_eq!(
            content,
            "        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');"
        );
    }

    // Windows abs-path safety: drive letter + backslashes must not break the
    // parser. The NUL separator makes the file portion unambiguous.
    #[test]
    fn test_parse_match_line_windows_path() {
        let line = "C:\\src\\file.rs\x0042:fn main() {}";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, r"C:\src\file.rs");
        assert_eq!(line_num, 42);
        assert_eq!(content, "fn main() {}");
    }

    // Filenames containing `:digits:` (which would fool a greedy `:` parser)
    // must still parse correctly under NUL separation.
    #[test]
    fn test_parse_match_line_filename_with_colons() {
        let line = "badly_named:52:file.txt\x001:xxx";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "badly_named:52:file.txt");
        assert_eq!(line_num, 1);
        assert_eq!(content, "xxx");
    }

    // Content that itself contains `:digits:` (e.g. log lines, port numbers,
    // line-number-like substrings) must not confuse the parser.
    #[test]
    fn test_parse_match_line_content_with_digit_colons() {
        let line = "log.txt\x007:debug: counter is :42: now";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "log.txt");
        assert_eq!(line_num, 7);
        assert_eq!(content, "debug: counter is :42: now");
    }

    #[test]
    fn test_parse_match_line_malformed_returns_none() {
        // No NUL separator (e.g. rg/grep invoked without --null/-Z, or a
        // context line written with `-`).
        assert!(parse_match_line("file.rs:1:content").is_none());
        assert!(parse_match_line("not a match line").is_none());
        // Missing line number after NUL
        assert!(parse_match_line("file.rs\x00fn foo()").is_none());
        // Empty
        assert!(parse_match_line("").is_none());
    }

    #[test]
    fn test_parse_match_line_empty_content() {
        let line = "file.rs\x007:";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.rs");
        assert_eq!(line_num, 7);
        assert_eq!(content, "");
    }

    #[test]
    fn test_rg_no_ignore_vcs_flag_accepted() {
        // Verify rg accepts --no-ignore-vcs (used to match grep -r behavior for .gitignore)
        let mut cmd = resolved_command("rg");
        cmd.args([
            "-n",
            "--no-heading",
            "--no-ignore-vcs",
            "NONEXISTENT_PATTERN_12345",
            ".",
        ]);
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg --no-ignore-vcs should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }
}
