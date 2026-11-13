import { readFile } from 'node:fs/promises';
import { AgentLogger } from './logger.js';
import { AgentMetrics } from './metrics.js';
import { createProvider } from './llm/index.js';
import { LocalPlannerProvider } from './llm/local.js';
import { loadConfig, splitArgs } from './config.js';
import { McpClient } from './mcp/client.js';
import { HashEmbedder, OpenAiEmbedder } from './memory/embeddings.js';
import { MemoryStore } from './memory/store.js';
import { Orchestrator } from './loop/orchestrator.js';

/**
 * The agent entrypoint.
 *
 * It starts the MCP clients, opens the memory store, and then runs the
 * plan/execute/verify loop on a fixed interval. Every cycle is independent:
 * the agent holds no state between cycles beyond its memory and the risk layer
 * in the core, which stays the only authority on what actually gets traded.
 */
async function main(): Promise<void> {
  const config = loadConfig();

  const logger = new AgentLogger(
    config.logDir,
    config.logLevel as 'debug' | 'info' | 'warn' | 'error',
    config.coreUrl,
  );
  await logger.init();

  const metrics = new AgentMetrics();
  await metrics.listen(config.metricsPort);

  const embedder = config.openaiApiKey
    ? new OpenAiEmbedder(config.openaiApiKey)
    : new HashEmbedder();
  const memory = await MemoryStore.open(config.memoryDir, embedder);

  const provider = createProvider(config);

  const coreClient = new McpClient('openether-agent');
  await coreClient.connect(config.coreMcpCommand, splitArgs(config.coreMcpArgs));

  let dataClient: McpClient | null = null;
  if (config.allowDataTools) {
    dataClient = new McpClient('openether-data');
    try {
      await dataClient.connect(config.dataMcpCommand, splitArgs(config.dataMcpArgs));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      await logger.warn(`the data MCP server is unavailable, continuing without it: ${message}`);
      dataClient = null;
    }
  }

  const orchestrator = new Orchestrator({
    provider,
    coreTools: coreClient,
    dataTools: dataClient ?? undefined,
    logger,
    metrics,
    memory,
    maxSteps: config.maxSteps,
    symbol: config.symbol,
    contractAllowlist: config.contractAllowlist
      .split(',')
      .map((item) => item.trim())
      .filter((item) => item.length > 0),
    onContext: (snapshot) => {
      if (provider instanceof LocalPlannerProvider) {
        provider.context = {
          portfolio: snapshot.portfolio ?? emptyPortfolio(),
          risk: snapshot.risk ?? emptyRisk(),
          ticks: snapshot.ticks,
          symbol: config.symbol,
        };
      }
    },
  });

  await logger.info('the agent started', {
    provider: provider.name,
    symbol: config.symbol,
    interval_ms: config.intervalMs,
    memory_records: memory.size,
    core_mcp: `${config.coreMcpCommand} ${config.coreMcpArgs}`,
    data_mcp: dataClient ? `${config.dataMcpCommand} ${config.dataMcpArgs}` : 'disabled',
  });

  let running = true;
  const stop = async (signal: string): Promise<void> => {
    if (!running) {
      return;
    }
    running = false;
    await logger.info(`the agent is stopping (${signal})`);
    await coreClient.close();
    await dataClient?.close();
    await metrics.close();
  };

  process.on('SIGINT', () => {
    void stop('SIGINT').then(() => process.exit(0));
  });
  process.on('SIGTERM', () => {
    void stop('SIGTERM').then(() => process.exit(0));
  });

  if (config.runOnce) {
    await runCycle(orchestrator, logger);
    await stop('run-once');
    return;
  }

  while (running) {
    await runCycle(orchestrator, logger);
    if (!running) {
      break;
    }
    await sleep(config.intervalMs);
  }
}

async function runCycle(orchestrator: Orchestrator, logger: AgentLogger): Promise<void> {
  try {
    const result = await orchestrator.runCycle();
    await logger.info('cycle finished', {
      planned: result.plan.slice(0, 400),
      tools: result.toolCalls.map((call) => call.name),
      trade_attempted: result.tradeAttempted,
      trade_approved: result.tradeApproved,
      trade_reason: result.tradeReason,
    });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await logger.error(`the cycle failed: ${message}`);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function emptyPortfolio() {
  return {
    venue: 'paper',
    equity_usd: 0,
    cash_usd: 0,
    unrealized_pnl_usd: 0,
    realized_pnl_usd: 0,
    total_pnl_pct: 0,
    onchain_value_usd: 0,
    positions: [],
    ts_ms: Date.now(),
  };
}

function emptyRisk() {
  return {
    breaker_state: 'open' as const,
    breaker_reason: 'the risk state could not be read',
    equity_usd: 0,
    peak_equity_usd: 0,
    drawdown_pct: 0,
    daily_pnl_usd: 0,
    open_positions: 0,
    gross_exposure_usd: 0,
    limits: {},
    ts_ms: Date.now(),
  };
}

main().catch(async (error: unknown) => {
  const message = error instanceof Error ? error.stack ?? error.message : String(error);
  process.stderr.write(`the agent crashed: ${message}\n`);
  process.exitCode = 1;
});
