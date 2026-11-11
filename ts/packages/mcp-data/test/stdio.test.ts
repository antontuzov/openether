import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

/**
 * The agent spawns this server over stdio with the MCP SDK client, so the
 * handshake has to work with that exact client. Only the protocol is
 * exercised here; no tool is called, so no request leaves the machine.
 */
describe('the stdio transport', () => {
  it('completes the handshake and lists the tools', async () => {
    const packageDir = fileURLToPath(new URL('..', import.meta.url));
    const tsx = fileURLToPath(new URL('../node_modules/tsx/dist/cli.mjs', import.meta.url));

    const client = new Client({ name: 'openether-mcp-data-test', version: '0.1.0' }, {
      capabilities: {},
    });
    const transport = new StdioClientTransport({
      command: 'node',
      args: [tsx, 'src/index.ts'],
      cwd: packageDir,
      stderr: 'ignore',
    });

    await client.connect(transport);
    try {
      const result = await client.listTools();
      const names = result.tools.map((tool) => tool.name).sort();
      expect(names).toContain('list_top_coins');
      expect(names).toContain('get_solana_balances');
      expect(names).toContain('get_evm_tx_history');
      for (const tool of result.tools) {
        expect(tool.inputSchema.type).toBe('object');
      }
    } finally {
      await client.close();
    }
  }, 30_000);
});
