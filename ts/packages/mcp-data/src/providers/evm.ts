/**
 * EVM chain access: native balances, ERC-20 holdings and transaction history.
 *
 * Balances come from a plain JSON-RPC node (no API key needed); transaction
 * history comes from Etherscan, which needs a key for the `txlist` action.
 */

import {
  decodeString,
  decodeUint,
  decodeUint8,
  encodeAddressCall,
  encodeCall,
  SELECTORS,
  weiToDecimalString,
  weiToNative,
} from '../abi.js';
import type { ToolContext } from '../context.js';
import { ETHERSCAN_HOST, hostOf, rpcUrlFor } from '../config.js';
import { HttpError } from '../http.js';
import { sanitizeText } from '../sanitize.js';
import type { EvmChainId } from '../validate.js';

interface RpcEnvelope<T> {
  result?: T;
  error?: { code?: number; message?: string };
}

export interface EvmTokenBalance {
  address: string;
  balance_raw: string;
  balance: number | null;
  decimals: number | null;
  symbol: string | null;
  name: string | null;
}

export interface EvmTx {
  hash: string;
  block: number | null;
  timestamp: number | null;
  from: string;
  to: string | null;
  value_wei: string;
  value_native: number | null;
  gas_used: number | null;
  gas_price_wei: string | null;
  success: boolean | null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

export async function evmRpc<T>(
  ctx: ToolContext,
  chain: EvmChainId,
  method: string,
  params: unknown[],
  env: NodeJS.ProcessEnv = process.env,
): Promise<T> {
  const url = rpcUrlFor(ctx.config, chain, env);
  const body = { jsonrpc: '2.0', id: 1, method, params };
  const envelope = await ctx.http.postJson<RpcEnvelope<T>>(url, body, {
    host: hostOf(url),
    ttlMs: ctx.config.cacheTtlMs.onchain,
  });
  if (envelope.error) {
    const message = sanitizeText(envelope.error.message ?? 'request failed', 120);
    throw new HttpError(`rpc error on ${chain}: ${message}`);
  }
  if (envelope.result === undefined) {
    throw new HttpError(`rpc on ${chain} returned no result`);
  }
  return envelope.result;
}

export async function getNativeBalance(
  ctx: ToolContext,
  chain: EvmChainId,
  address: string,
): Promise<bigint> {
  const result = await evmRpc<string>(ctx, chain, 'eth_getBalance', [address, 'latest']);
  return decodeUint(result) ?? 0n;
}

async function ethCall(
  ctx: ToolContext,
  chain: EvmChainId,
  to: string,
  data: string,
): Promise<string> {
  const result = await evmRpc<string>(ctx, chain, 'eth_call', [
    { to, data },
    'latest',
  ]);
  return typeof result === 'string' ? result : '0x';
}

export async function getTokenBalance(
  ctx: ToolContext,
  chain: EvmChainId,
  holder: string,
  token: string,
): Promise<EvmTokenBalance> {
  const balanceHex = await ethCall(ctx, chain, token, encodeAddressCall(SELECTORS.balanceOf, holder));
  const raw = decodeUint(balanceHex);

  let decimals: number | null = null;
  let symbol: string | null = null;
  let name: string | null = null;

  try {
    decimals = decodeUint8(await ethCall(ctx, chain, token, encodeCall(SELECTORS.decimals)));
  } catch {
    decimals = null;
  }
  try {
    const rawSymbol = await ethCall(ctx, chain, token, encodeCall(SELECTORS.symbol));
    symbol = rawSymbol ? sanitizeText(decodeString(rawSymbol) ?? '', 20) || null : null;
  } catch {
    symbol = null;
  }
  try {
    const rawName = await ethCall(ctx, chain, token, encodeCall(SELECTORS.name));
    name = rawName ? sanitizeText(decodeString(rawName) ?? '', 60) || null : null;
  } catch {
    name = null;
  }

  const balance = raw === null ? null : weiToNative(raw, decimals ?? 18);
  return {
    address: token,
    balance_raw: raw === null ? '0' : raw.toString(),
    balance,
    decimals,
    symbol,
    name,
  };
}

export async function getTokenMetadata(
  ctx: ToolContext,
  chain: EvmChainId,
  token: string,
): Promise<{ decimals: number | null; symbol: string | null; name: string | null }> {
  let decimals: number | null = null;
  let symbol: string | null = null;
  let name: string | null = null;

  try {
    decimals = decodeUint8(await ethCall(ctx, chain, token, encodeCall(SELECTORS.decimals)));
  } catch {
    decimals = null;
  }
  try {
    const rawSymbol = await ethCall(ctx, chain, token, encodeCall(SELECTORS.symbol));
    symbol = rawSymbol ? sanitizeText(decodeString(rawSymbol) ?? '', 20) || null : null;
  } catch {
    symbol = null;
  }
  try {
    const rawName = await ethCall(ctx, chain, token, encodeCall(SELECTORS.name));
    name = rawName ? sanitizeText(decodeString(rawName) ?? '', 60) || null : null;
  } catch {
    name = null;
  }

  return { decimals, symbol, name };
}

export async function getTxHistory(
  ctx: ToolContext,
  chain: EvmChainId,
  address: string,
  limit: number,
): Promise<EvmTx[]> {
  if (!ctx.config.etherscanApiKey) {
    throw new HttpError('etherscan api key is not configured (set ETHERSCAN_API_KEY)');
  }
  const chainId = ctx.config.chains[chain].explorerChainId ?? 1;
  const url =
    `${ctx.config.etherscanBaseUrl}?chainid=${chainId}` +
    '&module=account' +
    '&action=txlist' +
    `&address=${encodeURIComponent(address)}` +
    '&page=1' +
    `&offset=${limit}` +
    '&sort=desc' +
    `&apikey=${encodeURIComponent(ctx.config.etherscanApiKey)}`;

  const raw = await ctx.http.getJson<unknown>(url, {
    host: ETHERSCAN_HOST,
    ttlMs: ctx.config.cacheTtlMs.txHistory,
  });
  const response = asRecord(raw);
  const status = response.status;
  const result = response.result;

  if (!Array.isArray(result)) {
    const message = sanitizeText(asRecord(response.result).message ?? 'request failed', 120);
    throw new HttpError(`etherscan error: ${message || 'request failed'}`);
  }
  if (status !== '1') {
    const message = sanitizeText(asRecord(result).message ?? 'request failed', 120);
    if (/no transactions found/i.test(message)) {
      return [];
    }
    throw new HttpError(`etherscan error: ${message || 'request failed'}`);
  }

  return result.slice(0, limit).map((entry) => {
    const tx = asRecord(entry);
    const value = decodeUint(typeof tx.value === 'string' ? tx.value : '0x0') ?? 0n;
    const gasPrice = decodeUint(typeof tx.gasPrice === 'string' ? tx.gasPrice : '0x0');
    const isError = tx.isError === '0' || tx.isError === '0x0' ? false : tx.isError === '1' ? true : null;
    return {
      hash: sanitizeText(tx.hash, 80),
      block: num(tx.blockNumber),
      timestamp: num(tx.timeStamp),
      from: sanitizeText(tx.from, 60),
      to: typeof tx.to === 'string' ? sanitizeText(tx.to, 60) : null,
      value_wei: value.toString(),
      value_native: weiToNative(value),
      gas_used: num(tx.gasUsed),
      gas_price_wei: gasPrice === null ? null : gasPrice.toString(),
      success: isError,
    };
  });
}

export function nativeBalancePayload(wei: bigint): {
  balance_wei: string;
  balance_native: string;
} {
  return {
    balance_wei: wei.toString(),
    balance_native: weiToDecimalString(wei),
  };
}
