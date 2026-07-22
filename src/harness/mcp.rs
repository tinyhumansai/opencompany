//! OpenHuman MCP registry wrapper for the embedded company runtime.

use std::collections::HashMap;
use std::path::PathBuf;

use openhuman_core::openhuman as oh;
use serde_json::Value;

use oh::mcp_registry::types::{ConnStatus, InstalledServer, McpTool};

use crate::error::OpenCompanyError;

/// Company-home-scoped persistence and access to OpenHuman's live MCP registry.
pub struct McpRuntime {
    config: oh::config::Config,
}

impl McpRuntime {
    /// Creates a runtime whose MCP SQLite store lives beneath `workspace_dir`.
    pub fn new(workspace_dir: PathBuf) -> Self {
        let mut config = oh::config::Config::default();
        config.workspace_dir = workspace_dir;
        Self { config }
    }

    /// Reconnects enabled installed servers. Failures are logged by OpenHuman
    /// per server and never prevent the company runtime from booting.
    pub async fn boot(&self) {
        oh::mcp_registry::boot::spawn_installed_servers(&self.config).await;
    }

    /// Returns every persisted install without loading secret environment values.
    pub fn list(&self) -> crate::Result<Vec<InstalledServer>> {
        oh::mcp_registry::store::list_servers(&self.config).map_err(store_error)
    }

    /// Persists an install and its write-only environment values.
    pub fn install(
        &self,
        server: &InstalledServer,
        env: &HashMap<String, String>,
    ) -> crate::Result<()> {
        oh::mcp_registry::store::insert_server(&self.config, server).map_err(store_error)?;
        if let Err(error) =
            oh::mcp_registry::store::set_env_values(&self.config, &server.server_id, env)
        {
            let _ = oh::mcp_registry::store::delete_server(&self.config, &server.server_id);
            return Err(store_error(error));
        }
        Ok(())
    }

    /// Loads an installed server, establishing the company-store membership
    /// check before touching OpenHuman's process-global connection registry.
    pub fn get(&self, server_id: &str) -> crate::Result<InstalledServer> {
        oh::mcp_registry::store::get_server(&self.config, server_id)
            .map_err(|_| OpenCompanyError::McpServerNotFound(server_id.to_string()))
    }

    /// Connects an installed server and returns its advertised tools.
    pub async fn connect(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        let server = self.get(server_id)?;
        oh::mcp_registry::connections::connect(&self.config, &server)
            .await
            .map_err(harness_error)
    }

    /// Disconnects an installed server after verifying it belongs to this store.
    pub async fn disconnect(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        Ok(oh::mcp_registry::connections::disconnect(server_id).await)
    }

    /// Disconnects and deletes an installed server and its environment values.
    pub async fn uninstall(&self, server_id: &str) -> crate::Result<bool> {
        self.get(server_id)?;
        oh::mcp_registry::connections::disconnect(server_id).await;
        oh::mcp_registry::store::delete_server(&self.config, server_id).map_err(store_error)
    }

    /// Returns connection state joined by OpenHuman against this runtime's store.
    pub async fn status(&self) -> Vec<ConnStatus> {
        oh::mcp_registry::connections::all_status(&self.config).await
    }

    /// Returns the cached tool list for a connected installed server.
    pub async fn tools(&self, server_id: &str) -> crate::Result<Vec<McpTool>> {
        self.get(server_id)?;
        oh::mcp_registry::connections::tools_for(server_id)
            .await
            .ok_or_else(|| {
                OpenCompanyError::InvalidRequest(format!(
                    "MCP server '{server_id}' is not connected"
                ))
            })
    }

    /// Calls one tool after verifying the server belongs to this runtime's store.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> crate::Result<Value> {
        self.get(server_id)?;
        oh::mcp_registry::connections::call_tool(server_id, tool_name, arguments)
            .await
            .map_err(harness_error)
    }
}

fn store_error(error: anyhow::Error) -> OpenCompanyError {
    OpenCompanyError::Store(format!("MCP registry: {error}"))
}

fn harness_error(error: impl std::fmt::Display) -> OpenCompanyError {
    OpenCompanyError::Harness(format!("MCP registry: {error}"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use oh::mcp_registry::types::{CommandKind, Transport};

    use super::*;

    const NODE_STUB: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (value) => process.stdout.write(JSON.stringify(value) + '\n');
rl.on('line', (line) => {
  const request = JSON.parse(line);
  if (!request.id) return;
  if (request.method === 'initialize') {
    send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'test', version: '1' } } });
  } else if (request.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: request.id, result: { tools: [{ name: 'echo', description: 'Echo text', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } }] } });
  } else if (request.method === 'tools/call') {
    send({ jsonrpc: '2.0', id: request.id, result: { content: [{ type: 'text', text: 'echo: ' + request.params.arguments.text }] } });
  }
});
"#;

    #[tokio::test]
    async fn install_connect_call_disconnect_round_trip() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping MCP runtime test because node is unavailable");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("mcp-stub.cjs");
        std::fs::write(&script, NODE_STUB).expect("write node stub");
        let runtime = McpRuntime::new(temp.path().join("workspace"));
        let server = InstalledServer {
            server_id: uuid::Uuid::new_v4().to_string(),
            qualified_name: "test-node-echo".to_string(),
            display_name: "Test Node Echo".to_string(),
            description: None,
            icon_url: None,
            command_kind: CommandKind::Binary,
            command: "node".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env_keys: vec![],
            config: None,
            installed_at: 0,
            last_connected_at: None,
            transport: Transport::Stdio,
            enabled: true,
        };

        runtime.install(&server, &HashMap::new()).expect("install");
        let tools = runtime.connect(&server.server_id).await.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let result = runtime
            .call_tool(
                &server.server_id,
                "echo",
                serde_json::json!({"text": "hello"}),
            )
            .await
            .expect("call");
        assert_eq!(result["content"][0]["text"], "echo: hello");

        assert!(
            runtime
                .disconnect(&server.server_id)
                .await
                .expect("disconnect")
        );
        assert!(
            runtime
                .uninstall(&server.server_id)
                .await
                .expect("uninstall")
        );
        assert!(runtime.list().expect("list").is_empty());
    }
}
