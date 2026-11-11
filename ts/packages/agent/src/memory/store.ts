import { appendFile, mkdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { cosine, type Embedder } from './embeddings.js';

export type MemoryKind = 'trade' | 'plan' | 'observation' | 'outcome' | 'note';

export interface MemoryRecord {
  id: string;
  ts: string;
  kind: MemoryKind;
  text: string;
  tags: string[];
  embedding: number[];
}

/**
 * File-backed vector store.
 *
 * Records are appended to a JSONL file, so the memory survives restarts and can
 * be inspected by hand. Search is a cosine similarity scan: the store is meant
 * for thousands of records, not millions, and a single file keeps the agent
 * free of a database dependency.
 */
export class MemoryStore {
  private readonly records: MemoryRecord[] = [];

  private constructor(
    private readonly file: string,
    private readonly embedder: Embedder,
  ) {}

  static async open(directory: string, embedder: Embedder): Promise<MemoryStore> {
    await mkdir(directory, { recursive: true });
    const file = join(directory, 'memory.jsonl');
    const store = new MemoryStore(file, embedder);
    await store.load();
    return store;
  }

  private async load(): Promise<void> {
    let raw: string;
    try {
      raw = await readFile(this.file, 'utf8');
    } catch {
      return;
    }
    for (const line of raw.split('\n')) {
      if (!line.trim()) {
        continue;
      }
      try {
        const record = JSON.parse(line) as MemoryRecord;
        if (record && typeof record.text === 'string' && Array.isArray(record.embedding)) {
          this.records.push(record);
        }
      } catch {
        // A truncated last line must not stop the agent from starting.
      }
    }
  }

  async append(input: {
    kind: MemoryKind;
    text: string;
    tags?: string[];
  }): Promise<MemoryRecord> {
    const [embedding] = await this.embedder.embed([input.text]);
    const record: MemoryRecord = {
      id: randomUUID(),
      ts: new Date().toISOString(),
      kind: input.kind,
      text: input.text,
      tags: input.tags ?? [],
      embedding: embedding ?? [],
    };
    this.records.push(record);
    await appendFile(this.file, `${JSON.stringify(record)}\n`, 'utf8');
    return record;
  }

  async search(query: string, limit = 5): Promise<MemoryRecord[]> {
    if (this.records.length === 0) {
      return [];
    }
    const [embedding] = await this.embedder.embed([query]);
    if (!embedding) {
      return [];
    }
    return this.records
      .map((record) => ({ record, score: cosine(embedding, record.embedding) }))
      .filter((entry) => entry.score > 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, limit)
      .map((entry) => entry.record);
  }

  all(): MemoryRecord[] {
    return [...this.records];
  }

  get size(): number {
    return this.records.length;
  }
}
