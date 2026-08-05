//! BUG-048: config discovery walks up, so running ctxgrd from a
//! subdirectory of a governed repo lints the repo instead of silently
//! degrading to zero-config and reporting `ok`.
//!
//! Asserted as a **pair** (BUG-048 § Fix, ADR-112 § CLR-007). A test that
//! a subdirectory run finds the root config proves little on its own — a
//! change that always walked to the filesystem root would pass it too.
//! What pins the behaviour is asserting alongside that an explicit
//! `--root` does *not* search, that a genuinely config-less tree still
//! lints zero-config, and that a nested config wins over its parent.
//!
//! These tests run the binary with `current_dir` set, because the
//! defaulting is what is under test: the absence of `--root` is what
//! licenses the upward search, and any harness that passes `--root`
//! would test the opposite path.

use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::cargo::cargo_bin;

/// A governed repo: config at the root, one ADR, one deliberately
/// broken document that only the *configured* rule set catches.
///
/// `core.required-metadata` demanding `status` is the probe. Zero-config
/// does not include it, so a degraded run reports this tree clean while a
/// correctly-resolved run reports one error — that difference is the bug.
fn governed_repo(root: &Path) {
    fs::create_dir_all(root.join("docs/adrs")).expect("mkdir");
    fs::write(
        root.join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nowner = \"developer\"\n\
         rules = [\"core.frontmatter\", \"core.id\", \"core.required-metadata\"]\n\
         \n[ADR.\"core.required-metadata\"]\nkeys = [\"id\", \"title\", \"status\"]\n",
    )
    .expect("write config");
    fs::write(
        root.join("docs/adrs/001-missing-status.md"),
        "---\nid: ADR-001\ntitle: No status key\n---\n\n# No status key\n",
    )
    .expect("write ADR-001");
}

struct Run {
    code: Option<i32>,
    /// Both streams. ctxgrd splits them deliberately — diagnostics and
    /// the `found:` tally on stdout, the `ok:` trailer and hints on
    /// stderr — but which stream carries which is a separate contract
    /// with its own tests. Joining them here keeps these tests about
    /// config *discovery* and stops a routing change from failing them
    /// for the wrong reason.
    out: String,
}

fn run_in(dir: &Path, home: &Path, args: &[&str]) -> Run {
    let out = StdCommand::new(cargo_bin("ctxgrd"))
        .current_dir(dir)
        // Isolate from the developer's real ~/.ctxgrd — a global config
        // would supply namespaces and mask the zero-config path.
        .env("HOME", home)
        .args(args)
        .output()
        .expect("ctxgrd runs");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf-8");
    Run {
        code: out.status.code(),
        out: format!("{stdout}{stderr}"),
    }
}

#[test]
fn subdirectory_run_resolves_the_repo_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    governed_repo(root);

    let at_root = run_in(root, root, &[]);
    let from_sub = run_in(&root.join("docs/adrs"), root, &[]);

    assert_eq!(
        at_root.code,
        Some(1),
        "the configured rule set catches the missing `status`; output:\n{}",
        at_root.out
    );
    // The defect, stated as an assertion: this used to exit 0 and print
    // `ok`, because the subdirectory had no ctxgrd.toml of its own.
    assert_eq!(
        from_sub.code, at_root.code,
        "a subdirectory run must reach the same verdict as a root run; output:\n{}",
        from_sub.out
    );
    assert!(
        from_sub.out.contains("core.required-metadata"),
        "the subdirectory run must apply the *configured* rules, not the \
         zero-config six; output:\n{}",
        from_sub.out
    );
    assert!(
        !from_sub.out.contains("cfg.zero-config"),
        "a resolved config is not a zero-config run; output:\n{}",
        from_sub.out
    );
}

#[test]
fn explicit_root_does_not_search_upward() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    governed_repo(root);
    let sub = root.join("docs/adrs");

    // `--root <dir>` means exactly that directory. Without this half of
    // the pair, a fix that always walked to the filesystem root would
    // pass `subdirectory_run_resolves_the_repo_config` and quietly make
    // `--root` unable to express "just this subtree".
    let explicit = run_in(root, root, &["--root", sub.to_str().unwrap()]);

    assert!(
        explicit.out.contains("cfg.zero-config"),
        "an explicit --root with no config of its own is a genuine \
         zero-config run and must say so; output:\n{}",
        explicit.out
    );
    assert!(
        !explicit.out.contains("core.required-metadata"),
        "an explicit --root must not silently inherit the parent's rules; \
         output:\n{}",
        explicit.out
    );
}

#[test]
fn nearest_config_wins_over_an_ancestor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    governed_repo(root);

    // A subtree carrying its own ctxgrd.toml is a separate lint root with
    // its own namespace DAG — not a fragment of the parent's.
    let nested = root.join("packages/inner");
    fs::create_dir_all(nested.join("docs/adrs")).expect("mkdir nested");
    fs::write(
        nested.join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nowner = \"developer\"\nrules = [\"core.frontmatter\", \"core.id\"]\n",
    )
    .expect("write nested config");
    fs::write(
        nested.join("docs/adrs/001-inner.md"),
        "---\nid: ADR-001\ntitle: Inner\n---\n\n# Inner\n",
    )
    .expect("write inner ADR");

    let inner = run_in(&nested.join("docs/adrs"), root, &[]);

    assert_eq!(
        inner.code,
        Some(0),
        "the nested config does not require `status`, so this tree is \
         clean — resolving the ancestor's config instead would fail it; \
         output:\n{}",
        inner.out
    );
    assert!(
        !inner.out.contains("core.required-metadata"),
        "nearest config wins; output:\n{}",
        inner.out
    );
    // Without this, the test passes vacuously against a build that does
    // not walk up at all: finding *no* config also avoids the ancestor's
    // rules. Asserting the run was not zero-config is what makes it
    // evidence that the nested config was actually resolved.
    assert!(
        !inner.out.contains("cfg.zero-config"),
        "the nested config must have been resolved, not skipped; output:\n{}",
        inner.out
    );
}

#[test]
fn config_less_tree_still_lints_zero_config_and_says_so() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir_all(root.join("docs")).expect("mkdir");
    fs::write(
        root.join("docs/001-a.md"),
        "---\nid: ADR-001\ntitle: A\n---\n\n# A\n",
    )
    .expect("write doc");

    // DOC-001 first-touch behaviour must survive the walk-up: an
    // id-claimed document in a repo with no config anywhere still lints,
    // still exits 0, and is not turned into an error by this fix.
    let run = run_in(root, root, &[]);

    assert_eq!(run.code, Some(0), "output:\n{}", run.out);
    assert!(
        run.out.contains("cfg.zero-config"),
        "…but it no longer passes silently; output:\n{}",
        run.out
    );
}

/// BUG-048 § Fix flagged the `--recursive` interaction as the part most
/// likely to surprise, because ctxgrd already searches *downward* and the
/// two directions must be defined against each other.
///
/// Resolved order: the upward search picks the root, then `-r` descends
/// from there. So `-r` inside a workspace member lints that member (its
/// own `ctxgrd.toml` is the nearest), while `-r` at the workspace root
/// lints every member — nearest-wins applies before the descent, not
/// after.
#[test]
fn recursive_descends_from_the_resolved_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    for pkg in ["a", "b"] {
        let dir = root.join("pkg").join(pkg);
        fs::create_dir_all(dir.join("docs/adrs")).expect("mkdir");
        fs::write(
            dir.join("ctxgrd.toml"),
            "[ADR]\npaths = [\"docs/adrs/**\"]\nowner = \"developer\"\nrules = [\"core.frontmatter\", \"core.id\"]\n",
        )
        .expect("write member config");
        fs::write(
            dir.join("docs/adrs/001-x.md"),
            "---\nid: ADR-001\ntitle: X\n---\n\n# X\n",
        )
        .expect("write member ADR");
    }

    // From inside member `a`: the nearest config is `a`'s own, so the
    // descent starts there and `b` is out of scope.
    let inside = run_in(&root.join("pkg/a/docs/adrs"), root, &["-r"]);
    assert_eq!(inside.code, Some(0), "output:\n{}", inside.out);
    assert!(
        !inside.out.contains("pkg/b"),
        "-r inside a member must not reach a sibling member; output:\n{}",
        inside.out
    );

    // From the workspace root, which carries no config of its own: the
    // upward search finds nothing, the cwd stands as the root, and the
    // descent reaches both members.
    let outside = run_in(root, root, &["-r"]);
    assert!(
        outside.out.contains("pkg/a") && outside.out.contains("pkg/b"),
        "-r at the workspace root must lint every member; output:\n{}",
        outside.out
    );
}

#[test]
fn init_does_not_walk_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    governed_repo(root);
    let before = fs::read_to_string(root.join("ctxgrd.toml")).expect("read config");
    let sub = root.join("docs/adrs");

    // `init` writes `<root>/ctxgrd.toml` and `--force` overwrites without
    // prompting. If it resolved its root upward like every reading
    // command, this invocation would silently destroy the repository's
    // real config. It must scaffold where the user is standing instead.
    let run = run_in(&sub, root, &["init", "--force"]);
    assert_eq!(run.code, Some(0), "output:\n{}", run.out);

    assert_eq!(
        fs::read_to_string(root.join("ctxgrd.toml")).expect("read config"),
        before,
        "`ctxgrd init --force` from a subdirectory must not overwrite the \
         parent's config"
    );
    assert!(
        sub.join("ctxgrd.toml").is_file(),
        "init scaffolds in the working directory"
    );
}
