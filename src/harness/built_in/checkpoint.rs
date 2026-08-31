//! Automatic Git checkpoints for an agent's private filesystem workspace.
//!
//! The feature is deliberately a tool decorator: file tools, patches, shell
//! redirects, downloads, and future workspace-writing tools all pass the same
//! after-call boundary. A call that changed nothing produces no commit, and a
//! Git failure is logged without replacing the tool's real result.
//!
//! History is permanent for the lifetime of the out-of-band repository: a
//! workspace file committed at one checkpoint and deleted later survives in
//! `workspace.git` objects, so operators who enable checkpoints on workspaces
//! that can hold secrets must rotate or purge history deliberately (see the
//! retention note in `docs/spec/runtime/workspace-layout.md`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use oh::tools::traits::{
    PermissionLevel, Tool, ToolCallOptions, ToolCategory, ToolResult, ToolScope, ToolSpec,
    ToolTimeout,
};
use openhuman_core::openhuman as oh;

use crate::store::fs::path_lock;

const CHECKPOINT_AUTHOR_NAME: &str = "OpenCompany Workspace";
const CHECKPOINT_AUTHOR_EMAIL: &str = "workspace@opencompany.local";

/// A Git repository whose working tree is one agent workspace.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceCheckpointer {
    workspace: PathBuf,
    git_dir: PathBuf,
}

impl WorkspaceCheckpointer {
    /// Initializes (or reopens) the workspace repository and records a baseline.
    ///
    /// The object database always lives **beside** the workspace at
    /// `workspace.git`; `workspace/.git` is only Git's small pointer file. This
    /// keeps history out of the files agents enumerate and publish while still
    /// allowing ordinary `git` commands run inside the workspace to discover it.
    ///
    /// The pointer file is write-only from the checkpointer's point of view. An
    /// agent can plant a `.git` of its own (any file an agent can read it can
    /// rewrite), so `initialize` never resolves its Git directory through it —
    /// every Git invocation passes an explicit `--git-dir` for the out-of-band
    /// path, and a planted pointer is overwritten with the real one so commands
    /// the agent itself runs still discover the genuine repository.
    pub(crate) fn initialize(workspace: &Path) -> anyhow::Result<Self> {
        Self::initialize_with_lock_wait(workspace, true)
    }

    fn initialize_with_lock_wait(workspace: &Path, wait_for_lock: bool) -> anyhow::Result<Self> {
        std::fs::create_dir_all(workspace)?;
        let out_of_band = workspace.with_extension("git");

        // The in-workspace `.git` is checkpoint scaffolding, not agent data: it
        // exists only so ordinary `git` commands run inside the workspace
        // discover the out-of-band repository. Normalize it, dropping anything
        // (a pointer file or a planted directory) an agent left there — a
        // planted pointer could make `git init --separate-git-dir` refuse to
        // run or redirect ordinary `git` commands at a decoy repository.
        let dot_git = workspace.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            if meta.is_dir() {
                std::fs::remove_dir_all(&dot_git)?;
            } else {
                std::fs::remove_file(&dot_git)?;
            }
        }

        if !out_of_band.join("HEAD").is_file() {
            // Global options (`-c`, env) must precede the subcommand, so the
            // isolation is applied before `init` is named.
            let mut init = Command::new("git");
            isolate_git(&mut init);
            init.args(["init", "--quiet", "--initial-branch=checkpoints"])
                .arg("--separate-git-dir")
                .arg(&out_of_band)
                .arg(workspace);
            let status = init.status()?;
            require_success(status, "git init")?;
        }

        // Sanitize the in-workspace pointer to the out-of-band directory, even
        // when the agent planted one of its own. Never *read* an existing
        // pointer to derive a Git directory: an agent-controlled pointer could
        // name a repository whose hooks `git commit` would run in the host
        // process (CWE-94), which `base_git`'s explicit `--git-dir` and hook
        // suppression exist to make unreachable.
        std::fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", out_of_band.display()),
        )?;

        let checkpointer = Self {
            workspace: workspace.to_path_buf(),
            git_dir: out_of_band,
        };
        checkpointer.initialize_baseline(wait_for_lock)?;
        Ok(checkpointer)
    }

    /// Initializes checkpointing without blocking a Tokio worker thread.
    ///
    /// [`initialize`](Self::initialize) shells out to `git init` and commits the
    /// baseline, which can take tens of milliseconds of blocking file and
    /// subprocess I/O. When this is called from an async harness path it should
    /// run off the async worker pool: on a multi-threaded runtime the work is
    /// moved off the worker via [`block_in_place`](tokio::task::block_in_place);
    /// on a current-thread runtime or outside any runtime `block_in_place`
    /// panics, so it runs inline. In practice roster builds happen on the
    /// multi-threaded runtime, making the inline path a defensive fallback.
    pub(crate) fn initialize_off_worker(workspace: &Path) -> anyhow::Result<Self> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| Self::initialize(workspace))
            }
            // A current-thread runtime cannot synchronously wait for a Tokio
            // mutex: the future holding it needs this same worker to release
            // its guard. Fail safely on contention and let build_agent's
            // existing warning/fallback path proceed without checkpointing.
            Ok(_) => Self::initialize_with_lock_wait(workspace, false),
            Err(_) => Self::initialize(workspace),
        }
    }

    /// Records the baseline commit under the same process-wide lock the
    /// per-call checkpoint path holds, so an in-flight tool checkpoint cannot
    /// contend with this one on the Git index.
    ///
    /// On a multi-threaded runtime (or outside Tokio), a contended initializer
    /// may wait because another worker can poll the checkpoint future that
    /// releases the guard. On a current-thread runtime it must fail instead:
    /// synchronously spinning would prevent that sole worker from polling the
    /// guard owner and deadlock roster construction.
    fn initialize_baseline(&self, wait_for_lock: bool) -> anyhow::Result<()> {
        let lock = path_lock(&self.git_dir);
        let _guard = if wait_for_lock {
            loop {
                match lock.try_lock() {
                    Ok(guard) => break guard,
                    Err(_) => std::thread::yield_now(),
                }
            }
        } else {
            lock.try_lock().map_err(|_| {
                anyhow::anyhow!(
                    "workspace checkpoint initialization is contending with an active checkpoint"
                )
            })?
        };
        self.checkpoint_unlocked("initialize workspace", true)
    }

    /// Records current workspace changes. Failures are returned for the caller
    /// to log, never folded into the tool result.
    async fn checkpoint(&self, tool_name: &str) -> anyhow::Result<()> {
        let lock = path_lock(&self.git_dir);
        let _guard = lock.lock().await;
        let this = self.clone();
        let message = format!("after {tool_name}");
        tokio::task::spawn_blocking(move || this.checkpoint_unlocked(&message, false)).await??;
        Ok(())
    }

    fn checkpoint_unlocked(&self, message: &str, allow_empty_initial: bool) -> anyhow::Result<()> {
        require_success(self.git(["add", "--all"])?.status, "git add")?;

        let diff = self.git(["diff", "--cached", "--quiet"])?.status;
        let has_changes = match diff.code() {
            Some(0) => false,
            Some(1) => true,
            _ => anyhow::bail!("git diff --cached failed with {diff}"),
        };
        let has_head = self
            .git(["rev-parse", "--verify", "HEAD"])?
            .status
            .success();
        if !has_changes && (has_head || !allow_empty_initial) {
            return Ok(());
        }

        let mut command = self.base_git();
        command.args(["commit", "--quiet"]);
        if !has_changes {
            command.arg("--allow-empty");
        }
        let output = command
            .arg("-m")
            .arg(format!("checkpoint: {message}"))
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn base_git(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.workspace.display()));
        isolate_git(&mut command);
        command
            .args(["-c", &format!("user.name={CHECKPOINT_AUTHOR_NAME}")])
            .args(["-c", &format!("user.email={CHECKPOINT_AUTHOR_EMAIL}")]);
        command
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> anyhow::Result<std::process::Output> {
        Ok(self.base_git().args(args).output()?)
    }
}

/// Applies the checkpointer's configuration isolation to a Git command: no
/// inherited global or system config, and no repository hooks.
///
/// Command-line `-c` overrides rank above repository config, so even a
/// `core.hooksPath` an agent managed to write into the out-of-band repository's
/// config is ignored, and `GIT_CONFIG_NOSYSTEM` / `GIT_CONFIG_GLOBAL` cut off
/// config injection through the environment. Together these keep a committed
/// checkpoint from ever executing code (`core.hooksPath` can point into the
/// agent workspace) in the host process (CWE-94).
fn isolate_git(command: &mut Command) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "core.hooksPath="]);
}

fn require_success(status: ExitStatus, operation: &str) -> anyhow::Result<()> {
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{operation} failed with {status}")
    }
}

/// Wraps a tool and checkpoints the workspace after every completed call.
///
/// Every tool is wrapped rather than maintaining a fragile list of writers.
/// Read-only and external tools pay only an unchanged-tree check, while shell
/// redirects and newly-added writers cannot bypass checkpointing accidentally.
pub(crate) struct CheckpointingTool {
    inner: Box<dyn Tool>,
    checkpointer: Arc<WorkspaceCheckpointer>,
}

impl CheckpointingTool {
    pub(crate) fn wrap_all(
        tools: Vec<Box<dyn Tool>>,
        checkpointer: WorkspaceCheckpointer,
    ) -> Vec<Box<dyn Tool>> {
        let checkpointer = Arc::new(checkpointer);
        tools
            .into_iter()
            .map(|inner| {
                Box::new(Self {
                    inner,
                    checkpointer: checkpointer.clone(),
                }) as Box<dyn Tool>
            })
            .collect()
    }

    async fn checkpoint_after<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if let Err(error) = self.checkpointer.checkpoint(self.inner.name()).await {
            tracing::warn!(
                tool = self.inner.name(),
                workspace = %self.checkpointer.workspace.display(),
                %error,
                "[workspace-checkpoint] Git checkpoint failed; preserving the tool result"
            );
        }
        result
    }
}

#[async_trait]
impl Tool for CheckpointingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn supports_markdown(&self) -> bool {
        self.inner.supports_markdown()
    }
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }
    fn permission_level(&self) -> PermissionLevel {
        self.inner.permission_level()
    }
    fn permission_level_with_args(&self, args: &Value) -> PermissionLevel {
        self.inner.permission_level_with_args(args)
    }
    fn scope(&self) -> ToolScope {
        self.inner.scope()
    }
    fn category(&self) -> ToolCategory {
        self.inner.category()
    }
    fn is_concurrency_safe(&self, args: &Value) -> bool {
        self.inner.is_concurrency_safe(args)
    }
    fn external_effect(&self) -> bool {
        self.inner.external_effect()
    }
    fn external_effect_with_args(&self, args: &Value) -> bool {
        self.inner.external_effect_with_args(args)
    }
    // Host metadata this wrapper must not swallow.
    //
    // `Tool` used to carry a typed `generated_runtime_context`, and this
    // decorator forwarded it. The tinytools extraction replaced it with an
    // ERASED pair — `host_extension` for what the tool is, `host_call_extension`
    // for what a particular call is — because the answers are host policy
    // (OpenCompany's generated-tool provenance, OpenHuman's pack-registry
    // handle) and a shared vocabulary has no business naming either. The typed
    // reader is a free function now: `oh::agent::tools::traits`'s
    // `generated_runtime_context`, which downcasts what these return.
    //
    // Both must be forwarded, and forwarding is the whole job of this
    // decorator: a wrapper that answered `None` (the default) would make every
    // wrapped tool look like a tool with no provenance and no pack, silently,
    // to policy that has no other way to ask.
    fn host_extension(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        self.inner.host_extension()
    }

    fn host_call_extension(&self, args: &Value) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        self.inner.host_call_extension(args)
    }
    fn max_result_size_chars(&self) -> Option<usize> {
        self.inner.max_result_size_chars()
    }
    fn timeout_policy(&self, args: &Value) -> ToolTimeout {
        self.inner.timeout_policy(args)
    }
    fn display_label(&self, args: &Value) -> Option<String> {
        self.inner.display_label(args)
    }
    fn display_detail(&self, args: &Value) -> Option<String> {
        self.inner.display_detail(args)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let result = self.inner.execute(args).await;
        self.checkpoint_after(result).await
    }

    async fn execute_with_options(
        &self,
        args: Value,
        options: ToolCallOptions,
    ) -> anyhow::Result<ToolResult> {
        let result = self.inner.execute_with_options(args, options).await;
        self.checkpoint_after(result).await
    }

    // `Option<&dyn ToolRunContext>`, not the concrete TinyAgents
    // `ToolExecutionContext` this used to name. The tinytools extraction turned
    // the context into a trait so a shared tool vocabulary need not depend on
    // tinyagents (that would be a dependency cycle — tinyagents depends on
    // tinytools). The trait exposes the workspace, the thread id and the output
    // budget and nothing else; the run id, event sink and cancellation token
    // stay harness-internal on purpose.
    //
    // What matters here is unchanged and is the reason this method is
    // overridden at all: the context carries the per-worker worktree the
    // vendored tool uses as its action dir, so it is forwarded whole. Dropping
    // it would silently move where commands run.
    async fn execute_with_context(
        &self,
        args: Value,
        options: ToolCallOptions,
        context: Option<&dyn oh::tools::traits::ToolRunContext>,
    ) -> anyhow::Result<ToolResult> {
        let result = self
            .inner
            .execute_with_context(args, options, context)
            .await;
        self.checkpoint_after(result).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    struct WriteTool(PathBuf);

    #[async_trait]
    impl Tool for WriteTool {
        fn name(&self) -> &str {
            "write_fixture"
        }
        fn description(&self) -> &str {
            "writes a fixture"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
            std::fs::write(&self.0, args["body"].as_str().unwrap_or_default())?;
            Ok(ToolResult::success("written"))
        }
    }

    fn log(workspace: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["log", "--format=%s"])
                .current_dir(workspace)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn initializes_out_of_band_and_checkpoints_a_tool_write() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();
        let mut tools = CheckpointingTool::wrap_all(
            vec![Box::new(WriteTool(workspace.join("answer.txt")))],
            checkpointer,
        );

        let result = tools
            .remove(0)
            .execute(json!({"body": "42"}))
            .await
            .unwrap();

        assert_eq!(result.output(), "written");
        assert!(workspace.join(".git").is_file());
        assert!(dir.path().join("workspace.git/HEAD").is_file());
        let history = log(&workspace);
        assert!(
            history.contains("checkpoint: after write_fixture"),
            "{history}"
        );
        assert!(
            history.contains("checkpoint: initialize workspace"),
            "{history}"
        );
    }

    #[tokio::test]
    async fn an_unchanged_tool_call_creates_no_checkpoint() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();
        checkpointer.checkpoint("read_only").await.unwrap();
        assert_eq!(log(&workspace).lines().count(), 1);
    }

    #[tokio::test]
    async fn a_failed_checkpoint_preserves_the_successful_tool_result() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();
        std::fs::remove_file(checkpointer.git_dir.join("HEAD")).unwrap();
        let mut tools = CheckpointingTool::wrap_all(
            vec![Box::new(WriteTool(workspace.join("answer.txt")))],
            checkpointer,
        );

        let result = tools
            .remove(0)
            .execute(json!({"body": "still written"}))
            .await
            .unwrap();

        assert_eq!(result.output(), "written");
        assert_eq!(
            std::fs::read_to_string(workspace.join("answer.txt")).unwrap(),
            "still written"
        );
    }

    #[tokio::test]
    async fn concurrent_tool_calls_serialize_the_git_index() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();
        let mut tools = CheckpointingTool::wrap_all(
            vec![
                Box::new(WriteTool(workspace.join("one.txt"))),
                Box::new(WriteTool(workspace.join("two.txt"))),
            ],
            checkpointer,
        );
        let one = tools.remove(0);
        let two = tools.remove(0);

        let (one_result, two_result) = tokio::join!(
            one.execute(json!({"body": "one"})),
            two.execute(json!({"body": "two"}))
        );

        assert!(!one_result.unwrap().is_error);
        assert!(!two_result.unwrap().is_error);
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(String::from_utf8(status.stdout).unwrap().trim().is_empty());
        assert!(!dir.path().join("workspace.git/index.lock").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_written_git_pointer_cannot_redirect_checkpoints() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        // A decoy "repository" the agent's pointer names, bearing a hook that
        // would expose host execution were the checkpointer to commit inside
        // it.
        let decoy = dir.path().join("decoy");
        let decoy_hooks = decoy.join("hooks");
        std::fs::create_dir_all(&decoy_hooks).unwrap();
        std::fs::write(
            decoy_hooks.join("post-commit"),
            "#!/bin/sh\ntouch host-executed\n",
        )
        .unwrap();

        // The agent plants its own pointer before the checkpointer initializes.
        std::fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", decoy.display()),
        )
        .unwrap();

        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();
        let mut tools = CheckpointingTool::wrap_all(
            vec![Box::new(WriteTool(workspace.join("answer.txt")))],
            checkpointer,
        );
        tools
            .remove(0)
            .execute(json!({"body": "42"}))
            .await
            .unwrap();

        // Checkpoints land in the out-of-band repository, discovered normally
        // through the sanitized pointer...
        assert!(dir.path().join("workspace.git/HEAD").is_file());
        let history = log(&workspace);
        assert!(
            history.contains("checkpoint: after write_fixture"),
            "{history}"
        );
        // ...the planted pointer was overwritten with the real one...
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git")).unwrap(),
            format!("gitdir: {}\n", dir.path().join("workspace.git").display())
        );
        // ...the decoy repository was never initialized...
        assert!(!decoy.join("HEAD").is_file());
        // ...and its hook never ran in the host process.
        assert!(!dir.path().join("host-executed").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoint_commits_never_run_repository_hooks() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        let checkpointer = WorkspaceCheckpointer::initialize(&workspace).unwrap();

        // Even a hook dropped straight into the out-of-band repository's own
        // hooks directory must never execute: `isolate_git` pins `core.hooksPath`
        // so a checkpoint commit cannot run agent-supplied code in the host
        // process, however the repository was poisoned.
        let hooks = checkpointer.git_dir.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("post-commit"), "#!/bin/sh\ntouch hook-ran\n").unwrap();

        let mut tools = CheckpointingTool::wrap_all(
            vec![Box::new(WriteTool(workspace.join("answer.txt")))],
            checkpointer,
        );
        tools
            .remove(0)
            .execute(json!({"body": "42"}))
            .await
            .unwrap();

        assert!(
            !dir.path().join("hook-ran").exists(),
            "a checkpoint commit must not run repository hooks"
        );
        assert!(
            log(&workspace).contains("checkpoint: after write_fixture"),
            "the checkpoint still committed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_blocks_behind_an_in_flight_checkpoint_lock() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        WorkspaceCheckpointer::initialize(&workspace).unwrap();

        // Hold the per-git-dir checkpoint lock, then re-initialize on a thread
        // associated with the runtime: `initialize` must serialize behind the
        // same lock the per-call checkpoint path holds rather than racing it
        // into the Git index.
        let lock = path_lock(&dir.path().join("workspace.git"));
        let _guard = lock.lock().await;

        let entered = tokio::runtime::Handle::current();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _enter = entered.enter();
            let _ = tx.send(WorkspaceCheckpointer::initialize(&workspace).is_ok());
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            rx.try_recv().is_err(),
            "re-initialize raced past the in-flight checkpoint lock"
        );

        drop(_guard);
        assert!(
            rx.recv().expect("re-initialize completes"),
            "re-initialize must succeed once the checkpoint lock is released"
        );
        thread.join().expect("lock thread");
    }

    #[test]
    fn current_thread_initialization_fails_instead_of_deadlocking_on_contention() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");
        WorkspaceCheckpointer::initialize(&workspace).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let lock = path_lock(&dir.path().join("workspace.git"));
            let _guard = lock.lock().await;
            let started = std::time::Instant::now();

            let error = WorkspaceCheckpointer::initialize_off_worker(&workspace)
                .expect_err("current-thread initialization must not wait on a Tokio mutex");

            assert!(
                started.elapsed() < std::time::Duration::from_secs(1),
                "contended initialization blocked the current-thread runtime"
            );
            assert!(error.to_string().contains("contending"), "{error}");
        });
    }
}
