/**
 * Server configuration.
 *
 * Every endpoint is either a fixed public API or a URL the operator sets in
 * the environment. The agent can never choose an endpoint, which is what keeps
 * the server from being an SSRF proxy.
 */

import type { RateLimitRule } from './ratelimit.js';
import { CHAIN_IDS, type ChainId } from './validate.js';

export interface ChainDescriptor {
  id: ChainId;
  kind: 'solana' | 'evm';
  /** Chain id used by the DexScreener API. */
  dexChainId: string;
  /** Chain id used by the Etherscan V2 API. */
  explorerChainId?: number;
  /** Default RPC endpoint; the operator can override it per chain. */
  defaultRpcUrl: string;
  /** Environment variable that overrides the RPC endpoint. */
  rpcEnvVar: string;
}

const DEFAULT_RPC_URLS: Record<ChainId, string> = {
  solana: 'https://api.mainnet-beta.solana.com',
  ethereum: 'https://ethereum-rpc.publicnode.com',
  bsc: 'https://bsc-rpc.publicnode.com',
  polygon: 'https://polygon-bor-rpc.publicnode.com',
  arbitrum: 'https://arbitrum-one-rpc.publicnode.com',
  optimism: 'https://optimism-rpc.publicnode.com',
  base: 'https://base-rpc.publicnode.com',
  avalanche: 'https://avalanche-c-chain-rpc.publicnode.com',
};

const RPC_ENV_VARS: Record<ChainId, string> = {
  solana: 'SOLANA_RPC_URL',
  ethereum: 'ETHEREUM_RPC_URL',
  bsc: 'BSC_RPC_URL',
  polygon: 'POLYGON_RPC_URL',
  arbitrum: 'ARBITRUM_RPC_URL',
  optimism: 'OPTIMISM_RPC_URL',
  base: 'BASE_RPC_URL',
  avalanche: 'AVALANCHE_RPC_URL',
};

const DEX_CHAIN_IDS: Record<ChainId, string> = {
  solana: 'solana',
  ethereum: 'ethereum',
  bsc: 'bsc',
  polygon: 'polygon',
  arbitrum: 'arbitrum',
  optimism: 'optimism',
  base: 'base',
  avalanche: 'avalanche',
};

const EXPLORER_CHAIN_IDS: Partial<Record<ChainId, number>> = {
  ethereum: 1,
  bsc: 56,
  polygon: 137,
  arbitrum: 42161,
  optimism: 10,
  base: 8453,
  avalanche: 43114,
};

export interface CacheTtl {
  /** CoinGecko market lists. */
  markets: number;
  /** CoinGecko coin details. */
  details: number;
  /** DexScreener pairs and tokens. */
  dex: number;
  /** On-chain balances and token metadata. */
  onchain: number;
  /** Explorer transaction history. */
  txHistory: number;
}

export interface ServerConfig {
  chains: Record<ChainId, ChainDescriptor>;
  etherscanApiKey: string;
  etherscanBaseUrl: string;
  coingeckoBaseUrl: string;
  dexscreenerBaseUrl: string;
  httpTimeoutMs: number;
  maxRetries: number;
  cacheTtlMs: CacheTtl;
  rateLimits: Record<string, RateLimitRule>;
  /** Secrets that must never appear in a tool result. */
  secrets: string[];
}

export const COINGECKO_HOST = 'api.coingecko.com';
export const DEXSCREENER_HOST = 'api.dexscreener.com';
export const ETHERSCAN_HOST = 'api.etherscan.io';

/** Free tier budgets: CoinGecko 30/min, DexScreener 60/min, Etherscan 5/s. */
export function defaultRateLimits(): Record<string, RateLimitRule> {
  return {
    [COINGECKO_HOST]: { capacity: 30, refillPerMs: 30 / 60_000 },
    [DEXSCREENER_HOST]: { capacity: 60, refillPerMs: 60 / 60_000 },
    [ETHERSCAN_HOST]: { capacity: 5, refillPerMs: 5 / 1_000 },
  };
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): ServerConfig {
  const chains = {} as Record<ChainId, ChainDescriptor>;
  for (const id of CHAIN_IDS) {
    chains[id] = {
      id,
      kind: id === 'solana' ? 'solana' : 'evm',
      dexChainId: DEX_CHAIN_IDS[id],
      explorerChainId: EXPLORER_CHAIN_IDS[id],
      defaultRpcUrl: DEFAULT_RPC_URLS[id],
      rpcEnvVar: RPC_ENV_VARS[id],
    };
  }

  const etherscanApiKey = (env.ETHERSCAN_API_KEY ?? '').trim();
  const timeout = Number.parseInt(env.MCP_DATA_HTTP_TIMEOUT_MS ?? '', 10);

  return {
    chains,
    etherscanApiKey,
    etherscanBaseUrl: 'https://api.etherscan.io/v2/api',
    coingeckoBaseUrl: 'https://api.coingecko.com/api/v3',
    dexscreenerBaseUrl: 'https://api.dexscreener.com',
    httpTimeoutMs: Number.isFinite(timeout) && timeout > 0 ? Math.min(timeout, 60_000) : 10_000,
    maxRetries: 2,
    cacheTtlMs: {
      markets: 60_000,
      details: 300_000,
      dex: 30_000,
      onchain: 15_000,
      txHistory: 60_000,
    },
    rateLimits: defaultRateLimits(),
    secrets: etherscanApiKey ? [etherscanApiKey] : [],
  };
}

/** Resolves the RPC endpoint for a chain, honouring the operator override. */
export function rpcUrlFor(config: ServerConfig, chain: ChainId, env: NodeJS.ProcessEnv = process.env): string {
  const descriptor = config.chains[chain];
  const override = (env[descriptor.rpcEnvVar] ?? '').trim();
  return override || descriptor.defaultRpcUrl;
}

/** The rate limit bucket key for an endpoint. */
export function hostOf(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return 'unknown';
  }
}
