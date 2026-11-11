/**
 * Solana JSON-RPC access.
 *
 * The public endpoint rate limits aggressively, so every call is cached for a
 * short while and the operator can point the server at their own RPC.
 */

import type { ToolContext } from '../context.js';
import { hostOf, rpcUrlFor } from '../config.js';
import { HttpError } from '../http.js';
import { sanitizeText } from '../sanitize.js';

export const SPL_TOKEN_PROGRAM = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';

interface RpcEnvelope<T> {
  result?: T;
  error?: { code?: number; message?: string };
}

export interface SolanaTokenHolding {
  mint: string;
  amount_raw: string;
  decimals: number;
  ui_amount: number;
  symbol: string | null;
  name: string | null;
  price_usd: number | null;
  liquidity_usd: number | null;
}

export interface SolanaBalances {
  address: string;
  slot: number | null;
  lamports: string;
  sol: number;
  tokens: SolanaTokenHolding[];
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function toBigInt(value: string): bigint {
  try {
    return BigInt(value);
  } catch {
    return 0n;
  }
}

export async function rpc<T>(
  ctx: ToolContext,
  method: string,
  params: unknown[],
  env: NodeJS.ProcessEnv = process.env,
): Promise<T> {
  const url = rpcUrlFor(ctx.config, 'solana', env);
  const body = { jsonrpc: '2.0', id: 1, method, params };
  const envelope = await ctx.http.postJson<RpcEnvelope<T>>(url, body, {
    host: hostOf(url),
    ttlMs: ctx.config.cacheTtlMs.onchain,
  });
  if (envelope.error) {
    const message = sanitizeText(envelope.error.message ?? 'request failed', 120);
    throw new HttpError(`solana rpc error: ${message}`);
  }
  if (envelope.result === undefined) {
    throw new HttpError('solana rpc returned no result');
  }
  return envelope.result;
}

export async function getSlot(ctx: ToolContext): Promise<number | null> {
  const slot = await rpc<number>(ctx, 'getSlot', []);
  return num(slot);
}

export async function getSolBalance(
  ctx: ToolContext,
  address: string,
): Promise<{ lamports: string; sol: number }> {
  const result = await rpc<{ value?: number }>(ctx, 'getBalance', [address]);
  const lamports = num(result.value) ?? 0;
  return { lamports: String(lamports), sol: lamports / 1e9 };
}

export async function getTokenAccounts(
  ctx: ToolContext,
  address: string,
): Promise<Array<{ mint: string; amount: string; decimals: number; uiAmount: number }>> {
  const result = await rpc<{ value?: unknown[] }>(
    ctx,
    'getTokenAccountsByOwner',
    [address, { programId: SPL_TOKEN_PROGRAM }, { encoding: 'jsonParsed' }],
  );
  const accounts = Array.isArray(result.value) ? result.value : [];
  const merged = new Map<string, { mint: string; amount: bigint; decimals: number; uiAmount: number }>();

  for (const account of accounts) {
    const parsed = asRecord(asRecord(asRecord(asRecord(account).account).data).parsed);
    const infoRecord = asRecord(parsed.info);
    const mint = sanitizeText(infoRecord.mint, 80);
    if (!mint) {
      continue;
    }
    const tokenAmount = asRecord(infoRecord.tokenAmount);
    const raw = typeof tokenAmount.amount === 'string' ? tokenAmount.amount : '0';
    const decimals = num(tokenAmount.decimals) ?? 0;
    const uiAmount = num(tokenAmount.uiAmount) ?? 0;
    const amount = toBigInt(raw);
    const existing = merged.get(mint);
    const total = amount + (existing?.amount ?? 0n);
    merged.set(mint, {
      mint,
      amount: total,
      decimals,
      uiAmount: (existing?.uiAmount ?? 0) + uiAmount,
    });
  }

  return [...merged.values()].map((entry) => ({
    mint: entry.mint,
    amount: entry.amount.toString(),
    decimals: entry.decimals,
    uiAmount: entry.uiAmount,
  }));
}

export async function getMintInfo(
  ctx: ToolContext,
  mint: string,
): Promise<{ decimals: number | null; supply: string | null }> {
  const result = await rpc<{ value?: unknown }>(ctx, 'getAccountInfo', [
    mint,
    { encoding: 'jsonParsed' },
  ]);
  const value = asRecord(result.value);
  const parsed = asRecord(asRecord(value.data).parsed);
  const info = asRecord(parsed.info);
  const decimals = num(info.decimals);
  const supply = typeof info.supply === 'string' ? info.supply : null;
  return { decimals, supply };
}
