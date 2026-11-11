/**
 * The tool surface exposed to the agent.
 *
 * Every tool validates its input with zod, calls only the upstream providers
 * in `src/providers`, and returns a compact JSON payload. There is no generic
 * "fetch this URL" tool: URLs are always built from validated parts.
 */

import { z } from 'zod';
import type { ToolContext } from './context.js';
import { ValidationError } from './validate.js';
import {
  CURRENCIES,
  CHAIN_IDS,
  EVM_CHAIN_IDS,
  COIN_ID_PATTERN,
  DEX_QUERY_PATTERN,
  EVM_ADDRESS_PATTERN,
  SOLANA_ADDRESS_PATTERN,
  SYMBOL_PATTERN,
  assertAddress,
  assertCoinId,
  assertDexQuery,
  assertSymbol,
  type ChainId,
  type EvmChainId,
} from './validate.js';
import {
  fetchCoinDetails,
  fetchSimplePrices,
  listTopCoins,
  resolveIds,
  resolveSymbols,
} from './providers/coingecko.js';
import { fetchPair, fetchTokenPairs, searchPairs } from './providers/dexscreener.js';
import { getMintInfo, getSolBalance, getSlot, getTokenAccounts } from './providers/solana.js';
import {
  getNativeBalance,
  getTokenBalance,
  getTokenMetadata,
  getTxHistory,
  nativeBalancePayload,
} from './providers/evm.js';

export interface ToolDefinition {
  name: string;
  description: string;
  schema: z.ZodType;
  handler: (ctx: ToolContext, args: Record<string, unknown>) => Promise<unknown>;
}

const coinIdSchema = z
  .string()
  .regex(COIN_ID_PATTERN, 'a coin id must match ^[a-z0-9-]{1,64}$')
  .describe('CoinGecko coin id, for example "bitcoin"');

const symbolSchema = z
  .string()
  .regex(SYMBOL_PATTERN, 'a symbol must match ^[A-Za-z0-9]{1,20}$')
  .describe('Ticker symbol, for example "BTC"');

const currencySchema = z
  .enum(CURRENCIES)
  .default('usd')
  .describe('Quote currency, defaults to usd');

const evmAddressSchema = z
  .string()
  .regex(EVM_ADDRESS_PATTERN, 'an EVM address must match ^0x[a-fA-F0-9]{40}$')
  .describe('0x prefixed 20 byte EVM address');

const solanaAddressSchema = z
  .string()
  .regex(SOLANA_ADDRESS_PATTERN, 'a Solana address must be 32-44 base58 characters')
  .describe('Base58 Solana wallet address');

const chainSchema = z.enum(CHAIN_IDS).describe('Blockchain to query');

const evmChainSchema = z.enum(EVM_CHAIN_IDS).default('ethereum').describe('EVM chain to query');

export const TOOLS: ToolDefinition[] = [
  {
    name: 'list_top_coins',
    description:
      'Top coins ranked by market cap with price, market cap, 24h volume, 24h high/low and the 24h change. Use it to see what the market is doing right now.',
    schema: z.object({
      limit: z
        .number()
        .int()
        .min(1)
        .max(250)
        .default(50)
        .describe('How many coins to return, 1-250 (default 50)'),
      vs_currency: currencySchema,
      page: z
        .number()
        .int()
        .min(1)
        .max(10)
        .default(1)
        .describe('Page of the market cap ranking (default 1)'),
    }),
    handler: async (ctx, args) => {
      const coins = await listTopCoins(ctx, {
        limit: args['limit'] as number,
        vsCurrency: args['vs_currency'] as string,
        page: args['page'] as number,
      });
      return { vs_currency: args['vs_currency'], page: args['page'], count: coins.length, coins };
    },
  },
  {
    name: 'get_coin_price',
    description:
      'Current price of one or more coins, given CoinGecko ids ("bitcoin") or ticker symbols ("BTC"). Returns price, market cap, 24h volume and the 24h change.',
    schema: z
      .object({
        ids: z
          .array(coinIdSchema)
          .max(50)
          .optional()
          .describe('Coin ids to price'),
        symbols: z
          .array(symbolSchema)
          .max(50)
          .optional()
          .describe('Ticker symbols to price'),
        vs_currency: currencySchema,
      })
      .refine((value) => (value.ids?.length ?? 0) + (value.symbols?.length ?? 0) > 0, {
        message: 'provide at least one coin id or symbol',
      }),
    handler: async (ctx, args) => {
      const ids = (args['ids'] as string[] | undefined) ?? [];
      const symbols = (args['symbols'] as string[] | undefined) ?? [];
      const vsCurrency = args['vs_currency'] as string;

      const resolved: string[] = [];
      for (const id of ids) {
        assertCoinId(id);
        if (!resolved.includes(id)) {
          resolved.push(id);
        }
      }
      if (symbols.length > 0) {
        const mapping = await resolveSymbols(ctx, symbols, vsCurrency);
        for (const symbol of symbols) {
          assertSymbol(symbol);
          const id = mapping[symbol.toUpperCase()];
          if (id && !resolved.includes(id)) {
            resolved.push(id);
          }
        }
      }
      if (resolved.length === 0) {
        throw new ValidationError('none of the requested symbols could be resolved to a coin id');
      }

      const [prices, tickerMap] = await Promise.all([
        fetchSimplePrices(ctx, resolved, vsCurrency),
        resolveIds(ctx, resolved, vsCurrency),
      ]);

      const coins = resolved.map((id) => ({
        id,
        symbol: tickerMap[id] ?? null,
        price: prices[id]?.price ?? null,
        market_cap: prices[id]?.market_cap ?? null,
        volume_24h: prices[id]?.volume_24h ?? null,
        change_24h_pct: prices[id]?.change_24h_pct ?? null,
      }));
      return { vs_currency: vsCurrency, count: coins.length, coins };
    },
  },
  {
    name: 'get_coin_details',
    description:
      'Full metadata for a single coin: name, symbol, description, categories, supply figures, all time high/low and the current price.',
    schema: z.object({ id: coinIdSchema }),
    handler: async (ctx, args) => {
      const id = assertCoinId(args['id'] as string);
      const details = await fetchCoinDetails(ctx, id);
      if (!details) {
        throw new ValidationError(`no coin found for id "${id}"`);
      }
      return { coin: details };
    },
  },
  {
    name: 'search_dex_pairs',
    description:
      'Search DEX pairs by token symbol, name or address across the supported chains. Returns the price in USD, the liquidity in USD, the 24h volume and the 24h price change per pair.',
    schema: z.object({
      query: z
        .string()
        .regex(DEX_QUERY_PATTERN, 'a query must match ^[A-Za-z0-9 ._/-]{1,64}$')
        .describe('Search term, for example "SOL" or a token address'),
      limit: z
        .number()
        .int()
        .min(1)
        .max(50)
        .default(20)
        .describe('How many pairs to return (default 20)'),
    }),
    handler: async (ctx, args) => {
      const query = assertDexQuery(args['query'] as string);
      const pairs = await searchPairs(ctx, query, args['limit'] as number);
      return { query, count: pairs.length, pairs };
    },
  },
  {
    name: 'get_dex_liquidity',
    description:
      'Liquidity detail for a single DEX pair or token: the price in USD, the liquidity in USD, the 24h volume, the buy/sell counts and the venues the token trades on.',
    schema: z
      .object({
        chain: chainSchema,
        pair_address: z
          .string()
          .max(64)
          .optional()
          .describe('DEX pair address on the given chain'),
        token_address: z
          .string()
          .max(64)
          .optional()
          .describe('Token contract or mint address on the given chain'),
      })
      .refine((value) => Boolean(value.pair_address ?? value.token_address), {
        message: 'provide a pair_address or a token_address',
      }),
    handler: async (ctx, args) => {
      const chain = args['chain'] as ChainId;
      const pairAddress = args['pair_address'] as string | undefined;
      const tokenAddress = args['token_address'] as string | undefined;

      if (pairAddress) {
        assertAddress(chain, pairAddress, 'pair_address');
        const pair = await fetchPair(ctx, chain, pairAddress);
        return { chain, pair };
      }

      assertAddress(chain, tokenAddress as string, 'token_address');
      const pairs = await fetchTokenPairs(ctx, chain, tokenAddress as string);
      return { chain, count: pairs.length, pairs: pairs.slice(0, 10) };
    },
  },
  {
    name: 'get_solana_balances',
    description:
      'SOL balance and the SPL token holdings of a Solana wallet, with the mint, the raw amount and the decimals of every token.',
    schema: z.object({
      address: solanaAddressSchema,
      include_tokens: z
        .boolean()
        .default(true)
        .describe('Also read the SPL token accounts (default true)'),
      token_limit: z
        .number()
        .int()
        .min(0)
        .max(25)
        .default(10)
        .describe('How many tokens to report, largest first (default 10)'),
    }),
    handler: async (ctx, args) => {
      const address = args['address'] as string;
      const slot = await getSlot(ctx);
      const balance = await getSolBalance(ctx, address);

      const tokens = [];
      if (args['include_tokens'] === true) {
        const accounts = await getTokenAccounts(ctx, address);
        const top = accounts
          .filter((account) => account.uiAmount > 0)
          .sort((a, b) => b.uiAmount - a.uiAmount)
          .slice(0, args['token_limit'] as number);

        for (const account of top) {
          let symbol: string | null = null;
          let name: string | null = null;
          let price: number | null = null;
          let liquidity: number | null = null;
          try {
            const pairs = await fetchTokenPairs(ctx, 'solana', account.mint);
            const best = pairs[0];
            if (best) {
              symbol = best.base.symbol || null;
              name = best.base.name || null;
              price = best.price_usd;
              liquidity = best.liquidity_usd;
            }
          } catch {
            // Market metadata is a nice to have; the balance is the answer.
          }
          tokens.push({
            mint: account.mint,
            amount_raw: account.amount,
            decimals: account.decimals,
            ui_amount: account.uiAmount,
            symbol,
            name,
            price_usd: price,
            liquidity_usd: liquidity,
          });
        }
      }

      return {
        address,
        slot,
        lamports: balance.lamports,
        sol: balance.sol,
        token_count: tokens.length,
        tokens,
      };
    },
  },
  {
    name: 'get_evm_balances',
    description:
      'Native balance and the ERC-20 balances of an EVM wallet. Token balances are read on chain with balanceOf, so they need no API key.',
    schema: z.object({
      address: evmAddressSchema,
      chain: evmChainSchema,
      tokens: z
        .array(evmAddressSchema)
        .max(10)
        .default([])
        .describe('ERC-20 contracts to read, at most 10'),
    }),
    handler: async (ctx, args) => {
      const chain = args['chain'] as EvmChainId;
      const address = args['address'] as string;
      const native = await getNativeBalance(ctx, chain, address);

      const tokens = [];
      for (const token of args['tokens'] as string[]) {
        tokens.push(await getTokenBalance(ctx, chain, address, token));
      }

      return {
        chain,
        address,
        native: nativeBalancePayload(native),
        token_count: tokens.length,
        tokens,
      };
    },
  },
  {
    name: 'get_evm_tx_history',
    description:
      'Recent transactions of an EVM address from Etherscan: hash, block, timestamp, counterparty, value and gas. Needs ETHERSCAN_API_KEY to be configured.',
    schema: z.object({
      address: evmAddressSchema,
      chain: evmChainSchema,
      limit: z
        .number()
        .int()
        .min(1)
        .max(50)
        .default(10)
        .describe('How many transactions to return (default 10)'),
    }),
    handler: async (ctx, args) => {
      const chain = args['chain'] as EvmChainId;
      const address = args['address'] as string;
      const transactions = await getTxHistory(ctx, chain, address, args['limit'] as number);
      return { chain, address, count: transactions.length, transactions };
    },
  },
  {
    name: 'get_token_metadata',
    description:
      'Name, symbol and decimals of a token on a given chain, plus its price and the deepest pool it trades in. Solana mints are read from the chain, EVM tokens from the contract.',
    schema: z.object({
      chain: chainSchema,
      address: z
        .string()
        .max(64)
        .regex(/^[A-Za-z0-9]{32,64}$/, 'a token address must be 32-64 alphanumeric characters')
        .describe('Token mint or contract address on the given chain'),
    }),
    handler: async (ctx, args) => {
      const chain = args['chain'] as ChainId;
      const address = args['address'] as string;
      assertAddress(chain, address, 'address');

      let symbol: string | null = null;
      let name: string | null = null;
      let decimals: number | null = null;

      if (chain === 'solana') {
        const mint = await getMintInfo(ctx, address);
        decimals = mint.decimals;
      } else {
        const metadata = await getTokenMetadata(ctx, chain as EvmChainId, address);
        symbol = metadata.symbol;
        name = metadata.name;
        decimals = metadata.decimals;
      }

      let price: number | null = null;
      let liquidity: number | null = null;
      try {
        const pairs = await fetchTokenPairs(ctx, chain, address);
        const best = pairs[0];
        if (best) {
          symbol = symbol ?? best.base.symbol ?? null;
          name = name ?? best.base.name ?? null;
          price = best.price_usd;
          liquidity = best.liquidity_usd;
        }
      } catch {
        // Market data is optional; the on chain metadata is the answer.
      }

      return { chain, address, symbol, name, decimals, price_usd: price, liquidity_usd: liquidity };
    },
  },
];

export function findTool(name: string): ToolDefinition | undefined {
  return TOOLS.find((tool) => tool.name === name);
}
