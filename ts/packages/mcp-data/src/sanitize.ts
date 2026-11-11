/**
 * Output sanitisation for untrusted upstream data.
 *
 * Everything this server returns is produced by a third party (a coin listing,
 * a DEX pair, an RPC node) and is fed straight into an LLM prompt. The
 * patterns below mirror `ts/packages/agent/src/safety/guard.ts`: ANSI escapes
 * and control characters are stripped, prompt-injection shaped text is
 * dropped entirely, and every free-text field is truncated.
 */

const ANSI_CSI_PATTERN = /\u001B\[[0-9;?]*[A-Za-z]/g;
const ANSI_OSC_PATTERN = /\u001B\][^\u0007\u001B]*(?:\u0007|\u001B\\)/g;
const CONTROL_PATTERN = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g;
const LINE_BREAK_PATTERN = /[\t\r\n]+/g;

export interface InjectionRule {
  pattern: RegExp;
  label: string;
}

export const INJECTION_RULES: InjectionRule[] = [
  { pattern: /ignore\s+(all\s+)?(previous|prior|above|earlier)\s+instructions/i, label: 'instruction override' },
  { pattern: /disregard\s+(the\s+)?(system|previous|prior)\s+(prompt|instructions?)/i, label: 'system prompt override' },
  { pattern: /you\s+are\s+now\s+/i, label: 'persona replacement' },
  { pattern: /<\/?\s*(system|assistant|instructions?)\s*>/i, label: 'role tag injection' },
  { pattern: /\bnew\s+instructions?\s*:/i, label: 'instruction injection' },
  { pattern: /\b(execute_trade|transfer|withdraw)\b.{0,40}\bwithout\b.{0,20}\b(ask|confirm|approval)/i, label: 'approval bypass' },
  { pattern: /\b(system|developer|assistant)\s*(prompt|message)\s*[:=]/i, label: 'system prompt override' },
  { pattern: /```/, label: 'markdown fence injection' },
];

export const FILTERED_MARKER = '[filtered: possible prompt injection]';

export interface SanitizeOptions {
  /** Maximum length of a free-text field. */
  maxTextChars?: number;
  /** Maximum number of entries kept in an array. */
  maxArrayLength?: number;
  /** Maximum nesting depth kept. */
  maxDepth?: number;
  /** Secrets that must never appear in the output (API keys and the like). */
  secrets?: string[];
}

export interface SanitizeResult {
  value: unknown;
  findings: string[];
}

const DANGEROUS_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

function redactSecrets(value: string, secrets: string[]): string {
  let output = value;
  for (const secret of secrets) {
    if (secret && secret.length >= 6 && output.includes(secret)) {
      output = output.split(secret).join('[redacted]');
    }
  }
  return output;
}

/** Strips ANSI escapes, control characters and line breaks. */
export function stripControl(input: string): string {
  return input
    .replace(ANSI_OSC_PATTERN, '')
    .replace(ANSI_CSI_PATTERN, '')
    .replace(CONTROL_PATTERN, ' ')
    .replace(LINE_BREAK_PATTERN, ' ')
    .trim();
}

/** True when the text looks like it is trying to steer the model. */
export function looksLikeInjection(input: string): InjectionRule | undefined {
  return INJECTION_RULES.find((rule) => rule.pattern.test(input));
}

/**
 * Cleans a single free-text field. Anything that looks like an instruction is
 * replaced wholesale rather than passed through with holes in it.
 */
export function sanitizeText(input: unknown, maxChars = 200): string {
  if (typeof input !== 'string') {
    return '';
  }
  const redacted = redactSecrets(input, currentSecrets);
  const cleaned = stripControl(redacted);
  if (!cleaned) {
    return '';
  }
  if (looksLikeInjection(cleaned)) {
    return FILTERED_MARKER;
  }
  if (cleaned.length > maxChars) {
    return `${cleaned.slice(0, maxChars)}…[truncated]`;
  }
  return cleaned;
}

/** Keeps only http(s) URLs; anything else is dropped. */
export function sanitizeUrl(input: unknown, maxChars = 300): string | null {
  if (typeof input !== 'string') {
    return null;
  }
  const cleaned = stripControl(redactSecrets(input, currentSecrets));
  if (!cleaned || cleaned.includes(FILTERED_MARKER) || looksLikeInjection(cleaned)) {
    return null;
  }
  try {
    const url = new URL(cleaned);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      return null;
    }
    return url.toString().slice(0, maxChars);
  } catch {
    return null;
  }
}

// The sanitiser is used both inside the tool handlers and at the very end,
// before the payload is serialised for the model.
let currentSecrets: string[] = [];

/** Sets the secrets that must be redacted from every sanitised string. */
export function setSanitizerSecrets(secrets: string[]): void {
  currentSecrets = secrets.filter((secret) => secret.length > 0);
}

interface CleanOptions {
  maxTextChars: number;
  maxArrayLength: number;
  maxDepth: number;
  secrets: string[];
}

function clean(
  input: unknown,
  options: CleanOptions,
  findings: string[],
  depth: number,
): unknown {
  if (input === null || input === undefined) {
    return null;
  }
  if (typeof input === 'boolean') {
    return input;
  }
  if (typeof input === 'number') {
    return Number.isFinite(input) ? input : null;
  }
  if (typeof input === 'string') {
    const redacted = redactSecrets(input, options.secrets);
    const cleaned = stripControl(redacted);
    if (cleaned !== redacted.trim()) {
      findings.push('control characters stripped');
    }
    if (looksLikeInjection(cleaned)) {
      findings.push('prompt injection filtered');
      return FILTERED_MARKER;
    }
    if (cleaned.length > options.maxTextChars) {
      findings.push('text truncated');
      return `${cleaned.slice(0, options.maxTextChars)}…[truncated]`;
    }
    return cleaned;
  }
  if (Array.isArray(input)) {
    if (depth >= options.maxDepth) {
      findings.push('nesting truncated');
      return [];
    }
    const sliced = input.slice(0, options.maxArrayLength);
    if (sliced.length < input.length) {
      findings.push('array truncated');
    }
    return sliced.map((item) => clean(item, options, findings, depth + 1));
  }
  if (typeof input === 'object') {
    if (depth >= options.maxDepth) {
      findings.push('nesting truncated');
      return {};
    }
    const output: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(input as Record<string, unknown>)) {
      if (DANGEROUS_KEYS.has(key)) {
        continue;
      }
      output[key] = clean(value, options, findings, depth + 1);
    }
    return output;
  }
  return null;
}

/**
 * Deep-cleans a tool payload. Every string is stripped, screened and
 * truncated; every number must be finite; arrays and objects are bounded.
 */
export function sanitizeOutput(payload: unknown, options: SanitizeOptions = {}): SanitizeResult {
  const resolved: CleanOptions = {
    maxTextChars: options.maxTextChars ?? 200,
    maxArrayLength: options.maxArrayLength ?? 100,
    maxDepth: options.maxDepth ?? 6,
    secrets: options.secrets ?? currentSecrets,
  };
  const findings: string[] = [];
  const value = clean(payload, resolved, findings, 0);
  return { value, findings: [...new Set(findings)] };
}
