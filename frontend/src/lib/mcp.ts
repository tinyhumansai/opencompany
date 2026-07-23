export type McpTransport = "stdio" | "http";

export interface McpTool {
  name: string;
  description?: string | null;
  input_schema: Record<string, unknown>;
}

export interface McpServer {
  server_id: string;
  name: string;
  transport: McpTransport;
  command: string;
  args: string[];
  url?: string;
  env_keys: string[];
  enabled: boolean;
  status: "connected" | "connecting" | "disconnected" | "unauthorized" | "error" | "disabled";
  tool_count: number;
  last_error?: string;
}

export interface InstallMcpServerInput {
  name: string;
  transport: McpTransport;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
}

export interface McpCallResult {
  result: unknown;
}
