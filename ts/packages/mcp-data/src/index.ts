#!/usr/bin/env node
/**
 * The openEther data MCP server.
 *
 * It is spawned by the agent over stdio (`pnpm --filter @openether/mcp-data
 * start`) and exposes read-only market and on-chain tools. Nothing is written
 * to stdout except the JSON-RPC traffic.
 */

import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { loadConfig } from './config.js';
import { createServer } from './server.js';
import { createContext } from './runtime.js';
import { TOOLS } from './tools.js';

async function main(): Promise<void> {
  const config = loadConfig();
  const ctx = createContext(config);
  const server = createServer(ctx);
  const transport = new StdioServerTransport();

  await server.connect(transport);
  process.stderr.write(
    `[mcp-data] ready with ${TOOLS.length} tools; the etherscan key is ${
      config.etherscanApiKey ? 'configured' : 'not configured'
    }\n`,
  );

  let closing = false;
  const shutdown = (signal: string): void => {
    if (closing) {
      return;
    }
    closing = true;
    process.stderr.write(`[mcp-data] stopping (${signal})\n`);
    void server
      .close()
      .catch(() => undefined)
      .finally(() => {
        process.exit(0);
      });
  };

  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
  process.stderr.write(`[mcp-data] failed to start: ${message}\n`);
  process.exitCode = 1;
});
