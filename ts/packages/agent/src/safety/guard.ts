/**
 * Guard for untrusted tool output.
 *
 * Anything that comes back from a tool is data produced by an external system.
 * It can contain text crafted to steer the model, so it is stripped of control
 * characters, screened for injection patterns, and truncated before it ever
 * reaches the prompt.
 */

const INJECTION_PATTERNS: Array<{ pattern: RegExp; label: string }> = [
  { pattern: /ignore\s+(all\s+)?(previous|prior|above|earlier)\s+instructions/gi, label: 'instruction override' },
  { pattern: /disregard\s+(the\s+)?(system|previous|prior)\s+(prompt|instructions?)/gi, label: 'system prompt override' },
  { pattern: /you\s+are\s+now\s+/gi, label: 'persona replacement' },
  { pattern: /<\/?\s*(system|assistant|instructions?)\s*>/gi, label: 'role tag injection' },
  { pattern: /\bnew\s+instructions?\s*:/gi, label: 'instruction injection' },
  { pattern: /\b(execute_trade|transfer|withdraw)\b.{0,40}\bwithout\b.{0,20}\b(ask|confirm|approval)/gi, label: 'approval bypass' },
];

const ADDRESS_PATTERN = /0x[a-fA-F0-9]{40}/g;

export interface GuardOptions {
  maxChars?: number;
  contractAllowlist?: string[];
}

export interface GuardResult {
  value: unknown;
  findings: string[];
}

export function sanitizeToolOutput(payload: unknown, options: GuardOptions = {}): GuardResult {
  const maxChars = options.maxChars ?? 20_000;
  const allowlist = new Set((options.contractAllowlist ?? []).map((item) => item.toLowerCase()));
  const findings: string[] = [];

  const clean = (input: unknown): unknown => {
    if (typeof input === 'string') {
      return cleanString(input, maxChars, allowlist, findings);
    }
    if (Array.isArray(input)) {
      return input.map(clean);
    }
    if (input && typeof input === 'object') {
      const output: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(input as Record<string, unknown>)) {
        output[key] = clean(value);
      }
      return output;
    }
    return input;
  };

  let value = clean(payload);
  if (typeof value === 'string') {
    value = value.slice(0, maxChars);
  }

  return { value, findings: [...new Set(findings)] };
}

function cleanString(
  input: string,
  maxChars: number,
  allowlist: Set<string>,
  findings: string[],
): string {
  // Strip ANSI escape sequences and control characters.
  let output = input
    .replace(/\u001B\[[0-9;]*[A-Za-z]/g, '')
    .replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, ' ');

  for (const { pattern, label } of INJECTION_PATTERNS) {
    const replaced = output.replace(pattern, '[filtered: possible prompt injection]');
    if (replaced !== output) {
      findings.push(label);
      output = replaced;
    }
  }

  if (ADDRESS_PATTERN.test(output)) {
    ADDRESS_PATTERN.lastIndex = 0;
    output = output.replace(ADDRESS_PATTERN, (address) => {
      if (allowlist.has(address.toLowerCase())) {
        return address;
      }
      findings.push('unlisted contract address');
      return '0x…redacted';
    });
  }
  ADDRESS_PATTERN.lastIndex = 0;

  if (output.length > maxChars) {
    findings.push('output truncated');
    output = `${output.slice(0, maxChars)}…[truncated]`;
  }
  return output;
}

/** Validates the arguments an LLM wants to send to a tool before they run. */
export function validateToolArguments(
  name: string,
  args: Record<string, unknown>,
): { ok: true } | { ok: false; error: string } {
  if (name === 'execute_trade') {
    const qty = args['qty'];
    if (typeof qty !== 'number' || !Number.isFinite(qty) || qty <= 0) {
      return { ok: false, error: 'execute_trade needs a positive finite qty' };
    }
    const side = args['side'];
    if (side !== 'buy' && side !== 'sell') {
      return { ok: false, error: 'execute_trade needs side "buy" or "sell"' };
    }
    const symbol = args['symbol'];
    if (typeof symbol !== 'string' || symbol.trim() === '') {
      return { ok: false, error: 'execute_trade needs a symbol' };
    }
    const orderType = args['order_type'];
    if (
      orderType !== undefined &&
      orderType !== 'market' &&
      orderType !== 'limit' &&
      orderType !== 'stop' &&
      orderType !== 'take_profit'
    ) {
      return { ok: false, error: 'execute_trade order_type must be market, limit, stop or take_profit' };
    }
    if (orderType === 'stop' || orderType === 'take_profit') {
      const trigger = args['trigger_price'];
      if (typeof trigger !== 'number' || !Number.isFinite(trigger) || trigger <= 0) {
        return { ok: false, error: `${orderType} needs a positive finite trigger_price` };
      }
    }
  }
  if (name === 'cancel_order') {
    const orderId = args['order_id'];
    if (typeof orderId !== 'string' || orderId.trim() === '') {
      return { ok: false, error: 'cancel_order needs the order_id of a resting order' };
    }
  }
  return { ok: true };
}
