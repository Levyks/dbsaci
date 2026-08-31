//! Golden tests for `pgsaci::translate::oracle_to_postgres` — pure string→string
//! SQL translation, no backend. Cases live in `tests/corpus/translate/*.txt`,
//! one per line as `oracle SQL  =>  expected PostgreSQL`, or `oracle SQL  =>  !`
//! to assert the translator rejects it.
//!
//! `#[test] fn translate_goldens` walks every file so a bad line names its file
//! and line number.

use std::path::Path;

use pgsaci::translate::oracle_to_postgres;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/translate");

#[test]
fn translate_goldens() {
    let mut files: Vec<_> = std::fs::read_dir(DIR)
        .unwrap_or_else(|e| panic!("read {DIR}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no translate goldens in {DIR}");

    let mut failures = Vec::new();
    let mut count = 0usize;

    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((input, expected)) = line.split_once("=>") else {
                failures.push(format!(
                    "{}:{}: no `=>` separator",
                    file_name(path),
                    lineno + 1
                ));
                continue;
            };
            let input = input.trim();
            let expected = expected.trim();
            count += 1;

            let got = oracle_to_postgres(input);
            match (expected, got) {
                ("!", Ok(out)) => failures.push(format!(
                    "{}:{}: expected rejection, translated to `{out}`\n    input: {input}",
                    file_name(path),
                    lineno + 1
                )),
                ("!", Err(_)) => {}
                (_, Err(e)) => failures.push(format!(
                    "{}:{}: translation failed: {e}\n    input:    {input}\n    expected: {expected}",
                    file_name(path),
                    lineno + 1
                )),
                (_, Ok(out)) if out != expected => failures.push(format!(
                    "{}:{}: mismatch\n    input:    {input}\n    expected: {expected}\n    actual:   {out}",
                    file_name(path),
                    lineno + 1
                )),
                (_, Ok(_)) => {}
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "{}/{} translate goldens failed:\n\n{}",
            failures.len(),
            count,
            failures.join("\n\n")
        );
    }
    eprintln!(
        "translate goldens: {count} cases across {} files",
        files.len()
    );
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().to_string()
}
