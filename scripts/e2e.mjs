#!/usr/bin/env node
/**
 * End-to-end check against a real core process.
 *
 * The unit suites exercise the engine in-process. This drives the same money
 * path a client would: a spawned `openether-core`, HTTP calls with and without
 * the operator token, a forged oversized order that stops trading, the webhook
 * that must fire exactly once, and a restart that has to bring the book back.
 *
 *   cargo build --bin openether-core     # once
 *   node scripts/e2e.mjs
 *
 * The paper venue runs with zero volatility and zero slippage so the fills are
 * exact. Every OPENETHER_* variable in the parent environment is dropped, so a
 * developer's live-trading arming record can never leak into the run.
 */

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { createServer as createHttpServer } from 'node:http';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const TOKEN = 'operator-token-e2e';
const SYMBOL = 'SOL/USDT';
const BASE_PRICE = 142.0;
const START_CASH = 10_000.0;
const FEE_RATE = 0.0005;

function coreBinary() {
  const override = process.env.OPENETHER_CORE_BIN;
  if (override) {
    return resolve(override);
  }
  for (const profile of ['debug', 'release']) {
    const path = join(root, 'rust', 'target', profile, 'openether-core');
    if (existsSync(path)) {
      return path;
    }
  }
  return null;
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createNetServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolvePort(port));
    });
  });
}

function engineEnv({ port, db, webhook }) {
  const env = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith('OPENETHER_') || key === 'CORE_PORT' || key === 'CORE_HOST') {
      continue;
    }
    env[key] = value;
  }
  return {
    ...env,
    CORE_HOST: '127.0.0.1',
    CORE_PORT: String(port),
    OPENETHER_VENUE: 'paper',
    OPENETHER_SYMBOLS: SYMBOL,
    OPENETHER_STATE_DB: db,
    OPENETHER_ADMIN_TOKEN: TOKEN,
    OPENETHER_ALERT_WEBHOOK: webhook,
    // A still market: a fill is exactly the base price, so fees and equity can
    // be asserted to the cent.
    OPENETHER_PAPER_VOL_BPS: '0',
    OPENETHER_PAPER_SLIPPAGE_BPS: '0',
    OPENETHER_PAPER_TICK_MS: '60000',
    RUST_LOG: 'warn',
  };
}

class Core {
  constructor({ port, env }) {
    this.port = port;
    this.base = `http://127.0.0.1:${port}`;
    this.stderr = [];
    this.child = spawn(coreBinary(), [], { cwd: join(root, 'rust'), env, stdio: ['ignore', 'pipe', 'pipe'] });
    this.exited = new Promise((resolveExit) => this.child.once('exit', resolveExit));
    this.child.stdout.on('data', (chunk) => this.remember(chunk));
    this.child.stderr.on('data', (chunk) => this.remember(chunk));
  }

  remember(chunk) {
    this.stderr.push(chunk.toString());
    if (this.stderr.length > 40) {
      this.stderr.shift();
    }
  }

  async waitUntilHealthy() {
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      try {
        const response = await fetch(`${this.base}/healthz`);
        if (response.ok) {
          return response.json();
        }
      } catch {
        // Not listening yet.
      }
      await sleep(100);
    }
    throw new Error(`the core did not answer /healthz\n${this.stderr.join('')}`);
  }

  async stop() {
    if (this.child.exitCode !== null) {
      return;
    }
    this.child.kill('SIGTERM');
    const timer = setTimeout(() => this.child.kill('SIGKILL'), 5_000);
    await this.exited;
    clearTimeout(timer);
  }
}

const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));

function fail(message) {
  throw new Error(message);
}

function ok(condition, message) {
  if (!condition) {
    fail(message);
  }
}

function near(actual, expected, message, tolerance = 1e-9) {
  ok(
    typeof actual === 'number' && Math.abs(actual - expected) <= tolerance,
    `${message}: expected ${expected}, got ${actual}`,
  );
}

async function request(base, path, { method = 'GET', token, body } = {}) {
  const headers = {};
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
  }
  const response = await fetch(base + path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    // A non-JSON body is reported through `text` by the caller's assertions.
  }
  return { status: response.status, json, text };
}

async function waitFor(predicate, message, timeoutMs = 5_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) {
      return;
    }
    await sleep(50);
  }
  fail(message);
}

async function main() {
  const binary = coreBinary();
  if (!binary) {
    console.error('build the core first: cargo build --manifest-path rust/Cargo.toml --bin openether-core');
    process.exit(2);
  }

  const dir = mkdtempSync(join(tmpdir(), 'openether-e2e-'));
  const db = join(dir, 'state.db');
  const alerts = [];
  const webhook = createHttpServer((req, res) => {
    let raw = '';
    req.on('data', (chunk) => {
      raw += chunk;
    });
    req.on('end', () => {
      try {
        alerts.push(JSON.parse(raw));
      } catch {
        alerts.push({ unparsed: raw });
      }
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"ok":true}');
    });
  });
  await new Promise((resolveListen) => webhook.listen(0, '127.0.0.1', resolveListen));
  const webhookUrl = `http://127.0.0.1:${webhook.address().port}/hook`;

  const port = await freePort();
  const env = engineEnv({ port, db, webhook: webhookUrl });
  let core = new Core({ port, env });
  let steps = 0;

  const step = (name) => {
    steps += 1;
    console.log(`ok ${steps} — ${name}`);
  };

  try {
    const health = await core.waitUntilHealthy();
    ok(health.status === 'ok', `healthz reported ${JSON.stringify(health)}`);
    ok(health.venue === 'paper', `the harness must run paper, got ${health.venue}`);
    ok(health.live_trading === false, 'live trading must be off');
    step('the core starts in paper mode and answers /healthz');

    const config = await request(core.base, '/api/config');
    ok(config.status === 200, `config answered ${config.status}`);
    ok(config.json.admin_auth === 'required', `admin_auth is ${config.json.admin_auth}`);
    step('GET /api/config reports that the operator token is required');

    const risk = await request(core.base, '/api/risk');
    ok(risk.status === 200 && risk.json.breaker_state === 'closed', 'the breaker must start closed');
    step('GET /api/risk stays public and the breaker starts closed');

    const unauthorized = await request(core.base, '/api/orders', {
      method: 'POST',
      body: { venue: 'paper', symbol: SYMBOL, side: 'buy', qty: 1 },
    });
    ok(unauthorized.status === 401, `an untokened order answered ${unauthorized.status}`);
    ok(unauthorized.json?.code === 'admin_token_required', `code was ${unauthorized.json?.code}`);
    step('POST /api/orders without the token is refused with admin_token_required');

    const admin = await request(core.base, '/api/admin', { token: TOKEN });
    ok(admin.status === 200 && admin.json.authorized === true, 'the token must authorize');
    ok(!admin.text.includes(TOKEN), 'the admin status must never echo the token');
    step('the token authorizes without ever being echoed back');

    const buy = await request(core.base, '/api/orders', {
      method: 'POST',
      token: TOKEN,
      body: { venue: 'paper', symbol: SYMBOL, side: 'buy', qty: 2 },
    });
    ok(buy.status === 200 && buy.json.approved === true, `the buy answered ${buy.status} ${buy.text}`);
    near(buy.json.fill_price, BASE_PRICE, 'the fill must be the base price', 1e-6);
    const entryFee = 2 * BASE_PRICE * FEE_RATE;
    near(buy.json.fee, entryFee, 'the entry fee', 1e-9);
    step('a tokened 2 SOL market buy fills through the risk layer');

    const portfolio = await request(core.base, '/api/portfolio');
    ok(portfolio.json.positions.length === 1, `portfolio has ${portfolio.json.positions.length} positions`);
    const position = portfolio.json.positions[0];
    ok(position.symbol === SYMBOL, `position symbol is ${position.symbol}`);
    near(position.qty, 2, 'position qty');
    near(position.avg_entry, BASE_PRICE, 'position cost basis', 1e-6);
    near(portfolio.json.equity_usd, START_CASH - entryFee, 'equity after the entry fee', 1e-6);
    step('the portfolio shows the position and charges only the fee');

    const journal = await request(core.base, '/api/journal');
    ok(journal.json.summary.fills >= 1, 'the journal must hold the entry fill');
    ok(journal.json.fills[0]?.symbol === SYMBOL, 'the journal fill must be the SOL trade');
    near(journal.json.summary.realized_pnl_usd, 0, 'an entry books no realized PnL', 1e-9);
    step('the journal is queryable and books the entry as unrealized');

    const oversized = await request(core.base, '/api/orders', {
      method: 'POST',
      token: TOKEN,
      body: { venue: 'paper', symbol: SYMBOL, side: 'buy', qty: 20 },
    });
    ok(oversized.status === 403, `an oversized order answered ${oversized.status}`);
    ok(
      oversized.json.reason === 'position_size_exceeds_max_pct',
      `reason was ${oversized.json.reason}`,
    );
    const tripped = await request(core.base, '/api/risk');
    ok(tripped.json.breaker_state === 'open', `breaker is ${tripped.json.breaker_state}`);
    step('an order past the 20% position cap is refused and halts trading');

    await waitFor(() => alerts.some((a) => a.event === 'breaker_opened'), 'no breaker_opened alert arrived');
    const opened = alerts.find((a) => a.event === 'breaker_opened');
    ok(opened.severity === 'critical', `alert severity was ${opened.severity}`);
    ok(opened.source === 'openether', `alert source was ${opened.source}`);
    ok(opened.reason === 'position_size_exceeds_max_pct', `alert reason was ${opened.reason}`);
    ok(typeof opened.equity_usd === 'number' && opened.equity_usd > 0, 'the alert must carry equity');
    step('the webhook receives one breaker_opened payload with the risk state');

    const blocked = await request(core.base, '/api/orders', {
      method: 'POST',
      token: TOKEN,
      body: { venue: 'paper', symbol: SYMBOL, side: 'buy', qty: 1 },
    });
    ok(blocked.status === 403 && blocked.json.reason === 'breaker_open', 'the breaker must block');
    await sleep(300);
    ok(
      alerts.filter((a) => a.event === 'breaker_opened').length === 1,
      'a refusal while already open must not send a second alert',
    );
    step('an open breaker refuses every order and does not repeat the alert');

    const reset = await request(core.base, '/api/breaker/reset', {
      method: 'POST',
      token: TOKEN,
      body: { confirm: true },
    });
    ok(reset.status === 200, `the reset answered ${reset.status} ${reset.text}`);
    const afterReset = await request(core.base, '/api/risk');
    ok(afterReset.json.breaker_state === 'closed', `breaker is ${afterReset.json.breaker_state}`);
    await waitFor(() => alerts.some((a) => a.event === 'breaker_reset'), 'no breaker_reset alert arrived');
    step('a tokened reset closes the breaker and alerts it');

    await core.stop();
    core = new Core({ port, env });
    await core.waitUntilHealthy();
    const restored = await request(core.base, '/api/portfolio');
    ok(restored.json.positions.length === 1, 'the position must survive the restart');
    near(restored.json.positions[0].qty, 2, 'restored qty');
    near(restored.json.positions[0].avg_entry, BASE_PRICE, 'restored cost basis', 1e-6);
    near(restored.json.equity_usd, START_CASH - entryFee, 'restored equity', 1e-6);
    const restoredJournal = await request(core.base, '/api/journal');
    ok(restoredJournal.json.summary.fills >= 1, 'the journal must survive the restart');
    step('a restart brings back the position, its basis, equity and journal');

    const sell = await request(core.base, '/api/orders', {
      method: 'POST',
      token: TOKEN,
      body: { venue: 'paper', symbol: SYMBOL, side: 'sell', qty: 2 },
    });
    ok(sell.status === 200 && sell.json.approved === true, `the sell answered ${sell.status} ${sell.text}`);
    near(sell.json.fee, entryFee, 'the exit fee', 1e-9);
    const flat = await request(core.base, '/api/portfolio');
    ok(flat.json.positions.length === 0, 'the sell must close the position');
    near(flat.json.equity_usd, START_CASH - 2 * entryFee, 'equity after a flat round trip', 1e-6);
    step('a post-restart round trip at the same price costs exactly two fees');

    console.log(`\n${steps} end-to-end checks passed`);
  } catch (error) {
    const tail = core.stderr.join('').trim();
    if (tail) {
      error.message = `${error.message}\n--- core output ---\n${tail}`;
    }
    throw error;
  } finally {
    await core.stop().catch(() => {});
    await new Promise((resolveClose) => webhook.close(resolveClose));
    rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(`\nend-to-end check failed: ${error.message}`);
  process.exit(1);
});
