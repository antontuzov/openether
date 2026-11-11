import { describe, expect, it } from 'vitest';
import { TtlCache } from '../src/cache.js';

describe('TtlCache', () => {
  it('serves a value until the ttl expires', () => {
    let now = 0;
    const cache = new TtlCache({ now: () => now });

    cache.set('key', { value: 1 }, 1_000);
    expect(cache.get('key')).toEqual({ value: 1 });

    now = 999;
    expect(cache.get('key')).toEqual({ value: 1 });

    now = 1_000;
    expect(cache.get('key')).toBeUndefined();
  });

  it('runs the loader once and caches the result', async () => {
    let calls = 0;
    const cache = new TtlCache();

    const first = await cache.getOrLoad('key', 1_000, async () => {
      calls += 1;
      return 'value';
    });
    const second = await cache.getOrLoad('key', 1_000, async () => {
      calls += 1;
      return 'other';
    });

    expect(first).toBe('value');
    expect(second).toBe('value');
    expect(calls).toBe(1);
  });

  it('collapses concurrent loads of the same key', async () => {
    let calls = 0;
    const cache = new TtlCache();

    const loader = async (): Promise<string> => {
      calls += 1;
      await new Promise((resolve) => setTimeout(resolve, 5));
      return 'value';
    };

    const results = await Promise.all([
      cache.getOrLoad('key', 1_000, loader),
      cache.getOrLoad('key', 1_000, loader),
      cache.getOrLoad('key', 1_000, loader),
    ]);

    expect(results).toEqual(['value', 'value', 'value']);
    expect(calls).toBe(1);
  });

  it('does not cache a failed load', async () => {
    let calls = 0;
    const cache = new TtlCache();

    await expect(
      cache.getOrLoad('key', 1_000, async () => {
        calls += 1;
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');

    const value = await cache.getOrLoad('key', 1_000, async () => {
      calls += 1;
      return 'recovered';
    });

    expect(value).toBe('recovered');
    expect(calls).toBe(2);
  });

  it('drops expired entries when pruned', () => {
    let now = 0;
    const cache = new TtlCache({ now: () => now, maxEntries: 2 });

    cache.set('a', 1, 100);
    cache.set('b', 2, 100);
    now = 200;

    expect(cache.prune()).toBe(2);
    expect(cache.size).toBe(0);
  });
});
