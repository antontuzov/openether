/**
 * A tiny in-memory TTL cache.
 *
 * The agent polls on a fixed interval, so the same market list would be
 * fetched over and over. Caching by full URL keeps the free tiers healthy.
 * Identical in-flight requests are collapsed into a single upstream call.
 */

export interface TtlCacheOptions {
  now?: () => number;
  maxEntries?: number;
}

interface Entry {
  value: unknown;
  expiresAt: number;
}

export class TtlCache {
  private readonly values = new Map<string, Entry>();
  private readonly inflight = new Map<string, Promise<unknown>>();
  private readonly now: () => number;
  private readonly maxEntries: number;

  constructor(options: TtlCacheOptions = {}) {
    this.now = options.now ?? (() => Date.now());
    this.maxEntries = Math.max(1, options.maxEntries ?? 500);
  }

  get size(): number {
    return this.values.size;
  }

  get<T>(key: string): T | undefined {
    const entry = this.values.get(key);
    if (!entry) {
      return undefined;
    }
    if (entry.expiresAt <= this.now()) {
      this.values.delete(key);
      return undefined;
    }
    return entry.value as T;
  }

  set(key: string, value: unknown, ttlMs: number): void {
    if (ttlMs <= 0 || value === undefined) {
      return;
    }
    if (this.values.size >= this.maxEntries) {
      this.prune(true);
    }
    this.values.set(key, { value, expiresAt: this.now() + ttlMs });
  }

  delete(key: string): void {
    this.values.delete(key);
  }

  clear(): void {
    this.values.clear();
    this.inflight.clear();
  }

  /** Drops expired entries; with `aggressive` it also drops the oldest ones. */
  prune(aggressive = false): number {
    const now = this.now();
    let removed = 0;
    for (const [key, entry] of this.values) {
      if (entry.expiresAt <= now) {
        this.values.delete(key);
        removed += 1;
      }
    }
    if (aggressive) {
      while (this.values.size >= this.maxEntries) {
        const oldest = this.values.keys().next();
        if (oldest.done) {
          break;
        }
        this.values.delete(oldest.value);
        removed += 1;
      }
    }
    return removed;
  }

  /**
   * Returns the cached value, otherwise runs `loader` once and caches the
   * result. Concurrent calls for the same key share a single loader run.
   */
  async getOrLoad<T>(key: string, ttlMs: number, loader: () => Promise<T>): Promise<T> {
    const cached = this.get<T>(key);
    if (cached !== undefined) {
      return cached;
    }
    const pending = this.inflight.get(key);
    if (pending) {
      return pending as Promise<T>;
    }
    const promise = loader()
      .then((value) => {
        this.set(key, value, ttlMs);
        return value;
      })
      .finally(() => {
        this.inflight.delete(key);
      });
    this.inflight.set(key, promise);
    return promise;
  }
}
