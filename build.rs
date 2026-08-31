//! Embeds every shipped company bundle's `agents/` directory into the binary.
//!
//! The roster moved out of `company.toml` and into `agents/*.toml`. The on-disk
//! path followed it, but a `DesktopPreset` carries only an `include_str!`'d
//! `company.toml`, and `include_str!` cannot glob a directory — so the embedded
//! path silently lost every teammate, and first-run seeded an empty company.
//!
//! Generating the table here rather than hand-listing 135 files in a macro is
//! the point: a teammate added to a bundle is embedded because it exists, not
//! because someone remembered to add a line. A forgotten entry would not fail
//! the build, it would ship a company missing one agent.
//!
//! Both `*.toml` rosters and the documents `prompt_files` names are embedded,
//! because `agent_file::resolve_prompt_files` reads those bodies at parse time.
//!
//! The same script embeds the **global baseline** — `globals/` plus the shared
//! `skills/` library — for a second reason: a platform-provisioned container has
//! no repository checkout beside it, so anything only readable from disk is
//! simply absent there. A baseline that every company gets except the hosted
//! ones is not a baseline.
//!
//! It also stamps the **build commit** into `OPENCOMPANY_BUILD_COMMIT` — see
//! [`stamp_build_commit`], and `src/build_stamp.rs` for the choice of source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// The commit-stamp decision, shared verbatim with `cargo test` rather than
// duplicated. See the header of that file.
include!("src/build_stamp.rs");

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let companies = root.join("companies");

    stamp_build_commit(&root);
    embed_globals(&root);

    // Re-run when a bundle changes. Watching `companies/` alone is not enough:
    // cargo does not walk into it, so a new agent file inside an existing
    // bundle would not invalidate the build.
    println!("cargo:rerun-if-changed={}", companies.display());

    let mut bundles: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    let Ok(entries) = std::fs::read_dir(&companies) else {
        // No `companies/` at all is not this script's business to fail on:
        // the crate still compiles, and the desktop tests are what assert the
        // catalog is present.
        write_table(&bundles);
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        let bundle = entry.path();
        if !bundle.is_dir() {
            continue;
        }
        let Some(id) = bundle.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let agents_dir = bundle.join("agents");
        if !agents_dir.is_dir() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", agents_dir.display());

        let mut files = Vec::new();
        collect(&agents_dir, &agents_dir, &mut files);
        // Sorted by path, matching `agent_file::agent_file_paths`, because the
        // roster's order decides which teammate orchestrates when nobody is
        // tagged.
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if !files.is_empty() {
            bundles.insert(id.to_string(), files);
        }
    }

    write_table(&bundles);
}

/// Stamps the commit this binary is being built from into
/// `OPENCOMPANY_BUILD_COMMIT`, read back by `crate::BUILD_COMMIT`.
///
/// `CARGO_PKG_VERSION` has been `0.1.0` for thousands of commits, so a bug
/// report, a support conversation or an analytics event carrying only a
/// version cannot distinguish two builds a week apart. This closes that gap
/// and nothing more: it is one revision id, not a build-metadata surface.
///
/// **It must never fail a build.** Every source below is allowed to be absent,
/// and the worst outcome is the honest string `"unknown"` — which is why
/// [`resolve_build_commit`] takes options rather than unwrapping.
fn stamp_build_commit(root: &Path) {
    // Both environment sources are watched, not just read: an injected value
    // that changes must restamp, and cargo caches build-script output against
    // exactly the variables a script declares an interest in.
    println!("cargo:rerun-if-env-changed=OPENCOMPANY_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

    watch_git_refs(root);

    // The paths whose bytes become this binary, watched so the `-dirty` half
    // of the stamp is re-measured when they change. Without this the suffix
    // would report the tree as it stood the last time some *other* watched
    // path moved, which is a stamp that lies in the reassuring direction.
    //
    // `Cargo.lock` is here for the same reason as `Cargo.toml`, not as a
    // duplicate of it: a bare `cargo update` rewrites only the lockfile, and
    // cargo will happily recompile the dependency graph — and this binary
    // with it — while every other watched path is untouched. The stamp would
    // then keep a clean SHA on a tree the probe now calls dirty.
    //
    // Editing `docs/` or `frontend/` deliberately does not restamp: neither
    // changes this binary, so a stamp that stays clean across such an edit is
    // still telling the truth about the code that was compiled.
    watch_if_present(&root.join("src"));
    watch_if_present(&root.join("Cargo.toml"));
    watch_if_present(&root.join("Cargo.lock"));
    watch_submodule_heads(root);

    let commit = resolve_build_commit(
        std::env::var("OPENCOMPANY_BUILD_COMMIT").ok(),
        std::env::var("GITHUB_SHA").ok(),
        || git(root, &["rev-parse", "HEAD"]),
        || {
            // Tracked files against HEAD, plus a submodule pinned to a
            // different commit than the one recorded — that gitlink decides
            // which OpenHuman source is compiled in, so it belongs in the
            // answer. Deliberately *not* a full `git status`: recursing into
            // the working trees of eight nested vendored crates measured
            // 495ms against 27ms here, on a probe that runs on every
            // incremental build.
            //
            // So the boundary, stated rather than left to be rediscovered:
            // this measures **this repository's** tree at the commit being
            // stamped, and a submodule is part of that tree as a *gitlink*.
            // Uncommitted edits inside a vendored work tree are not — they
            // belong to another repository, at another commit, with a dirty
            // state of its own, and no stamp of one commit id can describe
            // two trees. Re-measured on 2026-08-26: `--ignore-submodules=`
            // `untracked` costs 347ms against `dirty`'s 36ms, and probing all
            // 18 vendored work trees individually costs 356ms. Nor could the
            // answer be kept fresh, since nothing watches those 11k files —
            // a probe that is right only on whichever build happened to rerun
            // for another reason is worse than one whose edge is written down.
            // The case is a deliberate local act in any event: CI, release and
            // desktop builds all compile a clean checkout, and the hosted
            // image is stamped from an injected `OPENCOMPANY_BUILD_COMMIT`.
            git(
                root,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=no",
                    "--ignore-submodules=dirty",
                ],
            )
            .is_some()
        },
    );

    println!("cargo:rustc-env=OPENCOMPANY_BUILD_COMMIT={commit}");
}

/// Watches the files git rewrites when the checked-out commit moves.
///
/// Not optional. This script already emits `rerun-if-changed`, which switches
/// off cargo's default "any file in the package" watch — so without an
/// explicit watch on the refs, `git commit` followed by `cargo build` would
/// leave the previous SHA embedded in the new binary. A stamp that is
/// confidently wrong is worse than no stamp at all.
///
/// Paths are resolved through `git rev-parse --git-path` rather than assumed
/// to sit under `.git/`: in a linked worktree `.git` is a *file*, `HEAD` lives
/// in that worktree's own directory, and the refs live in the shared one.
fn watch_git_refs(root: &Path) {
    // `HEAD` catches a checkout and every move of a detached head;
    // `refs/heads` catches switching to or creating a branch; `packed-refs`
    // catches a ref that lives there instead of in a loose file.
    for path in ["HEAD", "packed-refs", "refs/heads"] {
        if let Some(resolved) = git(root, &["rev-parse", "--git-path", path]) {
            watch_if_present(&root.join(resolved));
        }
    }
    // And the loose file behind the current branch, which is what an ordinary
    // `git commit` rewrites. Watched by name as well as through the directory
    // above, because a directory watch is the part of this that would be
    // quietly doing nothing if cargo ever stopped scanning recursively.
    if let Some(head_ref) = git(root, &["symbolic-ref", "--quiet", "HEAD"])
        && let Some(resolved) = git(root, &["rev-parse", "--git-path", &head_ref])
    {
        watch_if_present(&root.join(resolved));
    }
}

/// Watches each submodule's own `HEAD`, because moving one changes both the
/// source that compiles in and the answer the dirty probe gives.
///
/// `git status --ignore-submodules=dirty` still reports a submodule whose
/// checkout has left the commit the gitlink records — that is exactly the
/// state this catches, and nothing else in the watch set moves when it
/// happens: `git -C vendor/openhuman checkout <other>` rewrites no file under
/// `src/`, no manifest, and none of the superproject's refs.
///
/// Read from `.gitmodules` rather than hard-coded, so a submodule added later
/// is watched because it is declared. Nested submodules are deliberately not
/// recursed into: their gitlinks are recorded inside the parent's tree, so
/// `--ignore-submodules=dirty` never reports them, and watching what the probe
/// cannot see would only cost build time.
fn watch_submodule_heads(root: &Path) {
    let Some(declared) = git(
        root,
        &[
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    ) else {
        // No `.gitmodules`, no git, or no repository at all. Same contract as
        // everywhere else here: absence is a case, not a failure.
        return;
    };
    for line in declared.lines() {
        let Some((_, rel)) = line.split_once(' ') else {
            continue;
        };
        let rel = rel.trim();
        if rel.is_empty() {
            continue;
        }
        let dir = root.join(rel);
        // An uninitialized submodule is an empty directory with no `.git`, so
        // `git` declines and there is nothing to watch. `--git-path` rather
        // than an assumed `.git/modules/...` layout, for the reason
        // `watch_git_refs` gives: a submodule inside a linked worktree keeps
        // its git directory somewhere neither guess would find.
        if let Some(head) = git(&dir, &["rev-parse", "--git-path", "HEAD"]) {
            watch_if_present(&dir.join(head));
        }
    }
}

/// Emits a watch only for a path that exists.
///
/// Cargo reads `rerun-if-changed` on a *missing* path as "rerun every time",
/// so watching a loose ref that happens to be packed would re-read and
/// re-embed every bundle on every single build.
fn watch_if_present(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Runs `git` inside `root`, returning trimmed stdout when it says something.
///
/// Every failure collapses to `None` deliberately — no `git` on `PATH`, no
/// repository beside the crate, a shallow clone git declines to answer for, a
/// non-zero exit, non-UTF-8 output, or success with nothing to say. None of
/// them may turn a diagnostic string into a red build.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    // git walks *upwards* until it finds a repository. A crate unpacked into
    // a registry cache under a home directory that happens to be versioned
    // would otherwise be stamped with that unrelated repository's commit,
    // which is a worse answer than `"unknown"` because it looks right.
    if !root.join(".git").exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        // A build must not write to the repository it is reading: without
        // this, `git status` refreshes the index and takes `index.lock`,
        // which two builds running in parallel can collide on.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Every file under `agents/`, keyed by its path relative to that directory.
///
/// Recursive because `prompt_files` entries live in subdirectories beside the
/// agent that names them; the consumer is what restricts *roster* parsing to
/// the immediate directory.
fn collect(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect(base, &path, out);
            continue;
        }
        let Ok(rel) = path.strip_prefix(base) else {
            continue;
        };
        // Forward slashes regardless of host, so the embedded keys match the
        // `prompt_files` entries authors write.
        let key = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        match std::fs::read_to_string(&path) {
            Ok(body) => out.push((key, body)),
            // A non-UTF-8 file under `agents/` is not a roster file and not a
            // prompt document. Skipping it keeps a stray binary from failing
            // every build.
            Err(_) => continue,
        }
    }
}

/// Embeds `globals/` and the shared `skills/` library into `embedded_globals.rs`.
///
/// Absent directories are not this script's business to fail on, exactly as with
/// `companies/`: the crate still compiles, and `crate::globals` is what asserts
/// the baseline is non-empty.
fn embed_globals(root: &Path) {
    let globals = root.join("globals");
    let skills = root.join("skills");
    println!("cargo:rerun-if-changed={}", globals.display());
    println!("cargo:rerun-if-changed={}", skills.display());
    for sub in ["agents", "workflows", "ledgers"] {
        println!("cargo:rerun-if-changed={}", globals.join(sub).display());
    }

    let manifest = std::fs::read_to_string(globals.join("globals.toml")).unwrap_or_default();
    // The baseline's seed board cards. A file at the `globals/` root, so the
    // watch above already covers it — no `rerun-if-changed` of its own needed.
    let tasks = std::fs::read_to_string(globals.join("tasks.toml")).unwrap_or_default();

    let mut agents = Vec::new();
    collect(
        &globals.join("agents"),
        &globals.join("agents"),
        &mut agents,
    );
    agents.sort_by(|a, b| a.0.cmp(&b.0));

    let mut workflows = Vec::new();
    collect(
        &globals.join("workflows"),
        &globals.join("workflows"),
        &mut workflows,
    );
    workflows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut ledgers = Vec::new();
    collect(
        &globals.join("ledgers"),
        &globals.join("ledgers"),
        &mut ledgers,
    );
    ledgers.sort_by(|a, b| a.0.cmp(&b.0));

    // Every shared skill, keyed by slug: which of them the baseline *installs*
    // is `[skills].always`, read at runtime rather than here, so this script
    // never has to parse TOML to decide what to embed.
    let mut skill_docs: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills) {
        for entry in entries.filter_map(Result::ok) {
            let dir = entry.path();
            let Some(slug) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            println!("cargo:rerun-if-changed={}", dir.display());
            if let Ok(body) = std::fs::read_to_string(dir.join("SKILL.md")) {
                skill_docs.push((slug.to_string(), body));
            }
        }
    }
    skill_docs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from("// @generated by build.rs — do not edit.\n");
    out.push_str(&format!(
        "/// `globals/globals.toml`, verbatim (empty when the directory is absent).\n\
         pub static EMBEDDED_GLOBALS_MANIFEST: &str = {manifest:?};\n"
    ));
    out.push_str(&format!(
        "/// `globals/tasks.toml`, verbatim (empty when the file is absent).\n\
         pub static EMBEDDED_GLOBAL_TASKS: &str = {tasks:?};\n"
    ));
    write_pairs(
        &mut out,
        "EMBEDDED_GLOBAL_AGENTS",
        "Every file under `globals/agents/`, keyed by path relative to it, sorted.",
        &agents,
    );
    write_pairs(
        &mut out,
        "EMBEDDED_GLOBAL_WORKFLOWS",
        "Every file under `globals/workflows/`, keyed by path relative to it, sorted.",
        &workflows,
    );
    write_pairs(
        &mut out,
        "EMBEDDED_GLOBAL_LEDGERS",
        "Every file under `globals/ledgers/`, keyed by path relative to it, sorted.",
        &ledgers,
    );
    write_pairs(
        &mut out,
        "EMBEDDED_SHARED_SKILLS",
        "Every shared-library `SKILL.md`, keyed by skill slug, sorted.",
        &skill_docs,
    );

    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded_globals.rs");
    std::fs::write(dest, out).unwrap();
}

fn write_pairs(out: &mut String, name: &str, doc: &str, pairs: &[(String, String)]) {
    out.push_str(&format!(
        "/// {doc}\npub static {name}: &[(&str, &str)] = &[\n"
    ));
    for (key, body) in pairs {
        out.push_str(&format!("    ({key:?}, {body:?}),\n"));
    }
    out.push_str("];\n");
}

fn write_table(bundles: &BTreeMap<String, Vec<(String, String)>>) {
    let mut out = String::from(
        "// @generated by build.rs — do not edit.\n\
         /// Every shipped bundle's `agents/` directory, keyed by company id.\n\
         /// Inner entries are paths relative to `agents/`, sorted.\n\
         pub static EMBEDDED_AGENT_BUNDLES: &[(&str, &[(&str, &str)])] = &[\n",
    );
    for (id, files) in bundles {
        out.push_str(&format!("    ({id:?}, &[\n"));
        for (name, body) in files {
            out.push_str(&format!("        ({name:?}, {body:?}),\n"));
        }
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");

    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded_agents.rs");
    std::fs::write(dest, out).unwrap();
}
