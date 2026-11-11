/**
 * The shared state every tool handler works with.
 */

import type { TtlCache } from './cache.js';
import type { ServerConfig } from './config.js';
import type { HttpClient } from './http.js';
import type { TokenBucketRateLimiter } from './ratelimit.js';

export interface ToolContext {
  http: HttpClient;
  config: ServerConfig;
  limiter: TokenBucketRateLimiter;
  cache: TtlCache;
}
