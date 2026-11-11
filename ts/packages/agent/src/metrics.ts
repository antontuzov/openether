import { createServer, type Server } from 'node:http';
import { Counter, Gauge, Histogram, Registry, collectDefaultMetrics } from 'prom-client';

/**
 * Prometheus metrics for the agent layer.
 *
 * The core exposes its own metrics; these cover the parts that only the agent
 * can see: how often it plans, how long the model takes and what it decided.
 */
export class AgentMetrics {
  readonly registry = new Registry();

  readonly cycles = new Counter({
    name: 'openether_agent_cycles_total',
    help: 'Planning cycles completed',
    registers: [this.registry],
  });

  readonly llmCalls = new Counter({
    name: 'openether_agent_llm_calls_total',
    help: 'LLM calls by provider and result',
    labelNames: ['provider', 'result'] as const,
    registers: [this.registry],
  });

  readonly llmLatency = new Histogram({
    name: 'openether_agent_llm_latency_seconds',
    help: 'LLM call latency in seconds',
    labelNames: ['provider'] as const,
    buckets: [0.1, 0.25, 0.5, 1, 2, 5, 10, 30],
    registers: [this.registry],
  });

  readonly toolCalls = new Counter({
    name: 'openether_agent_tool_calls_total',
    help: 'Tool calls issued by the agent',
    labelNames: ['tool', 'result'] as const,
    registers: [this.registry],
  });

  readonly trades = new Counter({
    name: 'openether_agent_trades_total',
    help: 'Trade attempts by outcome',
    labelNames: ['result'] as const,
    registers: [this.registry],
  });

  readonly guardFindings = new Counter({
    name: 'openether_agent_guard_findings_total',
    help: 'Prompt injection or allowlist findings in tool output',
    labelNames: ['kind'] as const,
    registers: [this.registry],
  });

  readonly breakerState = new Gauge({
    name: 'openether_agent_breaker_state',
    help: 'Circuit breaker state seen by the agent (0=closed 1=half_open 2=open)',
    registers: [this.registry],
  });

  private server: Server | null = null;

  constructor() {
    collectDefaultMetrics({ register: this.registry });
  }

  async listen(port: number): Promise<void> {
    if (port <= 0) {
      return;
    }
    this.server = createServer(async (request, response) => {
      if (request.url === '/metrics') {
        response.writeHead(200, { 'content-type': this.registry.contentType });
        response.end(await this.registry.metrics());
        return;
      }
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: 'not found' }));
    });
    await new Promise<void>((resolve) => {
      this.server?.listen(port, resolve);
    });
  }

  async close(): Promise<void> {
    if (!this.server) {
      return;
    }
    await new Promise<void>((resolve) => {
      this.server?.close(() => resolve());
    });
    this.server = null;
  }
}
