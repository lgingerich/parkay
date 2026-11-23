//! `cargo xtask asm`: show a function's assembly, or save it and diff a later build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Lock, cargo, lock, parse_name, rel_path, run, xtask_dir};

/// Lines printed for plain `cargo xtask asm SYMBOL` before truncating (use `--full` for all).
const SHOW_LINES_MAX: usize = 120;

/// Lines of `diff -u` printed per changed function. The full diff is a command away.
const DIFF_LINES_MAX: usize = 60;

/// Functions listed in a diff summary before the rest are counted.
const CHANGED_FUNCTIONS_MAX: usize = 25;

/// Maximum changed functions to show inline `diff -u` hunks for in a whole-crate diff
/// (when a `symbol` filter is given, all changed functions are shown).
const INLINE_DIFF_FUNCTIONS_MAX: usize = 4;

/// Scratch files `--diff` rewrites each run, beside the saved `NAME.s` files.
const DIFF_BEFORE: &str = "diff.before.s";
const DIFF_AFTER: &str = "diff.after.s";
const CURRENT_SUFFIX: &str = ".current.s";

/// True for a scratch file in `target/xtask/asm/`, which `clean` removes.
pub(crate) fn is_asm_scratch(filename: &str) -> bool {
    filename == DIFF_BEFORE || filename == DIFF_AFTER || filename.ends_with(CURRENT_SUFFIX)
}

/// The `--save` name of `target/xtask/asm/FILENAME`, or `None` for scratch files.
pub(crate) fn saved_asm_name(filename: &str) -> Option<&str> {
    if is_asm_scratch(filename) {
        return None;
    }
    filename.strip_suffix(".s").filter(|name| !name.is_empty())
}

/// Where `--diff NAME` writes the current build's assembly for a full `diff -u`.
pub(crate) fn current_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{CURRENT_SUFFIX}"))
}

#[derive(clap::Args)]
pub struct Args {
    /// Cargo-show-asm symbol path, such as `<parkay::thrift::CompactReader>::skip_nested`.
    /// Omit to list candidates (or, with --save/--diff, inspect the whole library).
    pub symbol: Option<String>,
    /// Interleave Rust source with the assembly.
    #[arg(long, conflicts_with_all = ["save", "diff"])]
    pub rust: bool,
    /// Print full raw `cargo-show-asm` output without normalizing or truncating.
    #[arg(long, conflicts_with_all = ["save", "diff"])]
    pub full: bool,
    /// Save normalized assembly as NAME.
    #[arg(long, value_name = "NAME", value_parser = parse_name, conflicts_with = "diff")]
    pub save: Option<String>,
    /// Compare current build's assembly against saved NAME.
    #[arg(long, value_name = "NAME", value_parser = parse_name)]
    pub diff: Option<String>,
}

pub fn asm(args: &Args) -> Result<(), String> {
    let symbol = args.symbol.as_deref();
    let _lock = lock(Lock::Build, "asm")?;

    match (args.save.as_deref(), args.diff.as_deref()) {
        (None, None) => show(symbol, args.rust, args.full),
        (Some(name), None) => {
            let current = normalize(&assembly(symbol)?);
            let path = xtask_dir("asm")?.join(format!("{name}.s"));
            std::fs::write(&path, &current).map_err(|e| format!("{}: {e}", rel_path(&path)))?;
            let funcs = functions(&current);
            let (instructions, branches) = totals(&funcs);
            println!(
                "saved {} functions, {instructions} instructions, {branches} branches: {}",
                funcs.len(),
                rel_path(&path)
            );
            Ok(())
        }
        (None, Some(name)) => {
            let current = normalize(&assembly(symbol)?);
            let dir = xtask_dir("asm")?;
            let path = dir.join(format!("{name}.s"));
            let saved = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "{}: {e} (save it first with --save {name})",
                    rel_path(&path)
                )
            })?;
            let current_path = current_path(&dir, name);
            std::fs::write(&current_path, &current)
                .map_err(|e| format!("{}: {e}", rel_path(&current_path)))?;
            compare(&saved, &current, symbol, &dir);
            println!(
                "full diff: diff -u {} {}",
                rel_path(&path),
                rel_path(&current_path)
            );
            Ok(())
        }
        (Some(_), Some(_)) => Err("use --save or --diff, not both".into()),
    }
}

fn show(symbol: Option<&str>, rust: bool, full: bool) -> Result<(), String> {
    let Some(symbol) = symbol else {
        return run(&mut cargo_asm());
    };
    if rust || full {
        let mut command = cargo_asm();
        if rust {
            command.arg("--rust");
        }
        command.arg(symbol);
        return run(&mut command);
    }
    let normalized = normalize(&assembly(Some(symbol))?);
    let funcs = functions(&normalized);
    let (instructions, branches) = totals(&funcs);
    println!(
        "{} function(s), {instructions} instructions, {branches} branches:",
        funcs.len()
    );
    let (body, omitted) = cap_lines(&normalized, SHOW_LINES_MAX);
    print!("{body}");
    if omitted > 0 {
        println!("... {omitted} more lines (pass --full to print all)");
    }
    Ok(())
}

fn cap_lines(text: &str, max_lines: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let omitted = lines.len().saturating_sub(max_lines);
    let mut out = lines[..lines.len().min(max_lines)].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, omitted)
}

fn cargo_asm() -> Command {
    let mut command = cargo();
    command.args([
        "asm",
        "--locked",
        "--lib",
        "-p",
        "parkay",
        "--features",
        "internals",
        "--profile",
        "release-profiling",
    ]);
    command
}

/// One function's assembly, or with no symbol the whole library's.
fn assembly(symbol: Option<&str>) -> Result<String, String> {
    let mut command = cargo_asm();
    match symbol {
        Some(symbol) => command.arg(symbol),
        None => command.arg("--everything"),
    };
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run cargo asm: {e}"))?;
    if !out.status.success() {
        // An ambiguous SYMBOL lists its candidates on stdout.
        return Err(format!(
            "cargo asm failed:\n{}\n{}",
            crate::tail(&String::from_utf8_lossy(&out.stderr), 5),
            crate::tail(&String::from_utf8_lossy(&out.stdout), 15)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Strips assembly comments (`;` or `//`), ignoring `;` inside `<...>` or `[...]` so
/// demangled Rust array types such as `<[u8; 16]>` are kept intact.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut angle = 0_u32;
    let mut square = 0_u32;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'[' => square += 1,
            b']' => square = square.saturating_sub(1),
            b';' if angle == 0 && square == 0 => return line[..index].trim(),
            b'/' if bytes.get(index + 1) == Some(&b'/') => return line[..index].trim(),
            _ => {}
        }
        index += 1;
    }
    line.trim()
}

/// Assembly with what changes between identical builds taken out: directives,
/// comments, and the numbering of local labels, which are renumbered per function in
/// order of first use. Two builds of the same code normalize to the same text, and a
/// branch change in one function does not renumber labels in unrelated functions.
fn normalize(asm: &str) -> String {
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    let mut out = String::new();
    let mut debug_info = false;
    for line in asm.lines() {
        // Debug info sections describe the code; their churn is not a code change.
        let directive = line.trim_start();
        if directive.starts_with(".section") || directive.starts_with(".text") {
            debug_info = directive.contains("DWARF") || directive.contains(".debug");
        }
        if debug_info {
            continue;
        }
        let line = strip_comment(line);
        let is_directive = line.starts_with('.') && !line.starts_with(".L");
        if line.is_empty() || is_directive || line.starts_with("Lloh") {
            continue;
        }
        if let Some(label) = line.strip_suffix(':')
            && is_function_label(label)
        {
            labels.clear();
            out.push_str(line);
            out.push('\n');
            continue;
        }
        renumber_local_labels(line, &mut labels, &mut out);
        out.push('\n');
    }
    out
}

/// Appends `line` to `out` with assembler-local labels replaced by `L0`, `L1`, ...
fn renumber_local_labels(line: &str, labels: &mut BTreeMap<String, String>, out: &mut String) {
    let mut token = String::new();
    for c in line.chars().chain(std::iter::once('\n')) {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.') {
            token.push(c);
            continue;
        }
        if is_local_label(&token) {
            let next = format!("L{}", labels.len());
            out.push_str(labels.entry(token.clone()).or_insert(next));
        } else {
            out.push_str(&token);
        }
        token.clear();
        if c != '\n' {
            out.push(c);
        }
    }
}

/// Assembler-local labels (`LBB3_7`, `Ltmp12`, `ltmp0`, `.LBB0_1`, `LCPI0_0`,
/// `lCPI0_0`, `l_str`), whose numbers or names shift when unrelated code changes.
/// Prefix-checked so Rust identifiers like `Lz4` or `Load64` are not rewritten.
fn is_local_label(token: &str) -> bool {
    const NUMBERED_PREFIXES: &[&str] = &[
        ".L", "LBB", "Ltmp", "Lfunc_", "LPC", "LCPI", "lCPI", "LJTI", "Lstr", "Lswitch", "ltmp",
    ];
    if token
        .strip_prefix("l_")
        .is_some_and(|rest| !rest.is_empty())
    {
        return true;
    }
    NUMBERED_PREFIXES.iter().any(|prefix| {
        token
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.ends_with(|c: char| c.is_ascii_digit()))
    })
}

/// Labels that are not functions: local labels, data, and exception tables.
fn is_function_label(label: &str) -> bool {
    let is_normalized_local = label
        .strip_prefix('L')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
    !(is_local_label(label)
        || is_normalized_local
        || label.starts_with(".L")
        || label.starts_with("l_")
        || label.starts_with("GCC_except_table"))
}

/// The normalized text of each function, by name. A function starts at a label that
/// is not local.
fn functions(asm: &str) -> BTreeMap<String, String> {
    let mut functions = BTreeMap::new();
    let mut name = String::from("(preamble)");
    let mut body = String::new();
    for line in asm.lines() {
        if let Some(label) = line.strip_suffix(':')
            && is_function_label(label)
        {
            if !body.is_empty() {
                functions.insert(std::mem::take(&mut name), std::mem::take(&mut body));
            }
            label.clone_into(&mut name);
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if !body.is_empty() {
        functions.insert(name, body);
    }
    functions
}

/// True for `AArch64` (`b`, `b.*`, `cbz`, `cbnz`, `tbz`, `tbnz`, `br`) and `x86-64` (`j*`)
/// branch mnemonics, excluding call instructions (`bl`, `blr`, `call`, `callq`).
fn is_branch(mnemonic: &str) -> bool {
    matches!(mnemonic, "b" | "cbz" | "cbnz" | "tbz" | "tbnz" | "br")
        || mnemonic.starts_with("b.")
        || mnemonic.starts_with('j')
}

/// Instruction counts that track the costs a change usually moves.
#[derive(Debug, PartialEq, Eq)]
struct Counts {
    instructions: usize,
    branches: usize,
    calls: usize,
    loads: usize,
    stores: usize,
}

impl Counts {
    /// Simplification: loads and stores are counted by `AArch64` mnemonic (`ld*`, `st*`),
    /// the machine parkay is measured on. On x86-64 they read 0 and instructions,
    /// branches, and calls are counted; count memory operands to extend loads/stores.
    fn of(body: &str) -> Self {
        let mut counts = Self {
            instructions: 0,
            branches: 0,
            calls: 0,
            loads: 0,
            stores: 0,
        };
        for line in body.lines().filter(|l| !l.ends_with(':')) {
            let mnemonic = line.split_whitespace().next().unwrap_or("");
            counts.instructions += 1;
            counts.branches += usize::from(is_branch(mnemonic));
            counts.calls += usize::from(matches!(mnemonic, "bl" | "blr" | "call" | "callq"));
            counts.loads += usize::from(mnemonic.starts_with("ld"));
            counts.stores += usize::from(mnemonic.starts_with("st"));
        }
        counts
    }

    fn describe(&self) -> String {
        format!(
            "{} instr, {} branches, {} calls, {} loads, {} stores",
            self.instructions, self.branches, self.calls, self.loads, self.stores
        )
    }
}

/// `(instructions, branches)` summed over every function, for one-line summaries.
fn totals(functions: &BTreeMap<String, String>) -> (usize, usize) {
    functions.values().map(|body| Counts::of(body)).fold(
        (0, 0),
        |(instructions, branches), counts| {
            (
                instructions + counts.instructions,
                branches + counts.branches,
            )
        },
    )
}

/// True if `func_name` matches the CLI `symbol` filter passed to `cargo xtask asm SYMBOL`.
fn symbol_matches(func_name: &str, symbol: &str) -> bool {
    if func_name.contains(symbol) {
        return true;
    }
    // `cargo-show-asm` accepts `<Type>::method`, while some labels omit the brackets.
    let stripped: String = symbol
        .chars()
        .filter(|&c| !matches!(c, '<' | '>'))
        .collect();
    !stripped.is_empty() && func_name.contains(&stripped)
}

struct FunctionDiff {
    name: String,
    before: Option<String>,
    after: Option<String>,
}

struct DiffPlan {
    compared_total: usize,
    after_count: usize,
    after_instructions: usize,
    after_branches: usize,
    changed: Vec<FunctionDiff>,
    show_inline_diffs: bool,
}

fn plan_diff(saved: &str, current: &str, symbol: Option<&str>) -> DiffPlan {
    let mut before = functions(saved);
    let after = functions(current);
    // When diffing a single `symbol` against a whole-crate save, restrict `before` to
    // functions present in `current` or matching `symbol` so unrelated functions are not
    // falsely reported as `after: absent`.
    if let Some(sym) = symbol {
        before.retain(|name, _| after.contains_key(name) || symbol_matches(name, sym));
    }
    let mut changed = Vec::new();
    for (name, body) in &after {
        match before.get(name) {
            Some(old) if old == body => {}
            Some(old) => changed.push(FunctionDiff {
                name: name.clone(),
                before: Some(old.clone()),
                after: Some(body.clone()),
            }),
            None => changed.push(FunctionDiff {
                name: name.clone(),
                before: None,
                after: Some(body.clone()),
            }),
        }
    }
    for (name, old) in &before {
        if !after.contains_key(name) {
            changed.push(FunctionDiff {
                name: name.clone(),
                before: Some(old.clone()),
                after: None,
            });
        }
    }
    let (after_instructions, after_branches) = totals(&after);
    let show_inline_diffs = symbol.is_some() || changed.len() <= INLINE_DIFF_FUNCTIONS_MAX;
    DiffPlan {
        compared_total: before.len().max(after.len()),
        after_count: after.len(),
        after_instructions,
        after_branches,
        changed,
        show_inline_diffs,
    }
}

/// Prints which functions changed between `saved` and `current`, with counts, and
/// the start of the diff of each when appropriate.
fn compare(saved: &str, current: &str, symbol: Option<&str>, dir: &Path) {
    let plan = plan_diff(saved, current, symbol);
    if plan.changed.is_empty() {
        println!(
            "identical: {} functions, {} instructions, {} branches",
            plan.after_count, plan.after_instructions, plan.after_branches
        );
        return;
    }
    println!(
        "{} of {} functions differ:",
        plan.changed.len(),
        plan.compared_total
    );
    for entry in plan.changed.iter().take(CHANGED_FUNCTIONS_MAX) {
        let describe = |body: Option<&String>| {
            body.map_or_else(|| "absent".to_owned(), |b| Counts::of(b).describe())
        };
        println!(
            "  {}\n    before: {}\n    after:  {}",
            entry.name,
            describe(entry.before.as_ref()),
            describe(entry.after.as_ref())
        );
    }
    if plan.changed.len() > CHANGED_FUNCTIONS_MAX {
        println!("  ... {} more", plan.changed.len() - CHANGED_FUNCTIONS_MAX);
    }
    if plan.show_inline_diffs {
        let (a, b) = (dir.join(DIFF_BEFORE), dir.join(DIFF_AFTER));
        for entry in &plan.changed {
            let (Some(old), Some(new)) = (entry.before.as_ref(), entry.after.as_ref()) else {
                continue;
            };
            if std::fs::write(&a, old).is_ok() && std::fs::write(&b, new).is_ok() {
                // `diff` exits 1 when the files differ, so its status is not checked.
                let diff = Command::new("diff")
                    .arg("-u")
                    .arg(&a)
                    .arg(&b)
                    .output()
                    .map_or_default(|o| String::from_utf8_lossy(&o.stdout).into_owned());
                println!("diff of {}:", entry.name);
                let body: Vec<&str> = diff.lines().skip(2).collect();
                for line in body.iter().take(DIFF_LINES_MAX) {
                    println!("  {line}");
                }
                if body.len() > DIFF_LINES_MAX {
                    println!("  ... {} more diff lines", body.len() - DIFF_LINES_MAX);
                }
            }
        }
        drop(std::fs::remove_file(&a));
        drop(std::fs::remove_file(&b));
    }
}

#[cfg(test)]
mod tests {
    use super::{Counts, cap_lines, functions, is_local_label, normalize, plan_diff};

    #[test]
    fn cap_lines_truncates_and_counts_omitted_lines() {
        let (kept, omitted) = cap_lines("a\nb\nc\nd\n", 2);
        assert_eq!(kept, "a\nb\n");
        assert_eq!(omitted, 2);

        let (all, none) = cap_lines("a\nb\n", 5);
        assert_eq!(all, "a\nb\n");
        assert_eq!(none, 0);
    }

    #[test]
    fn label_renumbering_is_isolated_per_function() {
        let before = "\
parkay::first:
    cbz x0, LBB0_1
LBB0_1:
    ret
parkay::second:
    cbz x0, LBB1_1
LBB1_1:
    ret
";
        let after = "\
parkay::first:
    cbz x0, LBB0_1
    cbnz x1, LBB0_2
LBB0_1:
LBB0_2:
    ret
parkay::second:
    cbz x0, LBB9_4
LBB9_4:
    ret
";
        let before_fns = functions(&normalize(before));
        let after_fns = functions(&normalize(after));
        assert_ne!(before_fns["parkay::first"], after_fns["parkay::first"]);
        assert_eq!(before_fns["parkay::second"], after_fns["parkay::second"]);
    }

    #[test]
    fn demangled_array_semicolon_is_preserved_while_comments_are_stripped() {
        let asm = "\
<[u8; 16] as parkay::Codec>::decode:
    ldr x0, [x1, #8] ; load header
    str x0, [x2] // store result
    bl parkay::Lz4::decompress
    ret
";
        let normalized = normalize(asm);
        let funcs = functions(&normalized);
        let Some(body) = funcs.get("<[u8; 16] as parkay::Codec>::decode") else {
            panic!("array type function label preserved");
        };
        assert!(body.contains("bl parkay::Lz4::decompress"));
        assert!(!body.contains("load header"));
        assert!(!body.contains("store result"));
        let counts = Counts::of(body);
        assert_eq!(
            counts,
            Counts {
                instructions: 4,
                branches: 0,
                calls: 1,
                loads: 1,
                stores: 1,
            }
        );
    }

    #[test]
    fn counts_arm64_and_x86_branches_excluding_calls() {
        let arm64 = "\
    b L0
    b.eq L1
    cbz x0, L0
    cbnz x1, L1
    tbz x2, #0, L0
    tbnz x3, #1, L1
    br x4
    bl parkay::callee
    blr x5
    bic x0, x1, x2
L0:
    ret
";
        let counts = Counts::of(arm64);
        assert_eq!(counts.instructions, 11);
        assert_eq!(counts.branches, 7);
        assert_eq!(counts.calls, 2);

        let x86 = "\
    je L0
    jne L1
    jmp L2
    callq parkay::callee
    retq
";
        let x86_counts = Counts::of(x86);
        assert_eq!(x86_counts.instructions, 5);
        assert_eq!(x86_counts.branches, 3);
        assert_eq!(x86_counts.calls, 1);
    }

    #[test]
    fn local_label_predicate_recognizes_macho_labels_and_avoids_rust_identifiers() {
        for local in [
            "LBB3_7",
            "Ltmp12",
            "ltmp0",
            ".LBB0_1",
            "LCPI0_0",
            "lCPI0_0",
            "l_str",
            "l_anon.1234.0",
        ] {
            assert!(is_local_label(local), "{local} should be a local label");
        }
        for ident in ["Lz4", "Load64", "Layout16", "L", "l_"] {
            assert!(!is_local_label(ident), "{ident} is not a local label");
        }
    }

    #[test]
    fn symbol_filtered_diff_against_whole_crate_save_avoids_false_deletions() {
        let whole_crate_saved = normalize(
            "\
parkay::unrelated_a:
    ret
parkay::unrelated_b:
    ret
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Unmarked>:
    cbz x0, LBB0_1
LBB0_1:
    ret
",
        );
        let single_symbol_current = normalize(
            "\
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Unmarked>:
    cbnz x0, LBB0_1
LBB0_1:
    ret
",
        );
        let plan = plan_diff(
            &whole_crate_saved,
            &single_symbol_current,
            Some("skip_nested"),
        );
        assert_eq!(plan.compared_total, 1);
        assert_eq!(plan.changed.len(), 1);
        assert!(plan.changed[0].name.contains("skip_nested"));
        assert!(plan.changed[0].before.is_some());
        assert!(plan.changed[0].after.is_some());
        assert!(plan.show_inline_diffs);
    }

    #[test]
    fn multi_monomorphization_edit_shows_inline_diffs_for_both() {
        let before = normalize(
            "\
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Unmarked>:
    cbz x0, LBB0_1
LBB0_1:
    ret
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Marking>:
    cbz x0, LBB1_1
LBB1_1:
    ret
",
        );
        let after = normalize(
            "\
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Unmarked>:
    cbnz x0, LBB0_1
LBB0_1:
    ret
parkay::thrift::CompactReader::skip_nested::<parkay::thrift::Marking>:
    cbnz x0, LBB1_1
LBB1_1:
    ret
",
        );
        // Even in a whole-crate diff (symbol = None), 2 changed monomorphizations <= 4
        // so both diffs are shown inline.
        let plan = plan_diff(&before, &after, None);
        assert_eq!(plan.changed.len(), 2);
        assert!(plan.show_inline_diffs);
        assert!(
            plan.changed
                .iter()
                .all(|c| c.before.is_some() && c.after.is_some())
        );
    }
}
