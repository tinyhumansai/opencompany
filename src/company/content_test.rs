//! Content-validation walk over the shipped `companies/*` and `skills/*`.
//!
//! These tests parse every data file the WS1 readers cover against the real
//! on-disk content, so any future content edit that breaks the frozen formats
//! fails CI. This guards WS8 authoring forever (see `docs/specs/09-verification.md`).

use std::path::{Path, PathBuf};

use super::{CompanyManifest, load_dir_skills, parse_workflow, walk_workspace};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_company_manifest_is_valid() {
    let companies = repo_root().join("companies");
    let dirs = subdirs(&companies);
    assert!(!dirs.is_empty(), "no companies found under {companies:?}");

    for company in dirs {
        let manifest = CompanyManifest::from_path(&company)
            .unwrap_or_else(|err| panic!("{}: {err}", company.display()));
        let problems = manifest.validate();
        assert!(
            problems.is_empty(),
            "{} has manifest problems: {problems:?}",
            company.display()
        );
    }
}

#[test]
fn every_workflow_graph_parses() {
    for company in subdirs(&repo_root().join("companies")) {
        for file in toml_files(&company.join("workflows")) {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
            let workflow =
                parse_workflow(&text).unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            // The filename must match the declared workflow id.
            let stem = file.file_stem().and_then(|stem| stem.to_str()).unwrap();
            assert_eq!(
                workflow.id,
                stem,
                "{} declares id `{}` but is named `{stem}.toml`",
                file.display(),
                workflow.id
            );
        }
    }
}

#[test]
fn every_company_skill_and_workspace_parses() {
    for company in subdirs(&repo_root().join("companies")) {
        // Per-company skills (a missing dir yields an empty list).
        load_dir_skills(&company.join("skills"))
            .unwrap_or_else(|err| panic!("{}/skills: {err}", company.display()));
        // Workspace tree.
        walk_workspace(&company.join("workspace"))
            .unwrap_or_else(|err| panic!("{}/workspace: {err}", company.display()));
    }
}

#[test]
fn the_repo_skill_registry_parses() {
    let skills = load_dir_skills(&repo_root().join("skills"))
        .unwrap_or_else(|err| panic!("repo skills: {err}"));
    assert!(
        skills.iter().any(|skill| skill.slug == "web-research"),
        "expected the web-research skill in the shared registry"
    );
    for skill in &skills {
        assert!(!skill.name.is_empty(), "skill `{}` has no name", skill.slug);
        assert!(
            !skill.description.is_empty(),
            "skill `{}` has no description",
            skill.slug
        );
    }
}
