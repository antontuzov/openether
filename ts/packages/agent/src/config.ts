import { z } from 'zod';

const boolish = z
  .string()
  .transform((value) => ['1', 'true', 'yes', 'on'].includes(value.trim().toLowerCase()))
  .catch(false);

const schema = z.object({
  coreUrl: z.string().default('http://127.0.0.1:8080'),
  llmProvider: z.enum(['local', 'openai', 'anthropic', 'openai-compatible']).default('local'),
  llmModel: z.string().default('gpt-4o'),
  llmBaseUrl: z.string().default(''),
  openaiApiKey: z.string().default(''),
  anthropicApiKey: z.string().default(''),
  agentFramework: z.enum(['local', 'agentkit']).default('local'),
  intervalMs: z.coerce.number().int().min(1000).default(15_000),
  maxSteps: z.coerce.number().int().min(1).max(20).default(6),
  symbol: z.string().default('SOL/USDT'),
  runOnce: boolish.default(false),
  memoryDir: z.string().default('./data/memory'),
  logDir: z.string().default('./data/logs'),
  logLevel: z.string().default('info'),
  coreMcpCommand: z.string().default('cargo'),
  coreMcpArgs: z.string().default('run --release -p openether-mcp-server --manifest-path ../rust/Cargo.toml'),
  dataMcpCommand: z.string().default('pnpm'),
  dataMcpArgs: z.string().default('--filter @openether/mcp-data start'),
  metricsPort: z.coerce.number().int().min(0).default(9090),
  allowDataTools: boolish.default(true),
  contractAllowlist: z.string().default(''),
  maxToolOutputChars: z.coerce.number().int().min(500).default(20_000),
});

export type AgentConfig = z.infer<typeof schema>;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): AgentConfig {
  const parsed = schema.safeParse({
    coreUrl: env.CORE_URL,
    llmProvider: env.LLM_PROVIDER,
    llmModel: env.LLM_MODEL,
    llmBaseUrl: env.LLM_BASE_URL,
    openaiApiKey: env.OPENAI_API_KEY,
    anthropicApiKey: env.ANTHROPIC_API_KEY,
    agentFramework: env.AGENT_FRAMEWORK,
    intervalMs: env.AGENT_INTERVAL_MS,
    maxSteps: env.AGENT_MAX_STEPS,
    symbol: env.AGENT_SYMBOL,
    runOnce: env.AGENT_RUN_ONCE,
    memoryDir: env.AGENT_MEMORY_DIR,
    logDir: env.AGENT_LOG_DIR,
    logLevel: env.AGENT_LOG_LEVEL,
    coreMcpCommand: env.CORE_MCP_COMMAND,
    coreMcpArgs: env.CORE_MCP_ARGS,
    dataMcpCommand: env.DATA_MCP_COMMAND,
    dataMcpArgs: env.DATA_MCP_ARGS,
    metricsPort: env.AGENT_METRICS_PORT,
    allowDataTools: env.AGENT_ALLOW_DATA_TOOLS,
    contractAllowlist: env.OPENETHER_CONTRACT_ALLOWLIST,
    maxToolOutputChars: env.AGENT_MAX_TOOL_OUTPUT_CHARS,
  });

  if (!parsed.success) {
    const issues = parsed.error.issues
      .map((issue) => `${issue.path.join('.')}: ${issue.message}`)
      .join('; ');
    throw new Error(`invalid agent configuration: ${issues}`);
  }
  return parsed.data;
}

export function splitArgs(value: string): string[] {
  return value
    .split(/\s+/)
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
}
