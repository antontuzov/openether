/**
 * Wiring of the runtime: rate limiter, cache, HTTP client and the context the
 * tool handlers work with.
 */

import { TtlCache } from './cache.js';
import { loadConfig, type ServerConfig } from './config.js';
import type { ToolContext } from './context.js';
import { HttpClient, type FetchLike } from './http.js';
import { TokenBucketRateLimiter } from './ratelimit.js';
import { setSanitizerSecrets } from './sanitize.js';

export interface RuntimeOptions {
  env?: NodeJS.ProcessEnv;
  fetchImpl?: FetchLike;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
}

export function createContext(
  config: ServerConfig = loadConfig(),
  options: RuntimeOptions = {},
): ToolContext {
  const now = options.now ?? (() => Date.now());
  const limiter = new TokenBucketRateLimiter(config.rateLimits, {
    now,
    sleep: options.sleep,
  });
  const cache = new TtlCache({ now });
  const http = new HttpClient({
    limiter,
    cache,
    fetchImpl: options.fetchImpl,
    timeoutMs: config.httpTimeoutMs,
    maxRetries: config.maxRetries,
    secrets: config.secrets,
  });
  setSanitizerSecrets(config.secrets);
  return { http, config, limiter, cache };
}
