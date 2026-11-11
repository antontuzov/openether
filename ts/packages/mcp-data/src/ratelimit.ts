/**
 * Per-host token bucket rate limiting.
 *
 * Every upstream we talk to lives on a free tier, so the polite thing to do is
 * to keep a bucket per host and make sure we never burst past it. The limiter
 * is deterministic: both the clock and the sleep function are injectable so
 * the tests can drive it without waiting on real time.
 */

export interface RateLimitRule {
  /** Burst size: how many requests may be issued back to back. */
  capacity: number;
  /** How many tokens are added back per millisecond. */
  refillPerMs: number;
}

export interface RateLimiterOptions {
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
}

interface Bucket {
  tokens: number;
  updatedAt: number;
  blockedUntil: number;
}

const DEFAULT_RULE: RateLimitRule = { capacity: 10, refillPerMs: 10 };

export class TokenBucketRateLimiter {
  private readonly buckets = new Map<string, Bucket>();
  private readonly rules = new Map<string, RateLimitRule>();
  private readonly now: () => number;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(rules: Record<string, RateLimitRule> = {}, options: RateLimiterOptions = {}) {
    this.now = options.now ?? (() => Date.now());
    this.sleep =
      options.sleep ??
      ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
    for (const [host, rule] of Object.entries(rules)) {
      this.setRule(host, rule);
    }
  }

  setRule(host: string, rule: RateLimitRule): void {
    this.rules.set(host, {
      capacity: Math.max(1, rule.capacity),
      refillPerMs: Math.max(0.0001, rule.refillPerMs),
    });
  }

  getRule(host: string): RateLimitRule {
    return this.rules.get(host) ?? DEFAULT_RULE;
  }

  /** Waits until a token is available for the host and consumes it. */
  async acquire(host: string): Promise<void> {
    for (;;) {
      const waitMs = this.tryTake(host);
      if (waitMs <= 0) {
        return;
      }
      await this.sleep(waitMs);
    }
  }

  /**
   * Takes a token if one is free. Returns 0 when a token was consumed and the
   * number of milliseconds to wait otherwise.
   */
  tryTake(host: string): number {
    const rule = this.getRule(host);
    const now = this.now();
    const bucket = this.buckets.get(host) ?? {
      tokens: rule.capacity,
      updatedAt: now,
      blockedUntil: 0,
    };

    const elapsed = Math.max(0, now - bucket.updatedAt);
    bucket.tokens = Math.min(rule.capacity, bucket.tokens + elapsed * rule.refillPerMs);
    bucket.updatedAt = now;

    if (now < bucket.blockedUntil) {
      this.buckets.set(host, bucket);
      return bucket.blockedUntil - now;
    }

    if (bucket.tokens >= 1) {
      bucket.tokens -= 1;
      this.buckets.set(host, bucket);
      return 0;
    }

    this.buckets.set(host, bucket);
    const deficit = 1 - bucket.tokens;
    return Math.max(1, Math.ceil(deficit / rule.refillPerMs));
  }

  /** Blocks a host for `ms`; used to honour Retry-After on a 429. */
  cooldown(host: string, ms: number): void {
    const now = this.now();
    const bucket = this.buckets.get(host) ?? { tokens: 0, updatedAt: now, blockedUntil: 0 };
    bucket.tokens = 0;
    bucket.blockedUntil = Math.max(bucket.blockedUntil, now + Math.max(0, ms));
    this.buckets.set(host, bucket);
  }
}
