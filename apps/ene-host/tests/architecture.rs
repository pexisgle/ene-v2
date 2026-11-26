//! Workspace architecture ratchets.
//!
//! Each rule pins a structural property the crate layering depends on. A rule
//! fails when its measured count rises above the baseline in [`Baselines`].
//! Lower a baseline in the same commit that removes the last violation; never
//! raise one to get a change through.
//!
//! # Why this is a test and not a script
//!
//! The rules used to live in `scripts/check-architecture.py`, which stripped
//! `#[cfg(test)]` items by counting braces. A brace inside a string literal or
//! a block comment threw that count off, so a test-only item could be mistaken
//! for production code. This version parses each file with `syn` and blanks the
//! real spans, so the two can no longer be confused.
//!
//! # Rules that are not here
//!
//! Two former rules are enforced by the compiler and were deleted rather than
//! ported, so there is no second copy to drift:
//!
//! - no `println!`/`eprintln!` in production: `clippy::print_stdout` and
//!   `clippy::print_stderr`.
//! - no `unsafe` in `apps/ene-desktop`: `#![forbid(unsafe_code)]` in its
//!   `lib.rs` and `main.rs`.
//!
//! Two others moved to the tools built for them: crate-dependency bans
//! (`ene-rpc` must not be reached from an L0 domain crate) are `[[bans.deny]]`
//! entries in `deny.toml`, which reads the resolved dependency graph instead of
//! scraping manifests.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;

type Res<T> = Result<T, Box<dyn Error>>;

/// A production file over this many lines needs splitting by responsibility.
const PROD_FILE_LINE_LIMIT: usize = 500;
/// How many files one type's inherent `impl` blocks may be spread across.
const DEFAULT_MAX_IMPL_FILES: usize = 3;

/// Measured counts this tree is allowed to have today. Each entry is a debt
/// ceiling, not a target: the goal for every value is zero.
#[derive(Debug)]
struct Baselines {
    ui_rpc_refs: usize,
    db_upward_refs: usize,
    providers_model_refs: usize,
    host_world_refs: usize,
    impl_over_default: usize,
    long_prod_files: usize,
    rpc_ecs_resources: usize,
    prod_block_on: usize,
}

const BASELINES: Baselines = Baselines {
    ui_rpc_refs: 0,
    db_upward_refs: 0,
    providers_model_refs: 0,
    host_world_refs: 125,
    impl_over_default: 0,
    long_prod_files: 0,
    rpc_ecs_resources: 0,
    prod_block_on: 0,
};

/// One rule's measurement, kept whole so a failure can print what it saw.
#[derive(Debug)]
struct Measurement {
    rule: &'static str,
    count: usize,
    max: usize,
    detail: String,
}

impl Measurement {
    fn verdict(&self) -> Option<String> {
        (self.count > self.max).then(|| {
            format!(
                "{}: measured {} (baseline max {}). {}",
                self.rule, self.count, self.max, self.detail
            )
        })
    }
}

fn workspace_root() -> Res<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest.join("../..").canonicalize()?)
}

fn rust_files_under(root: &Path, base: &str) -> Res<Vec<PathBuf>> {
    let mut found = Vec::new();
    collect_rust_files(&root.join(base), &mut found)?;
    found.sort();
    Ok(found)
}

fn collect_rust_files(dir: &Path, found: &mut Vec<PathBuf>) -> Res<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    Ok(())
}

/// Files whose whole purpose is testing are never production code, whichever
/// rule is asking.
fn is_test_path(rel: &Path) -> bool {
    let in_test_dir = rel
        .components()
        .take(rel.components().count().saturating_sub(1))
        .any(|part| {
            let name = part.as_os_str().to_string_lossy();
            name == "tests"
                || name == "examples"
                || name.ends_with("_tests")
                || name.ends_with("_test")
        });
    if in_test_dir {
        return true;
    }
    let file = rel.file_name().unwrap_or_default().to_string_lossy();
    file == "tests.rs" || file.ends_with("_tests.rs") || file.ends_with("_test.rs")
}

fn is_production(rel: &Path) -> bool {
    !is_test_path(rel) && rel.file_name().is_some_and(|name| name != "build.rs")
}

/// A file with its production lines, and how many of them there are.
struct Production {
    /// The full text with `#[cfg(test)]` items blanked. Line numbers still
    /// point at the real file, which is what a violation report needs.
    text: String,
    /// How many lines survive the blanking. This is the count rule 9 ratchets;
    /// counting `text` instead would count the blanks too.
    lines: usize,
}

/// Blank every `#[cfg(test)]` item out of the file, keeping the rest.
///
/// Lines are blanked rather than removed so a reported line number still points
/// at the real file. Parsing with `syn` means a brace in a string literal or a
/// comment cannot move the boundary, which is what defeated the old brace count.
fn production_source(path: &Path) -> Res<Production> {
    let text = std::fs::read_to_string(path)?;
    let Ok(parsed) = syn::parse_file(&text) else {
        // Not parseable as an item list (a generated or macro-only file). Such
        // a file cannot carry a production item the rules look for.
        let lines = text.lines().count();
        return Ok(Production { text, lines });
    };
    let raw_lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![true; raw_lines.len()];
    mask_per_test_items(&parsed.items, &mut keep);

    let mut out = String::with_capacity(text.len());
    let mut lines = 0;
    for (index, line) in raw_lines.iter().enumerate() {
        if keep.get(index).copied().unwrap_or(true) {
            out.push_str(line);
            lines += 1;
        }
        out.push('\n');
    }
    Ok(Production { text: out, lines })
}

/// Mark the line span of each `#[cfg(test)]` item as not-production, then walk
/// into inline modules, where test modules usually live.
fn mask_per_test_items(items: &[syn::Item], keep: &mut [bool]) {
    for item in items {
        let attrs = match item {
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Enum(item) => &item.attrs,
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Struct(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::Type(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            _ => {
                if let syn::Item::Mod(module) = item
                    && let Some((_, statements)) = &module.content
                {
                    mask_per_test_items(statements, keep);
                }
                continue;
            }
        };
        if is_per_test(attrs) {
            let span = item.span();
            let first = attrs
                .first()
                .map_or_else(|| span.start().line, |attr| attr.span().start().line);
            let last = span.end().line;
            for slot in keep.iter_mut().take(last).skip(first.saturating_sub(1)) {
                *slot = false;
            }
            continue;
        }
        if let syn::Item::Mod(module) = item
            && let Some((_, statements)) = &module.content
        {
            mask_per_test_items(statements, keep);
        }
    }
}

fn is_per_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr_text_has_test_word(attr))
}

/// A `cfg` attribute whose tokens name `test`, which covers the plain
/// `cfg(test)` and nested forms such as `cfg(any(test, feature = "x"))`. A
/// `feature` value spelled `test` is the one false positive, and no crate here
/// defines one.
fn attr_text_has_test_word(attr: &syn::Attribute) -> bool {
    cfg_names_test(&attr.meta)
}

fn cfg_names_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => list.tokens.to_string().split_whitespace().any(|token| {
            let trimmed = token.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
            trimmed == "test"
        }),
        syn::Meta::NameValue(_) => false,
    }
}

/// Drop a trailing `//` comment while keeping `//` that sits inside a string.
fn strip_line_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        } else if ch == '/' && line[index..].starts_with("//") {
            return &line[..index];
        }
    }
    line
}

fn count_matches(text: &str, pattern: &Regex) -> usize {
    text.lines()
        .map(|line| pattern.find_iter(strip_line_comment(line)).count())
        .sum()
}

fn rule_ui_rpc_leak(root: &Path, base: &Baselines) -> Res<Measurement> {
    let patterns = [
        Regex::new(r"\bw::")?,
        Regex::new(r"\b(?:ene_rpc|buffa|connectrpc)::")?,
        Regex::new(r"\buse\s+(?:ene_rpc|buffa|connectrpc)\b")?,
    ];
    let mut count = 0;
    for path in rust_files_under(root, "apps/ene-desktop/src/ui")? {
        let text = std::fs::read_to_string(&path)?;
        for pattern in &patterns {
            count += count_matches(&text, pattern);
        }
    }
    Ok(Measurement {
        rule: "ui_rpc_leak",
        count,
        max: base.ui_rpc_refs,
        detail: "wire/RPC types must not appear under apps/ene-desktop/src/ui/.".into(),
    })
}

/// Rules of the form "this directory must not name that module".
fn rule_no_upward_refs(
    root: &Path,
    rel_dir: &str,
    pattern: &Regex,
    rule: &'static str,
    max: usize,
    detail: &str,
) -> Res<Measurement> {
    let mut hits = Vec::new();
    for path in rust_files_under(root, rel_dir)? {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) {
            continue;
        }
        let prod = production_source(&path)?;
        for (index, line) in prod.text.lines().enumerate() {
            let cleaned = strip_line_comment(line);
            if pattern.is_match(cleaned) {
                hits.push(format!(
                    "{}:{}: {}",
                    rel.display(),
                    index + 1,
                    cleaned.trim()
                ));
            }
        }
    }
    Ok(Measurement {
        rule,
        count: hits.len(),
        max,
        detail: format!("{detail} offenders: {hits:?}"),
    })
}

fn rule_host_world_refs(root: &Path, base: &Baselines) -> Res<Measurement> {
    let pattern = Regex::new(r"&\s*(?:mut\s+)?World\b")?;
    let mut count = 0;
    let mut per_file = BTreeMap::new();
    for path in rust_files_under(root, "apps/ene-host/src/host")? {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) {
            continue;
        }
        let prod = production_source(&path)?;
        let found = count_matches(&prod.text, &pattern);
        if found > 0 {
            per_file.insert(rel.display().to_string(), found);
            count += found;
        }
    }
    let mut top: Vec<_> = per_file.into_iter().collect();
    top.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    top.truncate(8);
    Ok(Measurement {
        rule: "host_world_refs",
        count,
        max: base.host_world_refs,
        detail: format!("World references belong behind the actor boundary. top: {top:?}"),
    })
}

fn rule_impl_file_spread(root: &Path, base: &Baselines) -> Res<Measurement> {
    let pattern = Regex::new(r"^\s*impl\b(?:<[^>{]*>)?\s+([A-Z][A-Za-z0-9_]*)\b([^\n{]*)")?;
    let trait_impl = Regex::new(r"\bfor\b")?;
    let mut impl_files: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for path in rust_files_under(root, "apps")?
        .into_iter()
        .chain(rust_files_under(root, "crates")?)
    {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) || rel.components().count() < 2 {
            continue;
        }
        let crate_dir = rel
            .components()
            .take(2)
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let prod = production_source(&path)?;
        for line in prod.text.lines() {
            let cleaned = strip_line_comment(line);
            let Some(captures) = pattern.captures(cleaned) else {
                continue;
            };
            let (Some(name), Some(rest)) = (captures.get(1), captures.get(2)) else {
                continue;
            };
            if trait_impl.is_match(rest.as_str()) {
                continue;
            }
            impl_files
                .entry((crate_dir.clone(), name.as_str().to_owned()))
                .or_default()
                .insert(rel.display().to_string());
        }
    }
    let offenders = impl_files
        .iter()
        .filter(|(_, files)| files.len() > DEFAULT_MAX_IMPL_FILES)
        .map(|((dir, name), files)| format!("{dir}::{name} spans {} files: {files:?}", files.len()))
        .collect::<Vec<_>>();
    Ok(Measurement {
        rule: "impl_file_spread",
        count: offenders.len(),
        max: base.impl_over_default,
        detail: format!("one type must not be spread across files. offenders: {offenders:?}"),
    })
}

fn rule_max_prod_file_lines(root: &Path, base: &Baselines) -> Res<Measurement> {
    let mut over = Vec::new();
    for path in rust_files_under(root, "apps")?
        .into_iter()
        .chain(rust_files_under(root, "crates")?)
    {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) {
            continue;
        }
        let lines = production_source(&path)?.lines;
        if lines > PROD_FILE_LINE_LIMIT {
            over.push((lines, rel.display().to_string()));
        }
    }
    over.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(Measurement {
        rule: "max_prod_file_lines",
        count: over.len(),
        max: base.long_prod_files,
        detail: format!("production files over {PROD_FILE_LINE_LIMIT} lines: {over:?}"),
    })
}

fn rule_rpc_no_ecs_resource(root: &Path, base: &Baselines) -> Res<Measurement> {
    let derive = Regex::new(r"#\s*\[\s*derive\s*\(([^)]*)\)\s*\]")?;
    let resource = Regex::new(r"\bResource\b")?;
    let mut found = Vec::new();
    for path in rust_files_under(root, "apps/ene-host/src/server/rpc")? {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) {
            continue;
        }
        let prod = production_source(&path)?;
        for captures in derive.captures_iter(&prod.text) {
            if let Some(names) = captures.get(1)
                && resource.is_match(names.as_str())
            {
                found.push(format!("{}: {}", rel.display(), captures[0].trim()));
            }
        }
    }
    Ok(Measurement {
        rule: "rpc_no_ecs_resource",
        count: found.len(),
        max: base.rpc_ecs_resources,
        detail: format!("the RPC layer must not define ECS resources. found: {found:?}"),
    })
}

fn rule_prod_block_on(root: &Path, base: &Baselines) -> Res<Measurement> {
    let pattern = Regex::new(r"\b(?:block_on|block_in_place)\s*\(")?;
    let mut count = 0;
    let mut per_file = BTreeMap::new();
    for path in rust_files_under(root, "apps")?
        .into_iter()
        .chain(rust_files_under(root, "crates")?)
    {
        let rel = path.strip_prefix(root)?.to_path_buf();
        if !is_production(&rel) {
            continue;
        }
        let prod = production_source(&path)?;
        let found = count_matches(&prod.text, &pattern);
        if found > 0 {
            per_file.insert(rel.display().to_string(), found);
            count += found;
        }
    }
    Ok(Measurement {
        rule: "prod_block_on",
        count,
        max: base.prod_block_on,
        detail: format!(
            "blocking the thread that must stay free stalls unrelated work. sites: {per_file:?}"
        ),
    })
}

fn measure_all(root: &Path) -> Res<Vec<Measurement>> {
    Ok(vec![
        rule_ui_rpc_leak(root, &BASELINES)?,
        rule_no_upward_refs(
            root,
            "apps/ene-host/src/host/db",
            &Regex::new(r"crate::host::(client|model|permission|providers|work)\b")?,
            "db_upward_imports",
            BASELINES.db_upward_refs,
            "the store layer must not reach up into the host modules.",
        )?,
        rule_no_upward_refs(
            root,
            "apps/ene-host/src/host/providers",
            &Regex::new(r"\b(?:crate::host|super)::model\b")?,
            "providers_no_model_import",
            BASELINES.providers_model_refs,
            "provider adapters must not depend on the model layer.",
        )?,
        rule_host_world_refs(root, &BASELINES)?,
        rule_impl_file_spread(root, &BASELINES)?,
        rule_max_prod_file_lines(root, &BASELINES)?,
        rule_rpc_no_ecs_resource(root, &BASELINES)?,
        rule_prod_block_on(root, &BASELINES)?,
    ])
}

#[test]
fn the_workspace_holds_its_architecture_ratchets() -> Res<()> {
    let root = workspace_root()?;
    let measurements = measure_all(&root)?;

    let mut summary = String::new();
    let mut failures = Vec::new();
    for measurement in &measurements {
        summary.push_str(&format!(
            "  {:<28} {:>4} / {}\n",
            measurement.rule, measurement.count, measurement.max
        ));
        failures.extend(measurement.verdict());
    }
    println!("architecture ratchets ({}):\n{summary}", root.display());

    assert!(
        failures.is_empty(),
        "{} architecture rule(s) regressed:\n{}\n\nFull measurements:\n{summary}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn stripper_ignores_test_items_and_keeps_production_lines() -> Res<()> {
    let sample = concat!(
        "fn before() {}\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    // { stray brace in a comment\n",
        "    fn it() { let _ = \"}\"; }\n",
        "}\n",
        "fn after() {}\n",
        "#[cfg(any(test, feature = \"x\"))]\n",
        "fn also_test() {}\n",
    );
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("sample.rs");
    std::fs::write(&path, sample)?;
    let stripped = production_source(&path)?.text;
    let kept: Vec<&str> = stripped
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        kept,
        vec!["fn before() {}", "fn after() {}"],
        "only non-test production lines may survive the strip"
    );
    Ok(())
}

#[test]
fn stripper_preserves_line_numbers() -> Res<()> {
    let sample = "#[cfg(test)]\nmod tests {\n    fn it() {}\n}\nfn after() {}\n";
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("sample.rs");
    std::fs::write(&path, sample)?;
    let stripped = production_source(&path)?.text;
    assert_eq!(
        stripped.lines().count(),
        sample.lines().count(),
        "blanking must not shift the line numbers the rules report"
    );
    let after_line = stripped
        .lines()
        .position(|line| line == "fn after() {}")
        .map(|index| index + 1);
    assert_eq!(after_line, Some(5), "`fn after` must stay on line 5");
    Ok(())
}
