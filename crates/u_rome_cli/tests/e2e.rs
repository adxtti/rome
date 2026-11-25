//! End-to-end: build a synthetic install, run the real `rome` binary on it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rome(index: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rome"))
        .arg("--index")
        .arg(index)
        .args(args)
        .output()
        .expect("run rome");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "rome {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("u_rome_cli_e2e_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn scan_search_extract() {
    let dir = temp("main");
    let game = dir.join("game");
    std::fs::create_dir_all(game.join("x64")).unwrap();
    std::fs::write(
        game.join("x64/sample.rpf"),
        u_rome_testdata::Rpf9Builder::sample(11).build().bytes,
    )
    .unwrap();
    let index = dir.join("index.sqlite");

    let scan = rome(&index, &["scan", game.to_str().unwrap()]);
    assert!(
        scan.contains("scanned 1 / skipped 0 / failed 0 of 1 files"),
        "{scan}"
    );

    let stats = rome(&index, &["stats"]);
    assert!(stats.contains("archives 1"), "{stats}");

    let archives = rome(&index, &["archives"]);
    assert!(archives.contains("x64/sample.rpf"), "{archives}");

    let ls = rome(&index, &["ls", "x64/sample.rpf"]);
    let first_id: i64 = ls
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("an entry id");

    let info = rome(&index, &["info", &first_id.to_string()]);
    assert!(info.contains("location"), "{info}");

    // Every listed entry extracts raw without error.
    let out = dir.join("out");
    for line in ls.lines() {
        let id = line.split_whitespace().next().unwrap();
        let written = rome(
            &index,
            &["extract", id, "--raw", "-o", out.to_str().unwrap()],
        );
        assert!(written.trim().ends_with(".bin"), "{written}");
    }

    // XML entries are searchable by kind.
    let xml = rome(&index, &["search", "--kind", "xml"]);
    assert!(xml.contains("XML"), "{xml}");

    let _ = std::fs::remove_dir_all(&dir);
}
