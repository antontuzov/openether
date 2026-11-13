import { describe, expect, it } from 'vitest';
import { emptyState } from '../src/lib/demo';
import { demoToPortfolio } from '../src/lib/demoView';
import { shortAddress } from '../src/lib/format';

describe('addresses', () => {
  it('keeps the head and tail of an EVM address', () => {
    expect(shortAddress('0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48')).toBe('0xA0b8…eB48');
  });

  it('keeps the head and tail of a Solana address', () => {
    expect(shortAddress('5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9')).toBe('5tzFki…uAi9');
  });

  it('leaves a short address readable', () => {
    expect(shortAddress('0xdeadbeef')).toBe('0xdeadbeef');
    expect(shortAddress('  SOL  ')).toBe('SOL');
  });
});

describe('the simulator has no wallet', () => {
  it('reports zero on-chain value, so no panel can mistake it for equity', () => {
    const portfolio = demoToPortfolio(emptyState());
    expect(portfolio.onchain_value_usd).toBe(0);
    expect(portfolio.equity_usd).toBeGreaterThan(0);
  });
});