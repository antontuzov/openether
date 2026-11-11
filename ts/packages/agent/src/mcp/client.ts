import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';

export interface McpTool {
  name: string;
  description: string;
  inputSchema: unknown;
}

/**
 * Thin wrapper around an MCP server that is spawned over stdio.
 *
 * The agent never talks to the trading core over HTTP: every privileged action
 * goes through the MCP server, which is the only sanctioned interface.
 */
export class McpClient {
  private readonly client: Client;
  private transport: StdioClientTransport | null = null;

  constructor(private readonly name: string) {
    this.client = new Client({ name, version: '0.1.0' }, { capabilities: {} });
  }

  async connect(command: string, args: string[], env: NodeJS.ProcessEnv = {}): Promise<void> {
    this.transport = new StdioClientTransport({
      command,
      args,
      env: {
        ...process.env,
        ...env,
      } as Record<string, string>,
      stderr: 'inherit',
    });
    await this.client.connect(this.transport);
  }

  async listTools(): Promise<McpTool[]> {
    const result = await this.client.listTools();
    return result.tools.map((tool) => ({
      name: tool.name,
      description: tool.description ?? '',
      inputSchema: tool.inputSchema ?? { type: 'object' },
    }));
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<string> {
    const result = await this.client.callTool({ name, arguments: args });
    const content = (result.content ?? []) as Array<{ type: string; text?: string }>;
    const text = content
      .filter((part) => part.type === 'text')
      .map((part) => part.text ?? '')
      .join('\n');
    if (result.isError) {
      throw new Error(text || `tool ${name} failed`);
    }
    return text;
  }

  async close(): Promise<void> {
    await this.client.close().catch(() => undefined);
    this.transport = null;
  }
}
