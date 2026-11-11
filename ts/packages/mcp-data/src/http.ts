/**
 * The HTTP layer.
 *
 * Every outbound request goes through the same path: cache lookup, rate limit,
 * timeout, retry on 429 with the upstream `Retry-After`. Errors are short and
 * sanitised so they can be handed to the model without leaking internals.
 */

import type { TtlCache } from './cache.js';
import type { TokenBucketRateLimiter } from './ratelimit.js';
import { sanitizeText } from './sanitize.js';

export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export class HttpError extends Error {
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'HttpError';
    this.status = status;
  }
}

export interface HttpClientOptions {
  limiter: TokenBucketRateLimiter;
  cache: TtlCache;
  fetchImpl?: FetchLike;
  timeoutMs?: number;
  maxRetries?: number;
  secrets?: string[];
}

export interface RequestOptions {
  /** The host used for rate limiting and error messages. */
  host: string;
  /** How long the response may be reused. */
  ttlMs: number;
}

function parseRetryAfter(value: string | null): number | undefined {
  if (!value) {
    return undefined;
  }
  const seconds = Number.parseInt(value, 10);
  if (Number.isFinite(seconds) && seconds >= 0) {
    return Math.min(seconds * 1_000, 30_000);
  }
  const date = Date.parse(value);
  if (Number.isFinite(date)) {
    return Math.min(Math.max(0, date - Date.now()), 30_000);
  }
  return undefined;
}

function excerpt(body: string, secrets: string[]): string {
  let text = body.slice(0, 160);
  for (const secret of secrets) {
    if (secret.length >= 6 && text.includes(secret)) {
      text = text.split(secret).join('[redacted]');
    }
  }
  return sanitizeText(text, 120);
}

export class HttpClient {
  private readonly limiter: TokenBucketRateLimiter;
  private readonly cache: TtlCache;
  private readonly fetchImpl: FetchLike;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;
  private readonly secrets: string[];

  constructor(options: HttpClientOptions) {
    this.limiter = options.limiter;
    this.cache = options.cache;
    this.fetchImpl = options.fetchImpl ?? ((input, init) => fetch(input, init));
    this.timeoutMs = options.timeoutMs ?? 10_000;
    this.maxRetries = options.maxRetries ?? 2;
    this.secrets = options.secrets ?? [];
  }

  async getJson<T>(url: string, options: RequestOptions): Promise<T> {
    return this.request<T>('GET', url, undefined, options);
  }

  async postJson<T>(url: string, body: unknown, options: RequestOptions): Promise<T> {
    return this.request<T>('POST', url, body, options);
  }

  private async request<T>(
    method: 'GET' | 'POST',
    url: string,
    body: unknown,
    options: RequestOptions,
  ): Promise<T> {
    const cacheKey = method === 'GET' ? url : `${method} ${url} ${JSON.stringify(body ?? null)}`;
    return this.cache.getOrLoad<T>(cacheKey, options.ttlMs, async () => {
      let attempt = 0;
      for (;;) {
        await this.limiter.acquire(options.host);

        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), this.timeoutMs);
        let response: Response | undefined;
        let failure: unknown;
        try {
          response = await this.fetchImpl(url, {
            method,
            signal: controller.signal,
            headers:
              method === 'POST'
                ? { 'content-type': 'application/json', accept: 'application/json' }
                : { accept: 'application/json' },
            body: method === 'POST' ? JSON.stringify(body ?? {}) : undefined,
          });
        } catch (error) {
          failure = error;
        } finally {
          clearTimeout(timer);
        }

        if (!response) {
          const aborted =
            failure instanceof Error && failure.name === 'AbortError';
          if (attempt < this.maxRetries) {
            attempt += 1;
            continue;
          }
          throw new HttpError(
            aborted
              ? `upstream ${options.host} timed out`
              : `upstream ${options.host} request failed`,
          );
        }

        if (response.status === 429) {
          const retryAfter = parseRetryAfter(response.headers.get('retry-after'));
          if (attempt < this.maxRetries) {
            attempt += 1;
            this.limiter.cooldown(options.host, retryAfter ?? 1_000);
            continue;
          }
          throw new HttpError(`upstream ${options.host} rate limited`, 429);
        }

        if (!response.ok) {
          const text = await response.text().catch(() => '');
          const detail = excerpt(text, this.secrets);
          throw new HttpError(
            `upstream ${options.host} returned HTTP ${response.status}${detail ? `: ${detail}` : ''}`,
            response.status,
          );
        }

        const text = await response.text();
        try {
          return JSON.parse(text) as T;
        } catch {
          throw new HttpError(`upstream ${options.host} returned malformed JSON`);
        }
      }
    });
  }
}
