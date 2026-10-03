//! Tests for the process-wide OpenHuman runtime cell.

use super::*;

/// Two callers get one runtime: the second `global` returns the same `Arc`
/// rather than tripping `AlreadyRunning`. This is the property every test
/// binary in this crate relies on, since each `#[tokio::test]` reaches for
/// the runtime independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_hands_out_one_runtime_per_process() {
    let first = global(RuntimeBoot::ephemeral()).await.expect("runtime");
    let second = global(RuntimeBoot::ephemeral()).await.expect("runtime");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(existing().is_some());
}

/// A boot with no workspace resolves to an ephemeral one, so a unit test
/// never writes into the operator's `~/.openhuman`.
#[test]
fn an_unset_workspace_is_ephemeral() {
    let boot = RuntimeBoot::default();
    assert!(matches!(boot.workspace(), Workspace::Ephemeral));
    let boot = RuntimeBoot {
        workspace_dir: Some(PathBuf::from("/data/openhuman")),
        ..RuntimeBoot::default()
    };
    match boot.workspace() {
        Workspace::Dir(dir) => assert_eq!(dir, PathBuf::from("/data/openhuman/workspace")),
        other => panic!("expected a directory workspace, got {other:?}"),
    }
}

#[test]
fn the_shared_runtime_never_registers_openhumans_memory_domain() {
    // One runtime serves every company; its memory domain would ingest every
    // company's turns into one engine under one credential.
    let domains = super::host_domains();
    assert!(
        !domains.memory,
        "memory must stay off on the shared runtime"
    );
    assert!(
        domains.agent,
        "the agent domain is what a company turn runs on"
    );
    assert!(domains.mcp && domains.skills);
}
