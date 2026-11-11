import { describe, expect, it } from 'vitest';
import {
  ValidationError,
  assertAddress,
  assertCoinId,
  assertDexQuery,
  assertSymbol,
  isEvmAddress,
  isSolanaAddress,
} from '../src/validate.js';
import { findTool } from '../src/tools.js';

describe('address validation', () => {
  it('rejects a malicious evm address', () => {
    expect(isEvmAddress('0x1111111111111111111111111111111111111111')).toBe(true);
    expect(isEvmAddress('0x1111111111111111111111111111111111111111; rm -rf /')).toBe(false);
    expect(isEvmAddress('http://evil.test/steal')).toBe(false);
    expect(isEvmAddress('../../etc/passwd')).toBe(false);
    expect(() => assertAddress('ethereum', '0xnothex')).toThrow(ValidationError);
  });

  it('rejects a malicious solana address', () => {
    expect(isSolanaAddress('So11111111111111111111111111111111111111112')).toBe(true);
    expect(isSolanaAddress('0x1111111111111111111111111111111111111111')).toBe(false);
    expect(() => assertAddress('solana', 'https://evil.test')).toThrow(ValidationError);
  });

  it('keeps the chain specific format', () => {
    const evm = '0x1111111111111111111111111111111111111111';
    expect(() => assertAddress('solana', evm)).toThrow(ValidationError);
    expect(() => assertAddress('base', evm)).not.toThrow();
  });
});

describe('identifier validation', () => {
  it('rejects a coin id that is not a slug', () => {
    expect(() => assertCoinId('bitcoin')).not.toThrow();
    expect(() => assertCoinId('bitcoin/../etc')).toThrow(ValidationError);
    expect(() => assertCoinId('https://evil.test')).toThrow(ValidationError);
  });

  it('rejects a symbol with unexpected characters', () => {
    expect(() => assertSymbol('BTC')).not.toThrow();
    expect(() => assertSymbol('BTC; cat /etc/passwd')).toThrow(ValidationError);
  });

  it('rejects a dex query that looks like a url', () => {
    expect(() => assertDexQuery('SOL/USDT')).not.toThrow();
    expect(() => assertDexQuery('http://evil.test')).toThrow(ValidationError);
    expect(() => assertDexQuery(' SOL')).toThrow(ValidationError);
  });
});

describe('tool schemas', () => {
  it('rejects a malicious address before any request is made', () => {
    const tool = findTool('get_evm_balances');
    expect(tool).toBeDefined();

    const parsed = tool?.schema.safeParse({
      address: '0x1111111111111111111111111111111111111111/../../etc/passwd',
    });
    expect(parsed?.success).toBe(false);

    const good = tool?.schema.safeParse({
      address: '0x1111111111111111111111111111111111111111',
    });
    expect(good?.success).toBe(true);
  });

  it('requires at least one coin id or symbol for a price query', () => {
    const tool = findTool('get_coin_price');
    expect(tool?.schema.safeParse({}).success).toBe(false);
    expect(tool?.schema.safeParse({ ids: ['bitcoin'] }).success).toBe(true);
    expect(tool?.schema.safeParse({ symbols: ['BTC'] }).success).toBe(true);
  });

  it('bounds the top coin list', () => {
    const tool = findTool('list_top_coins');
    expect(tool?.schema.safeParse({ limit: 50 }).success).toBe(true);
    expect(tool?.schema.safeParse({ limit: 5_000 }).success).toBe(false);
  });

  it('requires an address for the liquidity tool', () => {
    const tool = findTool('get_dex_liquidity');
    expect(tool?.schema.safeParse({ chain: 'solana' }).success).toBe(false);
    expect(
      tool?.schema.safeParse({
        chain: 'solana',
        pair_address: 'So11111111111111111111111111111111111111112',
      }).success,
    ).toBe(true);
  });
});
