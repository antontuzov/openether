/**
 * Embeddings for the agent's memory.
 *
 * The default embedder is deterministic and offline: a hashed bag of words
 * with a light stemming pass. It is not a semantic model, but it is enough to
 * recall "the SOL stop-loss trade" from "SOL stop loss", and it keeps the
 * agent runnable without an API key. Set `OPENAI_API_KEY` to use a hosted
 * embedding model instead.
 */

export interface Embedder {
  readonly name: string;
  readonly dimensions: number;
  embed(texts: string[]): Promise<number[][]>;
}

const DIMENSIONS = 256;

export class HashEmbedder implements Embedder {
  readonly name = 'hash';
  readonly dimensions = DIMENSIONS;

  async embed(texts: string[]): Promise<number[][]> {
    return texts.map((text) => this.embedOne(text));
  }

  private embedOne(text: string): number[] {
    const vector = new Array<number>(DIMENSIONS).fill(0);
    const tokens = tokenize(text);
    for (const token of tokens) {
      const hash = fnv1a(token);
      const bucket = hash % DIMENSIONS;
      const sign = (hash >>> 31) & 1 ? 1 : -1;
      vector[bucket] = (vector[bucket] ?? 0) + sign;
    }
    return normalize(vector);
  }
}

export class OpenAiEmbedder implements Embedder {
  readonly name = 'openai';
  readonly dimensions = 1536;

  constructor(
    private readonly apiKey: string,
    private readonly model = 'text-embedding-3-small',
    private readonly baseUrl = 'https://api.openai.com/v1',
  ) {}

  async embed(texts: string[]): Promise<number[][]> {
    const response = await fetch(`${this.baseUrl}/embeddings`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${this.apiKey}`,
      },
      body: JSON.stringify({ model: this.model, input: texts }),
    });
    if (!response.ok) {
      throw new Error(`embedding request failed with ${response.status}`);
    }
    const payload = (await response.json()) as {
      data?: Array<{ embedding?: number[] }>;
    };
    return (payload.data ?? []).map((item) => normalize(item.embedding ?? []));
  }
}

export function tokenize(text: string): string[] {
  return text
    .toLowerCase()
    .replace(/[^a-z0-9%$.\-/ ]+/g, ' ')
    .split(/\s+/)
    .map((token) => token.replace(/(ing|ed|es|s)$/u, ''))
    .filter((token) => token.length > 1);
}

export function cosine(a: number[], b: number[]): number {
  const length = Math.min(a.length, b.length);
  let dot = 0;
  for (let index = 0; index < length; index += 1) {
    dot += (a[index] ?? 0) * (b[index] ?? 0);
  }
  return dot;
}

function normalize(vector: number[]): number[] {
  const norm = Math.sqrt(vector.reduce((total, value) => total + value * value, 0));
  if (norm === 0) {
    return vector;
  }
  return vector.map((value) => value / norm);
}

function fnv1a(text: string): number {
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash >>> 0;
}
