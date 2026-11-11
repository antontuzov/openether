import { appendFile, mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

export interface LogContext {
  [key: string]: unknown;
}

/**
 * Agent logger.
 *
 * Every entry is written to a local JSONL file and pushed to the core engine
 * so the dashboard shows the agent's reasoning next to the engine's events.
 */
export class AgentLogger {
  private readonly levels: Record<LogLevel, number> = {
    debug: 10,
    info: 20,
    warn: 30,
    error: 40,
  };

  constructor(
    private readonly directory: string,
    private readonly minimumLevel: LogLevel,
    private readonly coreUrl: string,
  ) {}

  async init(): Promise<void> {
    await mkdir(this.directory, { recursive: true });
  }

  async log(
    level: LogLevel,
    message: string,
    context: LogContext = {},
  ): Promise<void> {
    if (this.levels[level] < this.levels[this.minimumLevel]) {
      return;
    }
    const entry = {
      id: randomUUID(),
      ts_ms: Date.now(),
      source: 'agent',
      level,
      message,
      context,
    };

    const line = `${JSON.stringify(entry)}\n`;
    await appendFile(join(this.directory, 'agent.jsonl'), line, 'utf8').catch(() => undefined);

    const target = level === 'error' ? process.stderr : process.stdout;
    target.write(`[${level}] ${message}\n`);

    await this.pushToCore(entry).catch(() => undefined);
  }

  info(message: string, context?: LogContext): Promise<void> {
    return this.log('info', message, context);
  }

  warn(message: string, context?: LogContext): Promise<void> {
    return this.log('warn', message, context);
  }

  error(message: string, context?: LogContext): Promise<void> {
    return this.log('error', message, context);
  }

  private async pushToCore(entry: unknown): Promise<void> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 3000);
    try {
      await fetch(`${this.coreUrl}/api/logs`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(entry),
        signal: controller.signal,
      });
    } finally {
      clearTimeout(timeout);
    }
  }
}
