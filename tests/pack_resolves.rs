//! Every shipped pack must produce a config that resolves (`BUG-020`).
//!
//! `ctxgrd pack add <name>` writes a namespace block into `ctxgrd.toml`.
//! If that block binds a parameterised rule without its params sub-table,
//! the *next* command — any command — dies at config load with exit 2.
//! The pack is then not merely unhelpful: it bricks the repo it was added
//! to, and the only way out is to hand-edit the file it just generated.
//!
//! `checklist` shipped in that state from `0.49.0`. It bound
//! `core.required-headings` while leaving the `[CHECKLIST."core.required-headings"]`
//! block commented out, on the reasoning that a bare checklist requires no
//! particular sections — true, and expressed as an empty list rather than
//! an absent table.
//!
//! This walks every pack rather than pinning the one that broke: the defect
//! is a property of the generate-then-load round trip, and any pack can
//! regress into it.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// Pack directories that `ctxgrd pack add` deliberately cannot install,
/// each with the reason. Pinned rather than filtered silently: a new pack
/// directory that nobody wired into `src/pack.rs`'s `include_str!` set is
/// invisible to every user who has only the binary, and this test is the
/// natural place to notice.
const NOT_INSTALLABLE: &[(&str, &str)] = &[
    (
        "arc42",
        "paid tier — advertised by `pack list --paid`, never compiled in (ADR-045)",
    ),
    (
        "port",
        "on disk but absent from src/pack.rs's include_str! set, so it resolves \
         only from a checkout of this repo and `pack add port` fails for anyone \
         holding just the binary",
    ),
];

/// Every pack under `packs/`, by directory name.
fn pack_names() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("packs");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("packs/ is readable")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("pack.toml").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn ctxgrd(root: &PathBuf, args: &[&str]) -> std::process::Output {
    let mut argv: Vec<&str> = args.to_vec();
    argv.extend_from_slice(&["--root", root.to_str().unwrap()]);
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .env("HOME", root)
        .args(&argv)
        .output()
        .expect("ctxgrd runs")
}

#[test]
fn every_shipped_pack_produces_a_config_that_resolves() {
    let names = pack_names();
    assert!(
        names.len() >= 33,
        "expected the full shipped pack set, found {}: {names:?}",
        names.len()
    );

    let mut broken: Vec<String> = Vec::new();
    let mut uninstallable: Vec<String> = Vec::new();
    for name in &names {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let init = ctxgrd(&root, &["init"]);
        assert!(
            init.status.success(),
            "`ctxgrd init` failed before adding {name}:\n{}",
            String::from_utf8_lossy(&init.stderr)
        );

        // `HOME` and `--root` both point at the temp dir, so neither
        // `~/.ctxgrd/packs` nor this repo's `./packs` is visible: what
        // resolves here is exactly what a user with only the binary has.
        let add = ctxgrd(&root, &["pack", "add", name]);
        if !add.status.success() {
            uninstallable.push(name.clone());
            continue;
        }

        // Any command would do; `rules` is the one whose whole job is
        // reporting the resolved config, so a config error here is maximally
        // on the nose.
        let out = ctxgrd(&root, &["rules"]);
        if out.status.code() == Some(2) {
            broken.push(format!(
                "{name}: config does not resolve after `pack add` — {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    assert_eq!(
        broken,
        Vec::<String>::new(),
        "packs that brick the repo they are added to:\n  {}",
        broken.join("\n  ")
    );

    let expected: Vec<String> = NOT_INSTALLABLE.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(
        uninstallable,
        expected,
        "the set of pack directories `pack add` cannot install changed. \
         Known and why:\n  {}\n\
         A new entry usually means a pack.toml was added without an \
         `include_str!` line in src/pack.rs, which makes it invisible to \
         everyone who has only the binary.",
        NOT_INSTALLABLE
            .iter()
            .map(|(n, why)| format!("{n} — {why}"))
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
