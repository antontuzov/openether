import { describe, expect, it } from 'vitest';
import { TokenBucketRateLimiter } from '../src/ratelimit.js';

function harness(rules: Record<string, { capacity: number; refillPerMs: number }>) {
  let now = 0;
  const waits: number[] = [];
  const limiter = new TokenBucketRateLimiter(rules, {
    now: () => now,
    sleep: async (ms: number) => {
      waits.push(ms);
      now += ms;
    },
  });
  return { limiter, waits, advance: (ms: number) => (now += ms) };
}

describe('TokenBucketRateLimiter', () => {
  it('lets a burst through up to the capacity', async () => {
    const { limiter, waits } = harness({ 'api.test': { capacity: 3, refillPerMs: 1 / 1000 } });

    await limiter.acquire('api.test');
    await limiter.acquire('api.test');
    await limiter.acquire('api.test');

    expect(waits).toHaveLength(0);
  });

  it('throttles once the bucket is empty and waits for the refill', async () => {
    const { limiter, waits } = harness({ 'api.test': { capacity: 1, refillPerMs: 1 / 1000 } });

    await limiter.acquire('api.test');
    await limiter.acquire('api.test');

    expect(waits).toHaveLength(1);
    expect(waits[0]).toBe(1000);
  });

  it('keeps separate buckets per host', async () => {
    const { limiter, waits } = harness({
      'a.test': { capacity: 1, refillPerMs: 1 / 1000 },
      'b.test': { capacity: 1, refillPerMs: 1 / 1000 },
    });

    await limiter.acquire('a.test');
    await limiter.acquire('b.test');
    await limiter.acquire('b.test');

    expect(waits).toHaveLength(1);
    expect(waits[0]).toBe(1000);
  });

  it('honours a cooldown, as used for a 429 with Retry-After', async () => {
    const { limiter, waits } = harness({ 'api.test': { capacity: 10, refillPerMs: 1 } });

    limiter.cooldown('api.test', 5_000);
    await limiter.acquire('api.test');

    expect(waits).toHaveLength(1);
    expect(waits[0]).toBe(5_000);
  });

  it('refills over time instead of blocking forever', async () => {
    const { limiter, waits, advance } = harness({
      'api.test': { capacity: 1, refillPerMs: 1 / 1000 },
    });

    await limiter.acquire('api.test');
    advance(500);
    await limiter.acquire('api.test');

    expect(waits).toHaveLength(1);
    expect(waits[0]).toBe(500);
  });
});
