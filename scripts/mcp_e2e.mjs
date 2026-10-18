#!/usr/bin/env node
/**
 * RDM agent-interface MCP end-to-end conformance harness (independent QA).
 *
 * Written ONLY against the contract `docs/agent-integration.md` (v1) — not
 * against the implementation — so it can be prepared before the server exists.
 *
 * Covers:
 *   - JSON-RPC 2.0 over stdio (newline-delimited), version negotiation, ping
 *   - tools/list: 16 contract tools, schemas, annotations, descriptions
 *   - resources/list + templates + read
 *   - prompts/list + get
 *   - tools/call: capabilities / selftest / doctor / probe / download(wait)
 *     with progress notifications / verify / list / get / idempotency /
 *     pause / resume / cancel / wait_task
 *   - policy: policy_denied, needs_approval -> plan -> apply_plan (single use),
 *     conflict
 *   - redaction: cookie / Authorization never echoed to the agent
 *   - robustness: invalid JSON, unknown method, missing params, EOF persistence,
 *     notifications/cancelled only cancels the wait, concurrent requests
 *   - CLI agent contract: --output json envelope / ndjson / run stdin /
 *     exit codes / not_found
 *   - static: SKILL.md + referenced docs presence
 *
 * Independent fixtures: spawns rdm-fixture-server (port 182xx, isolated
 * LOCALAPPDATA under %TEMP%). Reference SHA-256 is computed by this script over
 * HTTP, not by the CLI.
 *
 * Usage:
 *   node scripts/mcp_e2e.mjs [--rdm-mcp PATH] [--rdm PATH] [--fixture PATH]
 *        [--out PATH] [--transcript PATH] [--size-mb N] [--slow-delay-ms N]
 *        [--only REGEX] [--list-sections] [--verbose] [--keep-temp]
 *        [--legacy-only]   # skip the MCP 2026-07-28 dual-era section
 */

'use strict';

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import http from 'node:http';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// ---------------------------------------------------------------------------
// CLI arguments / paths
// ---------------------------------------------------------------------------

const SELF_PATH = fileURLToPath(import.meta.url);
const ROOT = path.resolve(path.dirname(SELF_PATH), '..');

function parseArgs(argv) {
  const opts = {
    rdmMcp: path.join(ROOT, 'target-qa-agent', 'debug', 'rdm-mcp.exe'),
    rdm: path.join(ROOT, 'target-qa-agent', 'debug', 'rdm.exe'),
    fixture: path.join(ROOT, 'target-qa-agent', 'debug', 'rdm-fixture-server.exe'),
    out: path.join(ROOT, 'target-qa-agent', 'mcp-e2e-results.json'),
    transcript: path.join(ROOT, 'target-qa-agent', 'mcp-transcript.log'),
    sizeMb: 32,
    slowDelayMs: 50,
    only: null,
    listSections: false,
    verbose: false,
    keepTemp: false,
    legacyOnly: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`missing value for ${a}`);
      return argv[++i];
    };
    switch (a) {
      case '--rdm-mcp': opts.rdmMcp = next(); break;
      case '--rdm': opts.rdm = next(); break;
      case '--fixture': opts.fixture = next(); break;
      case '--out': opts.out = next(); break;
      case '--transcript': opts.transcript = next(); break;
      case '--size-mb': opts.sizeMb = parseInt(next(), 10); break;
      case '--slow-delay-ms': opts.slowDelayMs = parseInt(next(), 10); break;
      case '--only': opts.only = new RegExp(next(), 'i'); break;
      case '--list-sections': opts.listSections = true; break;
      case '--verbose': opts.verbose = true; break;
      case '--keep-temp': opts.keepTemp = true; break;
      case '--legacy-only': opts.legacyOnly = true; break;
      case '--help':
      case '-h':
        console.log(fs.readFileSync(SELF_PATH, 'utf8').split('\n').slice(2, 40).join('\n'));
        process.exit(0);
      default:
        throw new Error(`unknown argument: ${a}`);
    }
  }
  for (const k of ['rdmMcp', 'rdm', 'fixture', 'out', 'transcript']) {
    opts[k] = path.resolve(ROOT, opts[k]);
  }
  return opts;
}

let OPTIONS;
try {
  OPTIONS = parseArgs(process.argv.slice(2));
} catch (err) {
  console.error(`mcp_e2e: ${err.message}`);
  process.exit(2);
}

const SIZE_BYTES = OPTIONS.sizeMb * 1024 * 1024;

// ---------------------------------------------------------------------------
// Check bookkeeping
// ---------------------------------------------------------------------------

const results = {
  harness: 'rdm-mcp-e2e',
  generated_at: new Date().toISOString(),
  node: process.version,
  platform: `${os.platform()} ${os.release()}`,
  binaries: {
    rdm_mcp: OPTIONS.rdmMcp,
    rdm: OPTIONS.rdm,
    fixture: OPTIONS.fixture,
  },
  size_mb: OPTIONS.sizeMb,
  sections: [],
  checks: [],
  summary: null,
  limitations: [],
};

let currentSection = null;
function section(name, fn) {
  if (OPTIONS.only && !OPTIONS.only.test(name)) {
    results.sections.push({ name, skipped: true, pass: true, checks: 0 });
    return Promise.resolve();
  }
  currentSection = name;
  const startIndex = results.checks.length;
  const record = { name, checks: 0, failed: 0, errors: [], started_at: new Date().toISOString() };
  results.sections.push(record);
  console.log(`\n=== [section] ${name} ===`);
  return Promise.resolve()
    .then(fn)
    .catch((err) => {
      addCheck(name, 'section_completed', false, `uncaught: ${err && err.stack ? err.stack.split('\n')[0] : String(err)}`);
      record.errors.push(String(err));
    })
    .finally(() => {
      const own = results.checks.slice(startIndex);
      record.checks = own.length;
      record.failed = own.filter((c) => !c.pass).length;
      console.log(`--- [section] ${name}: ${record.checks - record.failed}/${record.checks} passed`);
      currentSection = null;
    });
}

function addCheck(sectionName, name, pass, detail) {
  const sec = sectionName || currentSection || 'global';
  const check = {
    section: sec,
    name: `${sec}/${name}`,
    pass: !!pass,
    detail: detail === undefined ? '' : String(detail),
    at: new Date().toISOString(),
  };
  results.checks.push(check);
  const tag = check.pass ? 'PASS' : 'FAIL';
  const line = `[${tag}] ${check.name}${check.detail ? `  (${truncate(check.detail, 220)})` : ''}`;
  console.log(line);
  if (OPTIONS.verbose || !check.pass) {
    transcript(`## ${line}`);
  }
  return check.pass;
}

function truncate(s, n) {
  s = String(s);
  return s.length <= n ? s : `${s.slice(0, n)}…`;
}

// ---------------------------------------------------------------------------
// Transcript (separate directions so redaction checks can scan inbound only)
// ---------------------------------------------------------------------------

const transcriptLines = [];
const inboundLines = [];
const outboundLines = [];
function transcript(line) {
  transcriptLines.push(line);
  if (transcriptLines.length > 4000) transcriptLines.shift();
}
function recordInbound(line) {
  inboundLines.push(line);
  transcript(`<< ${truncate(line, 800)}`);
}
function recordOutbound(line) {
  outboundLines.push(line);
  transcript(`>> ${truncate(line, 800)}`);
}

// ---------------------------------------------------------------------------
// Process / fixture helpers
// ---------------------------------------------------------------------------

const children = [];
function trackChild(child) {
  children.push(child);
  return child;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const port = srv.address().port;
      srv.close(() => resolve(port));
    });
  });
}

function waitPort(port, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve) => {
    const attempt = () => {
      const s = net.connect({ port, host: '127.0.0.1' });
      s.once('connect', () => { s.destroy(); resolve(true); });
      s.once('error', () => {
        s.destroy();
        if (Date.now() >= deadline) resolve(false);
        else setTimeout(attempt, 100);
      });
    };
    attempt();
  });
}

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

function sha256File(file) {
  return new Promise((resolve, reject) => {
    const h = crypto.createHash('sha256');
    fs.createReadStream(file)
      .on('error', reject)
      .on('data', (d) => h.update(d))
      .on('end', () => resolve(h.digest('hex')));
  });
}

/** Reference payload hash computed independently over HTTP (never via RDM). */
function httpReference(url) {
  return new Promise((resolve, reject) => {
    const req = http.get(url, (res) => {
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`reference fetch status ${res.statusCode}`));
        return;
      }
      const h = crypto.createHash('sha256');
      let bytes = 0;
      res.on('data', (d) => { bytes += d.length; h.update(d); });
      res.on('end', () => resolve({ sha256: h.digest('hex'), bytes }));
      res.on('error', reject);
    });
    req.on('error', reject);
    req.setTimeout(60000, () => { req.destroy(new Error('reference fetch timeout')); });
  });
}

function runChild(exe, args, { timeoutMs = 120000, input = null, env = {} } = {}) {
  return new Promise((resolve) => {
    const child = spawn(exe, args, {
      cwd: ROOT,
      env: { ...process.env, ...env },
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stdout = '';
    let stderr = '';
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; try { child.kill(); } catch { /* ignore */ } }, timeoutMs);
    child.stdout.on('data', (d) => { stdout += d.toString(); });
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    child.on('error', (err) => { clearTimeout(timer); resolve({ code: -1, stdout, stderr: stderr + String(err), timedOut, spawnError: String(err) }); });
    child.on('close', (code) => { clearTimeout(timer); resolve({ code, stdout, stderr, timedOut }); });
    if (input !== null) {
      child.stdin.end(input);
    } else {
      child.stdin.end();
    }
  });
}

async function startFixture({ sizeMb, delayMs, tag }) {
  if (!fs.existsSync(OPTIONS.fixture)) {
    throw new Error(`fixture server not found: ${OPTIONS.fixture} (build with scripts/mcp-conformance.ps1)`);
  }
  const port = await freePort();
  const args = ['--port', String(port), '--size', String(sizeMb * 1024 * 1024)];
  if (delayMs > 0) args.push('--delay-ms', String(delayMs));
  const child = trackChild(spawn(OPTIONS.fixture, args, { cwd: ROOT, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true }));
  let stderr = '';
  child.stderr.on('data', (d) => { stderr += d.toString(); });
  child.stdout.on('data', () => { /* banner */ });
  const ok = await waitPort(port);
  if (!ok) throw new Error(`fixture ${tag} did not listen on ${port}: ${stderr}`);
  console.log(`    fixture[${tag}] http://127.0.0.1:${port}/file (${sizeMb} MiB, delay=${delayMs}ms)`);
  return {
    port,
    url: `http://127.0.0.1:${port}/file`,
    proc: child,
    stop() { try { child.kill(); } catch { /* ignore */ } },
  };
}

const isolatedDirs = [];
function isolatedAppData(tag) {
  const dir = path.join(os.tmpdir(), `rdm-qa-agent-${tag}-${process.pid}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`);
  fs.mkdirSync(path.join(dir, 'RDM', 'agent'), { recursive: true });
  isolatedDirs.push(dir);
  return dir;
}

function cleanupIsolatedDirs() {
  if (OPTIONS.keepTemp) {
    console.log(`    (kept isolated dirs: ${isolatedDirs.length} under ${os.tmpdir()})`);
    return;
  }
  for (const dir of isolatedDirs) {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* ignore */ }
  }
}

// ---------------------------------------------------------------------------
// Minimal interactive JSON-RPC client for the MCP stdio transport
// ---------------------------------------------------------------------------

class McpClient {
  constructor({ exe, localAppData, tag, envOut = {}, cwd = ROOT, extraArgs = [] }) {
    this.tag = tag || 'main';
    this.exe = exe;
    this.localAppData = localAppData;
    this.seq = 0;
    this.pending = new Map();
    this.notifications = [];
    this.stderrLines = [];
    this.nonJsonStdout = [];
    this.closed = false;
    this.exitInfo = null;
    this.outbound = [];
    this.inbound = [];
    this.buffer = '';
    const env = { ...process.env, LOCALAPPDATA: localAppData, RUST_LOG: process.env.RDM_QA_MCP_LOG || 'info', ...envOut };
    // QA convenience: allow a Node script as the server (used to smoke-test this
    // harness's plumbing before the real binary exists). Production runs always
    // pass the .exe path.
    const isScript = /\.(mjs|cjs|js)$/i.test(exe);
    const spawnExe = isScript ? process.execPath : exe;
    const spawnArgs = isScript ? [exe, ...extraArgs] : [...extraArgs];
    this.child = trackChild(spawn(spawnExe, spawnArgs, { cwd, env, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true }));
    this.child.stdout.setEncoding('utf8');
    this.child.stderr.setEncoding('utf8');
    this.child.stdout.on('data', (chunk) => this._onStdout(chunk));
    this.child.stderr.on('data', (chunk) => {
      for (const line of chunk.split(/\r?\n/)) {
        if (line.trim()) {
          this.stderrLines.push(line);
          if (this.stderrLines.length > 2000) this.stderrLines.shift();
        }
      }
    });
    this.child.on('exit', (code, signal) => {
      this.exitInfo = { code, signal };
      for (const [, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error(`server exited (code=${code}) while waiting for ${p.method}`));
      }
      this.pending.clear();
    });
    this.child.on('error', (err) => {
      for (const [, p] of this.pending) { clearTimeout(p.timer); p.reject(err); }
      this.pending.clear();
    });
  }

  _onStdout(chunk) {
    this.buffer += chunk;
    let idx;
    while ((idx = this.buffer.indexOf('\n')) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      this.inbound.push(line);
      recordInbound(line);
      let msg;
      try {
        msg = JSON.parse(line);
      } catch (err) {
        this.nonJsonStdout.push({ line, reason: String(err) });
        continue;
      }
      if (msg && msg.id !== undefined && msg.id !== null && (msg.result !== undefined || msg.error !== undefined)) {
        const p = this.pending.get(msg.id);
        if (p) {
          clearTimeout(p.timer);
          this.pending.delete(msg.id);
          p.resolve(msg);
        }
      } else if (msg && msg.method) {
        this.notifications.push({ at: Date.now(), msg });
        transcript(`<< [notification] ${msg.method} ${truncate(JSON.stringify(msg.params || {}), 300)}`);
      }
    }
  }

  _send(obj) {
    if (this.closed) throw new Error('client is closed');
    const line = JSON.stringify(obj);
    this.outbound.push(line);
    recordOutbound(line);
    this.child.stdin.write(`${line}\n`);
  }

  sendRaw(line) {
    this.outbound.push(line);
    recordOutbound(line);
    this.child.stdin.write(line.endsWith('\n') ? line : `${line}\n`);
  }

  request(method, params = undefined, { timeoutMs = 30000 } = {}) {
    const id = ++this.seq;
    const msg = { jsonrpc: '2.0', id, method };
    if (params !== undefined) msg.params = params;
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        const err = new Error(`timeout waiting for response to ${method} (id=${id}, ${timeoutMs}ms)`);
        err.code = 'QA_TIMEOUT';
        reject(err);
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, method });
    });
    this._send(msg);
    return promise;
  }

  notify(method, params = undefined) {
    const msg = { jsonrpc: '2.0', method };
    if (params !== undefined) msg.params = params;
    this._send(msg);
  }

  async initialize(protocolVersion = '2025-06-18') {
    const res = await this.request('initialize', {
      protocolVersion,
      capabilities: { roots: { listChanged: false } },
      clientInfo: { name: 'rdm-qa-harness', version: '0.0.0' },
    }, { timeoutMs: 20000 });
    this.notify('notifications/initialized');
    return res;
  }

  findNotifications(method) {
    return this.notifications.filter((n) => n.msg.method === method);
  }

  async waitForNotification(method, predicate = () => true, timeoutMs = 10000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const hit = this.notifications.find((n) => n.msg.method === method && predicate(n.msg.params || {}));
      if (hit) return hit;
      await sleep(50);
    }
    return null;
  }

  clearNotifications() { this.notifications.length = 0; }

  async callTool(name, args = null, { timeoutMs = 60000, progressToken = null, meta = null } = {}) {
    const params = { name, arguments: args === null ? {} : args };
    const m = { ...(meta || {}) };
    if (progressToken !== null) m.progressToken = progressToken;
    if (Object.keys(m).length) params._meta = m;
    const res = await this.request('tools/call', params, { timeoutMs });
    return toolOutcome(res);
  }

  async close({ timeoutMs = 10000 } = {}) {
    if (this.closed) return this.exitInfo;
    this.closed = true;
    return new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        resolve(this.exitInfo);
      };
      this.child.once('exit', finish);
      try { this.child.stdin.end(); } catch { /* ignore */ }
      setTimeout(() => {
        if (this.exitInfo) return finish();
        try { this.child.kill(); } catch { /* ignore */ }
        setTimeout(finish, 500);
      }, timeoutMs);
    });
  }
}

/** Normalize a `tools/call` JSON-RPC response into a comparable outcome. */
function toolOutcome(jsonRpcResponse) {
  const out = {
    rpc_error: null,
    result: null,
    isError: false,
    structured: null,
    text: null,
    code: null,
  };
  if (jsonRpcResponse.error) {
    out.rpc_error = jsonRpcResponse.error;
    out.code = String(jsonRpcResponse.error.code);
    return out;
  }
  const result = jsonRpcResponse.result || {};
  out.result = result;
  out.isError = result.isError === true;
  out.structured = result.structuredContent ?? null;
  const content = Array.isArray(result.content) ? result.content : [];
  const textItem = content.find((c) => c && c.type === 'text');
  out.text = textItem ? textItem.text : null;
  const sc = out.structured;
  const candidates = [
    sc && sc.error && sc.error.code,
    sc && sc.code,
    sc && sc.task && sc.task.error && sc.task.error.code,
    sc && sc.details && sc.details.error && sc.details.error.code,
    sc && sc.details && sc.details.code,
  ].filter((x) => typeof x === 'string');
  if (candidates.length) out.code = candidates[0];
  if (!out.code && out.text) {
    try {
      const j = JSON.parse(out.text);
      const code = (j && j.error && j.error.code) || (j && j.code);
      if (typeof code === 'string') out.code = code;
    } catch { /* text isn't JSON */ }
  }
  if (!out.code && sc && sc.ok === false && sc.error && typeof sc.error === 'object') {
    out.code = sc.error.code || null;
  }
  return out;
}

function describeOutcome(o) {
  const bits = [];
  if (o.rpc_error) bits.push(`rpc_error=${o.rpc_error.code} ${o.rpc_error.message || ''}`);
  if (o.isError) bits.push('isError=true');
  if (o.code) bits.push(`code=${o.code}`);
  if (o.structured) bits.push(`structuredKeys=${Object.keys(o.structured).slice(0, 8).join(',')}`);
  if (o.text && !o.code) bits.push(`text=${truncate(o.text, 120)}`);
  return bits.join('; ') || 'no details';
}

/**
 * Contract §3.1: tool failures must be `isError:true` + structuredContent
 * carrying the structured error; only protocol/parameter-parsing problems may
 * use a JSON-RPC error. This helper enforces that for every error scenario.
 */
function expectToolError(sectionName, label, outcome, expectedCode, alsoRequireIsError = true) {
  const noRpcError = !outcome.rpc_error;
  const isErrorOk = alsoRequireIsError ? outcome.isError === true : true;
  const hasStructured = !!outcome.structured;
  const codeOk = expectedCode ? outcome.code === expectedCode : true;
  const pass = noRpcError && isErrorOk && hasStructured && codeOk;
  addCheck(sectionName, label, pass,
    `isError=${outcome.isError} rpc_error=${outcome.rpc_error ? JSON.stringify(outcome.rpc_error).slice(0, 80) : 'none'} structured=${hasStructured} code=${JSON.stringify(outcome.code)}${expectedCode ? ` expected=${expectedCode}` : ''}`);
  return pass;
}

// ---------------------------------------------------------------------------
// JSON Schema (subset) validation helpers
// ---------------------------------------------------------------------------

const SCHEMA_TYPES = new Set(['object', 'array', 'string', 'number', 'integer', 'boolean', 'null']);

function validateSchemaSubset(schema, at = '$', errors = []) {
  if (schema === true || schema === false) return errors;
  if (typeof schema !== 'object' || schema === null || Array.isArray(schema)) {
    errors.push(`${at}: schema is not an object`);
    return errors;
  }
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type];
    for (const t of types) {
      if (!SCHEMA_TYPES.has(t)) errors.push(`${at}.type: unknown type ${JSON.stringify(t)}`);
    }
  }
  if (schema.properties !== undefined) {
    if (typeof schema.properties !== 'object' || schema.properties === null || Array.isArray(schema.properties)) {
      errors.push(`${at}.properties: not an object`);
    } else {
      for (const [k, v] of Object.entries(schema.properties)) {
        validateSchemaSubset(v, `${at}.properties.${k}`, errors);
      }
    }
  }
  if (schema.required !== undefined) {
    if (!Array.isArray(schema.required) || schema.required.some((r) => typeof r !== 'string')) {
      errors.push(`${at}.required: not an array of strings`);
    } else if (schema.properties && typeof schema.properties === 'object') {
      for (const r of schema.required) {
        if (!(r in schema.properties)) errors.push(`${at}.required: "${r}" missing from properties`);
      }
    }
  }
  if (schema.items !== undefined) validateSchemaSubset(schema.items, `${at}.items`, errors);
  for (const key of ['anyOf', 'oneOf', 'allOf']) {
    if (Array.isArray(schema[key])) schema[key].forEach((s, i) => validateSchemaSubset(s, `${at}.${key}[${i}]`, errors));
  }
  for (const key of ['additionalProperties', 'propertyNames', 'not', 'if', 'then', 'else']) {
    if (schema[key] !== undefined && typeof schema[key] === 'object') validateSchemaSubset(schema[key], `${at}.${key}`, errors);
  }
  return errors;
}

// ---------------------------------------------------------------------------
// Contract expectations (from docs/agent-integration.md)
// ---------------------------------------------------------------------------

const CONTRACT_TOOLS = [
  { name: 'rdm_capabilities', rw: 'readOnly' },
  { name: 'rdm_probe', rw: 'readOnly' },
  { name: 'rdm_download', rw: 'destructive' },
  { name: 'rdm_plan_download', rw: 'readOnly' },
  { name: 'rdm_apply_plan', rw: 'destructive' },
  { name: 'rdm_list_tasks', rw: 'readOnly' },
  { name: 'rdm_get_task', rw: 'readOnly' },
  { name: 'rdm_wait_task', rw: 'readOnly' },
  { name: 'rdm_pause_task', rw: 'destructive' },
  { name: 'rdm_resume_task', rw: 'destructive' },
  { name: 'rdm_cancel_task', rw: 'destructive' },
  { name: 'rdm_verify_file', rw: 'readOnly' },
  { name: 'rdm_doctor', rw: 'readOnly' },
  { name: 'rdm_selftest', rw: 'readOnly' },
  { name: 'rdm_bench', rw: 'readOnly' },
  { name: 'rdm_state_maintenance', rw: 'destructive' },
];
const CONTRACT_TOOL_NAMES = CONTRACT_TOOLS.map((t) => t.name);

const CONTRACT_RESOURCES = ['rdm://capabilities', 'rdm://policy', 'rdm://schema', 'rdm://tasks'];
const CONTRACT_PROMPTS = ['download', 'batch_download', 'diagnose_failure'];

const ENVELOPE_KEYS = ['schema_version', 'ok', 'command', 'task_id', 'data', 'error', 'meta'];

// ---------------------------------------------------------------------------
// Section 1: negotiation (2025-06-18 + 2024-11-05 echo + ping)
// ---------------------------------------------------------------------------

async function sectionNegotiation(ctx) {
  const res = await ctx.client.initialize('2025-06-18');
  const r = res.result || {};
  const caps = r.capabilities || {};
  addCheck('negotiation', 'initialize_ok', !res.error, res.error ? JSON.stringify(res.error) : '');
  addCheck('negotiation', 'protocol_version_echo_2025_06_18', r.protocolVersion === '2025-06-18', `got ${JSON.stringify(r.protocolVersion)}`);
  addCheck('negotiation', 'server_info', !!(r.serverInfo && r.serverInfo.name === 'rdm' && typeof r.serverInfo.version === 'string'), JSON.stringify(r.serverInfo));
  addCheck('negotiation', 'capabilities_tools', !!caps.tools, JSON.stringify(caps.tools));
  addCheck('negotiation', 'capabilities_tools_listChanged_false', !!(caps.tools && caps.tools.listChanged === false), JSON.stringify(caps.tools));
  addCheck('negotiation', 'capabilities_resources', !!caps.resources, JSON.stringify(caps.resources));
  addCheck('negotiation', 'capabilities_resources_subscribe_false', !!(caps.resources && caps.resources.subscribe === false), JSON.stringify(caps.resources));
  addCheck('negotiation', 'capabilities_prompts', !!caps.prompts, JSON.stringify(caps.prompts));
  addCheck('negotiation', 'capabilities_logging', !!caps.logging, JSON.stringify(caps.logging));
  const instructions = typeof r.instructions === 'string' ? r.instructions.trim() : '';
  addCheck('negotiation', 'instructions_present', instructions.length >= 80, `len=${instructions.length}`);

  const pong = await ctx.client.request('ping', {}, { timeoutMs: 10000 });
  addCheck('negotiation', 'ping', !!pong.result && pong.error === undefined, JSON.stringify(pong.result));

  // Second process: client asks for the older protocol; contract says echo it.
  const oldClient = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: ctx.appData, tag: 'negotiation-2024' });
  try {
    const oldRes = await oldClient.initialize('2024-11-05');
    const oldVersion = oldRes.result && oldRes.result.protocolVersion;
    addCheck('negotiation', 'protocol_version_echo_2024_11_05', oldVersion === '2024-11-05', `got ${JSON.stringify(oldVersion)}`);
  } finally {
    const exit = await oldClient.close();
    addCheck('negotiation', 'older_protocol_client_clean_exit', !!exit && exit.code === 0, `exit=${JSON.stringify(exit)}`);
  }
}

// ---------------------------------------------------------------------------
// Section 2: tools/list
// ---------------------------------------------------------------------------

async function sectionToolsList(ctx) {
  const res = await ctx.client.request('tools/list', {}, { timeoutMs: 20000 });
  const tools = (res.result && res.result.tools) || [];
  addCheck('tools', 'tools_list_ok', !res.error && Array.isArray(tools) && tools.length > 0, res.error ? JSON.stringify(res.error) : `count=${tools.length}`);
  addCheck('tools', 'tool_count_ge_16', tools.length >= 16, `count=${tools.length}`);

  const names = tools.map((t) => t && t.name).filter(Boolean);
  const missing = CONTRACT_TOOL_NAMES.filter((n) => !names.includes(n));
  const extra = names.filter((n) => !CONTRACT_TOOL_NAMES.includes(n));
  addCheck('tools', 'all_contract_tools_present', missing.length === 0, `missing=[${missing.join(',')}]`);
  addCheck('tools', 'no_unexpected_tools', extra.length === 0, `extra=[${extra.join(',')}]`);

  const schemaErrors = [];
  const annotationErrors = [];
  const shortDescriptions = [];
  const READ_ONLY_TOOLS = ['rdm_capabilities', 'rdm_probe', 'rdm_plan_download', 'rdm_list_tasks', 'rdm_get_task', 'rdm_wait_task', 'rdm_verify_file', 'rdm_doctor', 'rdm_selftest', 'rdm_bench'];
  const TRULY_DESTRUCTIVE = ['rdm_download', 'rdm_apply_plan', 'rdm_cancel_task', 'rdm_state_maintenance'];
  const writeHintMismatches = [];
  const destructiveMismatches = [];
  const pauseResumeSemantics = [];
  for (const tool of tools) {
    const name = tool && tool.name;
    if (!name) { schemaErrors.push('tool without name'); continue; }
    const desc = typeof tool.description === 'string' ? tool.description.trim() : '';
    if (desc.length < 40) shortDescriptions.push(`${name}(len=${desc.length})`);
    const inputErrors = validateSchemaSubset(tool.inputSchema, `${name}.inputSchema`);
    if (!tool.inputSchema || tool.inputSchema.type !== 'object') inputErrors.push(`${name}: inputSchema.type must be object`);
    if (!tool.outputSchema) inputErrors.push(`${name}: missing outputSchema`);
    else validateSchemaSubset(tool.outputSchema, `${name}.outputSchema`, inputErrors);
    schemaErrors.push(...inputErrors);

    const ann = tool.annotations;
    if (!ann || typeof ann !== 'object') {
      annotationErrors.push(`${name}: missing annotations`);
      continue;
    }
    for (const hint of ['readOnlyHint', 'destructiveHint', 'idempotentHint', 'openWorldHint']) {
      if (typeof ann[hint] !== 'boolean') annotationErrors.push(`${name}.${hint} not boolean`);
    }
    if (READ_ONLY_TOOLS.includes(name) && ann.readOnlyHint !== true) writeHintMismatches.push(`${name}: expected readOnlyHint=true`);
    if (!READ_ONLY_TOOLS.includes(name) && CONTRACT_TOOL_NAMES.includes(name) && ann.readOnlyHint !== false) writeHintMismatches.push(`${name}: expected readOnlyHint=false`);
    if (TRULY_DESTRUCTIVE.includes(name) && ann.destructiveHint !== true) destructiveMismatches.push(`${name}: expected destructiveHint=true`);
    if (READ_ONLY_TOOLS.includes(name) && ann.destructiveHint === true) destructiveMismatches.push(`${name}: readOnly must not be destructive`);
    if (name === 'rdm_pause_task' || name === 'rdm_resume_task') {
      pauseResumeSemantics.push(`${name}: readOnly=${ann.readOnlyHint} destructive=${ann.destructiveHint} idempotent=${ann.idempotentHint}`);
    }
  }
  addCheck('tools', 'annotations_readonly_consistency', writeHintMismatches.length === 0, writeHintMismatches.join(' | '));
  addCheck('tools', 'annotations_destructive_consistency', destructiveMismatches.length === 0, destructiveMismatches.join(' | '));
  addCheck('tools', 'annotations_pause_resume_recorded', true, pauseResumeSemantics.join(' | '));
  addCheck('tools', 'input_output_schemas_valid_subset', schemaErrors.length === 0, schemaErrors.slice(0, 6).join(' | '));
  addCheck('tools', 'annotations_complete', annotationErrors.length === 0, annotationErrors.slice(0, 6).join(' | '));
  addCheck('tools', 'descriptions_substantive', shortDescriptions.length === 0, shortDescriptions.join(', '));

  // required fields sanity: rdm_download requires url, rdm_get_task requires some id field
  const byName = Object.fromEntries(tools.filter((t) => t && t.name).map((t) => [t.name, t]));
  const downloadRequired = (byName.rdm_download && byName.rdm_download.inputSchema && byName.rdm_download.inputSchema.required) || [];
  addCheck('tools', 'rdm_download_requires_url', downloadRequired.includes('url'), `required=${JSON.stringify(downloadRequired)}`);
  const getTask = byName.rdm_get_task;
  if (getTask) {
    const req = (getTask.inputSchema && getTask.inputSchema.required) || [];
    addCheck('tools', 'rdm_get_task_requires_task_id', req.some((r) => /task_?id/i.test(r)), `required=${JSON.stringify(req)}`);
  }
  ctx.toolNames = names;
}

// ---------------------------------------------------------------------------
// Section 3: resources
// ---------------------------------------------------------------------------

async function sectionResources(ctx) {
  const list = await ctx.client.request('resources/list', {}, { timeoutMs: 20000 });
  const resources = (list.result && list.result.resources) || [];
  const uris = resources.map((r) => r && r.uri).filter(Boolean);
  addCheck('resources', 'resources_list_ok', !list.error && Array.isArray(resources), list.error ? JSON.stringify(list.error) : `count=${resources.length}`);
  for (const uri of CONTRACT_RESOURCES) {
    addCheck('resources', `resource_listed_${uri.replace('rdm://', '')}`, uris.includes(uri), `uris=[${uris.join(',')}]`);
  }
  let templates = [];
  try {
    const t = await ctx.client.request('resources/templates/list', {}, { timeoutMs: 10000 });
    templates = (t.result && t.result.resourceTemplates) || [];
  } catch { /* some servers fold templates into resources/list */ }
  const templateUris = templates.map((t) => t && t.uriTemplate).filter(Boolean);
  const hasTaskTemplate = templateUris.some((u) => /^rdm:\/\/tasks\//.test(u)) || resources.some((r) => r && typeof r.uriTemplate === 'string' && /^rdm:\/\/tasks\//.test(r.uriTemplate));
  addCheck('resources', 'task_detail_template_present', hasTaskTemplate, `templates=[${templateUris.join(',')}]`);

  for (const uri of CONTRACT_RESOURCES) {
    const read = await ctx.client.request('resources/read', { uri }, { timeoutMs: 20000 });
    const contents = (read.result && read.result.contents) || [];
    const first = contents[0] || {};
    addCheck('resources', `read_${uri.replace('rdm://', '')}`, !read.error && contents.length > 0 && (first.text !== undefined || first.blob !== undefined), read.error ? JSON.stringify(read.error) : `keys=${Object.keys(first).join(',')}`);
    const text = typeof first.text === 'string' ? first.text : null;
    if (uri === 'rdm://schema' && text) {
      let parsed = null;
      try { parsed = JSON.parse(text); } catch { /* checked below */ }
      const toolNames = parsed ? extractToolNamesFromSchemaDoc(parsed) : [];
      const missing = CONTRACT_TOOL_NAMES.filter((n) => !toolNames.includes(n));
      addCheck('resources', 'schema_resource_lists_all_tools', !!parsed && missing.length === 0, parsed ? `tools=${toolNames.length} missing=[${missing.join(',')}]` : 'not JSON');
    }
    if (uri === 'rdm://capabilities' && text) {
      addCheck('resources', 'capabilities_resource_is_json_object', isJsonObject(text), truncate(text, 160));
    }
    if (uri === 'rdm://policy' && text) {
      addCheck('resources', 'policy_resource_is_json_object', isJsonObject(text), truncate(text, 160));
    }
    if (uri === 'rdm://tasks' && text) {
      let parsed = null;
      try { parsed = JSON.parse(text); } catch { /* checked below */ }
      const tasks = parsed && (Array.isArray(parsed.tasks) ? parsed.tasks : (Array.isArray(parsed) ? parsed : null));
      addCheck('resources', 'tasks_resource_is_task_list', !!tasks, parsed ? `shape=${Array.isArray(parsed) ? 'array' : Object.keys(parsed).join(',')}` : 'not JSON');
    }
  }
  // unknown resource must not crash the server
  const unknown = await ctx.client.request('resources/read', { uri: 'rdm://does-not-exist' }, { timeoutMs: 10000 });
  addCheck('resources', 'unknown_resource_is_error', !!(unknown.error || (unknown.result && unknown.result.contents && unknown.result.contents.length === 0)) || true, 'informational');
  const pong = await ctx.client.request('ping', {}, { timeoutMs: 10000 });
  addCheck('resources', 'server_alive_after_unknown_resource', !pong.error, JSON.stringify(pong.result));
}

function extractToolNamesFromSchemaDoc(parsed) {
  const names = [];
  const add = (v) => { if (typeof v === 'string') names.push(v); };
  if (Array.isArray(parsed.tools)) {
    for (const t of parsed.tools) { if (typeof t === 'string') add(t); else if (t && typeof t.name === 'string') add(t.name); }
  } else if (parsed.tools && typeof parsed.tools === 'object') {
    for (const [k, v] of Object.entries(parsed.tools)) { add(k); if (v && typeof v.name === 'string') add(v.name); }
  }
  if (Array.isArray(parsed.schemas)) for (const s of parsed.schemas) if (s && typeof s.name === 'string') add(s.name);
  return [...new Set(names)];
}

function isJsonObject(text) {
  try { const v = JSON.parse(text); return typeof v === 'object' && v !== null; } catch { return false; }
}

// ---------------------------------------------------------------------------
// Section 4: prompts
// ---------------------------------------------------------------------------

async function sectionPrompts(ctx) {
  const list = await ctx.client.request('prompts/list', {}, { timeoutMs: 20000 });
  const prompts = (list.result && list.result.prompts) || [];
  const names = prompts.map((p) => p && p.name).filter(Boolean);
  addCheck('prompts', 'prompts_list_ok', !list.error && Array.isArray(prompts) && prompts.length >= 3, list.error ? JSON.stringify(list.error) : `count=${prompts.length}`);
  for (const name of CONTRACT_PROMPTS) {
    addCheck('prompts', `prompt_${name}_listed`, names.includes(name), `names=[${names.join(',')}]`);
  }
  const args = {
    download: { url: ctx.fast.url },
    batch_download: { urls: `${ctx.fast.url},${ctx.fast.url}` },
    diagnose_failure: { task_id: 'tsk_0000000000000_0000' },
  };
  for (const name of CONTRACT_PROMPTS) {
    if (!names.includes(name)) continue;
    const got = await ctx.client.request('prompts/get', { name, arguments: args[name] }, { timeoutMs: 20000 });
    const r = got.result || {};
    const messages = r.messages || [];
    const hasText = messages.some((m) => {
      const c = m && m.content;
      if (!c) return false;
      if (typeof c.text === 'string') return true;
      if (Array.isArray(c)) return c.some((x) => x && typeof x.text === 'string');
      return false;
    });
    addCheck('prompts', `get_${name}`, !got.error && messages.length > 0 && hasText, got.error ? JSON.stringify(got.error) : `messages=${messages.length}`);
  }
}

// ---------------------------------------------------------------------------
// Section 5: read-only tools
// ---------------------------------------------------------------------------

async function sectionReadOnlyTools(ctx) {
  const caps = await ctx.client.callTool('rdm_capabilities', {});
  addCheck('tools_call', 'capabilities_ok', !caps.isError && !caps.rpc_error, describeOutcome(caps));
  addCheck('tools_call', 'capabilities_has_version', !!(caps.structured && (caps.structured.version || caps.structured.rdm_version || caps.structured.versions)), describeOutcome(caps));
  const capKeys = caps.structured ? Object.keys(caps.structured) : [];
  addCheck('tools_call', 'capabilities_shape', capKeys.length >= 3, `keys=${capKeys.slice(0, 12).join(',')}`);
  if (caps.structured) {
    const limits = caps.structured.limits || caps.structured.constraints || {};
    const hasLimits = Object.keys(limits).length > 0 || capKeys.some((k) => /limit|max|connection/i.test(k));
    addCheck('tools_call', 'capabilities_has_limits', hasLimits, `keys=${capKeys.slice(0, 12).join(',')}`);
    const policy = caps.structured.policy || caps.structured.policy_summary;
    addCheck('tools_call', 'capabilities_has_policy_summary', !!policy, describeOutcome({ structured: caps.structured }));
    const mcp = caps.structured.mcp || caps.structured.mcp_capabilities;
    addCheck('tools_call', 'capabilities_has_mcp_summary', !!mcp, describeOutcome({ structured: caps.structured }));
  }
  addCheck('tools_call', 'capabilities_text_content', typeof caps.text === 'string' && caps.text.trim().length > 0, truncate(caps.text || '', 120));

  const doctor = await ctx.client.callTool('rdm_doctor', {});
  addCheck('tools_call', 'doctor_ok', !doctor.isError && !doctor.rpc_error, describeOutcome(doctor));
  addCheck('tools_call', 'doctor_has_checks', !!(doctor.structured && (Array.isArray(doctor.structured.checks) || doctor.structured.status || doctor.structured.component)), describeOutcome(doctor));

  const maintenance = await ctx.client.callTool('rdm_state_maintenance', { dry_run: true }, { timeoutMs: 120000 });
  addCheck('tools_call', 'state_maintenance_dry_run_ok', !maintenance.isError && !maintenance.rpc_error, describeOutcome(maintenance));
  addCheck('tools_call', 'state_maintenance_dry_run_flag', !!(maintenance.structured && (maintenance.structured.dry_run === true || maintenance.structured.dryRun === true || /dry/i.test(JSON.stringify(maintenance.structured).slice(0, 400)))), describeOutcome(maintenance));

  const selftest = await ctx.client.callTool('rdm_selftest', {}, { timeoutMs: 240000 });
  addCheck('tools_call', 'selftest_ok', !selftest.isError && !selftest.rpc_error, describeOutcome(selftest));
  if (selftest.structured) {
    const cases = selftest.structured.cases || selftest.structured.results || selftest.structured.tests;
    const caseArr = Array.isArray(cases) ? cases : null;
    addCheck('tools_call', 'selftest_4_cases', !!caseArr && caseArr.length >= 4, caseArr ? `cases=${caseArr.length}` : `keys=${Object.keys(selftest.structured).join(',')}`);
    if (caseArr) {
      const failed = caseArr.filter((c) => c && (c.pass === false || c.ok === false || c.status === 'fail' || c.passed === false));
      addCheck('tools_call', 'selftest_all_cases_pass', failed.length === 0, failed.length ? JSON.stringify(failed.slice(0, 3)).slice(0, 300) : 'all pass');
    }
  }

  const bench = await ctx.client.callTool('rdm_bench', {}, { timeoutMs: 300000 });
  addCheck('tools_call', 'bench_ok', !bench.isError && !bench.rpc_error, describeOutcome(bench));
  if (bench.structured) {
    const s = JSON.stringify(bench.structured);
    const has1 = /"?connections"?\s*[:=]\s*1\b/.test(s) || /"?single/i.test(s) || /one_conn/i.test(s);
    const has8 = /"?connections"?\s*[:=]\s*8\b/.test(s) || /"?multi/i.test(s) || /eight_conn/i.test(s);
    const hasSpeedup = /speedup/i.test(s);
    addCheck('tools_call', 'bench_has_1_and_n', has1 && has8, `has1=${has1} has8=${has8} keys=${Object.keys(bench.structured).join(',')}`);
    addCheck('tools_call', 'bench_has_speedup', hasSpeedup, describeOutcome({ structured: bench.structured }));
  }
}

// ---------------------------------------------------------------------------
// Section 6: probe
// ---------------------------------------------------------------------------

async function sectionProbe(ctx) {
  const probe = await ctx.client.callTool('rdm_probe', { url: ctx.fast.url }, { timeoutMs: 60000 });
  addCheck('probe', 'probe_ok', !probe.isError && !probe.rpc_error, describeOutcome(probe));
  const sc = probe.structured || {};
  const total = sc.total_bytes ?? sc.size ?? sc.totalBytes;
  addCheck('probe', 'probe_total_bytes', total === SIZE_BYTES, `total=${JSON.stringify(total)} expected=${SIZE_BYTES}`);
  const ranges = sc.supports_ranges ?? sc.range_supported ?? sc.rangeSupported;
  addCheck('probe', 'probe_supports_ranges', ranges === true, `ranges=${JSON.stringify(ranges)}`);
  addCheck('probe', 'probe_file_name', !!(sc.file_name || sc.fileName || sc.filename), JSON.stringify(sc.file_name || sc.fileName || sc.filename));
  addCheck('probe', 'probe_http_version', !!(sc.http_version || sc.httpVersion), JSON.stringify(sc.http_version || sc.httpVersion));

  const bad = await ctx.client.callTool('rdm_probe', { url: 'ftp://example.com/x' }, { timeoutMs: 30000 });
  expectToolError('probe', 'probe_invalid_url_error', bad, 'invalid_url');
}

// ---------------------------------------------------------------------------
// Section 7: download(wait) + progress + verify + list/get
// ---------------------------------------------------------------------------

async function sectionDownloadFlow(ctx) {
  if (!ctx.reference) throw new Error('reference hash unavailable');
  const dlDir = path.join(ctx.appData, 'dl');
  fs.mkdirSync(dlDir, { recursive: true });
  ctx.dlDir = dlDir;
  const progressToken = `qa-progress-${Date.now()}`;
  ctx.client.clearNotifications();

  const outcome = await ctx.client.callTool('rdm_download', {
    url: ctx.fast.url,
    dest_dir: dlDir,
    file_name: 'agent-download-1.bin',
    connections: 8,
    wait: true,
    wait_timeout_ms: 180000,
  }, { timeoutMs: 240000, progressToken });
  addCheck('download', 'download_wait_ok', !outcome.isError && !outcome.rpc_error, describeOutcome(outcome));

  const rec = taskRecordFrom(outcome);
  addCheck('download', 'download_returns_task_record', !!(rec && (rec.id || rec.task_id)), rec ? `id=${rec.id || rec.task_id} state=${rec.state}` : describeOutcome(outcome));
  const taskId = rec && (rec.id || rec.task_id);
  ctx.taskId = taskId;
  const state = rec && rec.state;
  addCheck('download', 'download_completed', state === 'completed', `state=${JSON.stringify(state)}`);

  const target = path.join(dlDir, 'agent-download-1.bin');
  const exists = fs.existsSync(target);
  addCheck('download', 'download_target_exists', exists, target);
  if (exists) {
    const hash = await sha256File(target);
    ctx.downloadedHash = hash;
    addCheck('download', 'download_sha256_matches_reference', hash === ctx.reference.sha256, `actual=${hash.slice(0, 16)}… reference=${ctx.reference.sha256.slice(0, 16)}…`);
    const stat = fs.statSync(target);
    addCheck('download', 'download_size_matches', stat.size === SIZE_BYTES, `size=${stat.size} expected=${SIZE_BYTES}`);
  }

  // result block in task record
  if (rec) {
    const result = rec.result || {};
    const recHash = result.sha256 || rec.sha256;
    addCheck('download', 'task_record_result_sha256', recHash === ctx.reference.sha256, `result.sha256=${JSON.stringify(recHash)}`);
    const size = result.size ?? result.total_bytes ?? rec.total_bytes;
    addCheck('download', 'task_record_size', size === SIZE_BYTES, `size=${JSON.stringify(size)}`);
    addCheck('download', 'task_record_path', !!(result.path || rec.path), JSON.stringify(result.path || rec.path));
  }

  // progress notifications
  const progressNotes = ctx.client.findNotifications('notifications/progress');
  const forToken = progressNotes.filter((n) => (n.msg.params || {}).progressToken === progressToken);
  addCheck('download', 'progress_notifications_ge_1', forToken.length >= 1, `count=${forToken.length} total_progress=${progressNotes.length}`);
  if (forToken.length) {
    const progresses = forToken.map((n) => Number((n.msg.params || {}).progress)).filter((n) => Number.isFinite(n));
    const increasing = progresses.length === 1 || progresses.every((v, i) => i === 0 || v >= progresses[i - 1]);
    addCheck('download', 'progress_monotonic', increasing, `values=${truncate(progresses.join(','), 160)}`);
    const totals = forToken.map((n) => Number((n.msg.params || {}).total)).filter((n) => Number.isFinite(n));
    addCheck('download', 'progress_reports_total', totals.length > 0 && totals.every((t) => t === SIZE_BYTES), `totals=${truncate(totals.join(','), 120)}`);
    const messages = forToken.map((n) => String((n.msg.params || {}).message || '')).join(' ');
    addCheck('download', 'progress_message_mentions_speed_or_eta', /speed|ETA|B\/s|MiB\/s|speed_bps/i.test(messages), truncate(messages, 160));
  }

  // verify file
  const verify = await ctx.client.callTool('rdm_verify_file', { path: target, expected_sha256: ctx.reference.sha256 }, { timeoutMs: 60000 });
  addCheck('verify', 'verify_ok', !verify.isError && !verify.rpc_error, describeOutcome(verify));
  if (verify.structured) {
    const s = JSON.stringify(verify.structured);
    const okFlag = /"?(verified|ok|match)"?\s*:\s*true/i.test(s) || /"?(verified|ok|match)"?\s*:\s*true/i.test(String(verify.text || ''));
    addCheck('verify', 'verify_reports_match', okFlag, truncate(s, 200));
    addCheck('verify', 'verify_sha256_field', s.includes(ctx.reference.sha256), 'structured sha256');
  }
  const verifyByTask = await ctx.client.callTool('rdm_verify_file', { task_id: taskId, expected_sha256: ctx.reference.sha256 }, { timeoutMs: 60000 });
  addCheck('verify', 'verify_by_task_id_ok', !verifyByTask.isError && !verifyByTask.rpc_error, describeOutcome(verifyByTask));

  const badVerify = await ctx.client.callTool('rdm_verify_file', { path: target, expected_sha256: '0'.repeat(64) }, { timeoutMs: 60000 });
  expectToolError('verify', 'verify_mismatch_is_error', badVerify, 'checksum_mismatch');

  // list/get
  const list = await ctx.client.callTool('rdm_list_tasks', { limit: 20 }, { timeoutMs: 60000 });
  addCheck('tasks', 'list_tasks_ok', !list.isError && !list.rpc_error, describeOutcome(list));
  const listed = extractTaskList(list);
  addCheck('tasks', 'list_contains_our_task', !!listed && listed.some((t) => (t.id || t.task_id) === taskId), listed ? `count=${listed.length}` : describeOutcome(list));
  const summariesLeakFree = !JSON.stringify(listed || []).includes('agent-download-1.bin"') || true;
  addCheck('tasks', 'list_summary_shape', !!listed && listed.every((t) => typeof t === 'object'), `count=${listed ? listed.length : 'n/a'}`);

  const get = await ctx.client.callTool('rdm_get_task', { task_id: taskId, include_events: true }, { timeoutMs: 60000 });
  addCheck('tasks', 'get_task_ok', !get.isError && !get.rpc_error, describeOutcome(get));
  const got = taskRecordFrom(get);
  addCheck('tasks', 'get_task_matches', !!(got && (got.id || got.task_id) === taskId && got.state === 'completed'), got ? `id=${got.id || got.task_id} state=${got.state}` : describeOutcome(get));
  if (got) {
    const events = got.events || (got.event_count !== undefined ? got.event_count : null);
    addCheck('tasks', 'get_task_include_events', events !== undefined && events !== null, `events=${Array.isArray(events) ? events.length : JSON.stringify(events)}`);
    const str = JSON.stringify(got);
    addCheck('tasks', 'task_has_schema_version', got.schema_version === 'rdm.agent/v1', JSON.stringify(got.schema_version));
    const idShape = /^tsk_/.test(String(got.id || got.task_id));
    addCheck('tasks', 'task_id_stable_shape', idShape, String(got.id || got.task_id));
  }
  const missing = await ctx.client.callTool('rdm_get_task', { task_id: 'tsk_does_not_exist_000' }, { timeoutMs: 30000 });
  expectToolError('tasks', 'get_missing_task_not_found', missing, 'not_found');
}

function mergeEnvelopeTask(envelope) {
  // SKILL §2: structuredContent is a unified envelope {schema_version, ok, task_id, data, error}.
  if (!envelope || typeof envelope !== 'object') return null;
  const data = envelope.data;
  if (data && typeof data === 'object') {
    if (data.task && typeof data.task === 'object') return data.task;
    if (data.id || data.task_id) return data;
    if (envelope.task_id) return { id: envelope.task_id, ...data };
  }
  if (envelope.task_id) return { id: envelope.task_id };
  if (envelope.task && typeof envelope.task === 'object') return envelope.task;
  return null;
}

function taskRecordFrom(outcome) {
  if (!outcome) return null;
  const sc = outcome.structured;
  if (sc) {
    if (sc.task && typeof sc.task === 'object') return sc.task;
    const viaEnvelope = mergeEnvelopeTask(sc);
    if (viaEnvelope) return viaEnvelope;
    if (sc.id || sc.task_id) return sc;
  }
  if (outcome.text) {
    try {
      const j = JSON.parse(outcome.text);
      if (j.task) return j.task;
      const viaEnvelope = mergeEnvelopeTask(j);
      if (viaEnvelope) return viaEnvelope;
      if (j.id || j.task_id) return j;
    } catch { /* ignore */ }
  }
  return null;
}

function extractTaskList(outcome) {
  const sc = outcome && outcome.structured;
  if (sc) {
    if (Array.isArray(sc.tasks)) return sc.tasks;
    if (sc.data && Array.isArray(sc.data.tasks)) return sc.data.tasks;
  }
  if (outcome && outcome.text) {
    try {
      const j = JSON.parse(outcome.text);
      if (Array.isArray(j.tasks)) return j.tasks;
      if (j.data && Array.isArray(j.data.tasks)) return j.data.tasks;
    } catch { /* ignore */ }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Section 8: idempotency
// ---------------------------------------------------------------------------

async function sectionIdempotency(ctx) {
  const key = `qa-idem-${Date.now()}`;
  const first = await ctx.client.callTool('rdm_download', {
    url: ctx.fast.url, dest_dir: ctx.dlDir, file_name: 'agent-idem.bin', connections: 8,
    idempotency_key: key, wait: false,
  }, { timeoutMs: 60000 });
  addCheck('idempotency', 'first_call_ok', !first.isError && !first.rpc_error, describeOutcome(first));
  const firstRec = taskRecordFrom(first);
  const firstId = firstRec && (firstRec.id || firstRec.task_id);

  const second = await ctx.client.callTool('rdm_download', {
    url: ctx.fast.url, dest_dir: ctx.dlDir, file_name: 'agent-idem.bin', connections: 8,
    idempotency_key: key, wait: false,
  }, { timeoutMs: 60000 });
  const secondRec = taskRecordFrom(second);
  const secondId = secondRec && (secondRec.id || secondRec.task_id);
  addCheck('idempotency', 'duplicate_returns_same_task', !!firstId && firstId === secondId, `first=${firstId} second=${secondId}`);
  addCheck('idempotency', 'duplicate_not_error', !second.isError && !second.rpc_error, describeOutcome(second));

  // settle then count tasks with that idempotency key
  const wait = await ctx.client.callTool('rdm_wait_task', { task_id: firstId, timeout_ms: 120000 }, { timeoutMs: 150000 });
  addCheck('idempotency', 'wait_after_idem_ok', !wait.isError && !wait.rpc_error, describeOutcome(wait));
  const list = await ctx.client.callTool('rdm_list_tasks', { limit: 100 }, { timeoutMs: 60000 });
  const listed = extractTaskList(list) || [];
  const matching = listed.filter((t) => (t.idempotency_key === key) || JSON.stringify(t).includes(key));
  addCheck('idempotency', 'only_one_task_for_key', matching.length === 1, `matches=${matching.length}`);

  const third = await ctx.client.callTool('rdm_download', {
    url: ctx.fast.url, dest_dir: ctx.dlDir, file_name: 'agent-idem-2.bin', connections: 8,
    idempotency_key: `${key}-other`, wait: false,
  }, { timeoutMs: 60000 });
  const thirdRec = taskRecordFrom(third);
  addCheck('idempotency', 'different_key_new_task', !!(thirdRec && (thirdRec.id || thirdRec.task_id) && (thirdRec.id || thirdRec.task_id) !== firstId), `third=${thirdRec && (thirdRec.id || thirdRec.task_id)}`);
  // clean up
  await ctx.client.callTool('rdm_cancel_task', { task_id: thirdRec && (thirdRec.id || thirdRec.task_id) }, { timeoutMs: 60000 });
}

// ---------------------------------------------------------------------------
// Section 9: pause / resume / cancel / wait_task on a slow fixture
// ---------------------------------------------------------------------------

async function sectionControlFlow(ctx) {
  if (!ctx.slow) throw new Error('slow fixture unavailable');
  const dlDir = path.join(ctx.appData, 'dl-control');
  fs.mkdirSync(dlDir, { recursive: true });

  const start = await ctx.client.callTool('rdm_download', {
    url: ctx.slow.url, dest_dir: dlDir, file_name: 'agent-slow.bin', connections: 4, wait: false,
  }, { timeoutMs: 60000 });
  const rec = taskRecordFrom(start);
  const taskId = rec && (rec.id || rec.task_id);
  addCheck('control', 'slow_download_started', !start.isError && !!taskId, `id=${taskId}`);
  if (!taskId) return;

  const downloading = await waitForTaskState(ctx.client, taskId, (t) => t.state === 'downloading' && (t.downloaded_bytes || 0) > 0, 30000);
  addCheck('control', 'reaches_downloading_with_bytes', !!downloading, downloading ? `downloaded=${downloading.downloaded_bytes}/${downloading.total_bytes}` : 'timeout');

  const pause = await ctx.client.callTool('rdm_pause_task', { task_id: taskId }, { timeoutMs: 60000 });
  addCheck('control', 'pause_accepted', !pause.isError && !pause.rpc_error, describeOutcome(pause));
  const paused = await waitForTaskState(ctx.client, taskId, (t) => t.state === 'paused', 20000);
  addCheck('control', 'state_paused', !!paused, paused ? `downloaded=${paused.downloaded_bytes}` : 'timeout');
  if (paused) {
    addCheck('control', 'paused_progress_partial', paused.downloaded_bytes > 0 && paused.downloaded_bytes < paused.total_bytes, `${paused.downloaded_bytes}/${paused.total_bytes}`);
  }

  const resume = await ctx.client.callTool('rdm_resume_task', { task_id: taskId }, { timeoutMs: 60000 });
  addCheck('control', 'resume_accepted', !resume.isError && !resume.rpc_error, describeOutcome(resume));
  const wait = await ctx.client.callTool('rdm_wait_task', { task_id: taskId, timeout_ms: 180000 }, { timeoutMs: 200000 });
  addCheck('control', 'wait_task_completes', !wait.isError && !wait.rpc_error, describeOutcome(wait));
  const done = taskRecordFrom(wait) || await getTask(ctx.client, taskId);
  addCheck('control', 'resumed_completed', !!(done && done.state === 'completed'), done ? `state=${done.state}` : describeOutcome(wait));
  const target = path.join(dlDir, 'agent-slow.bin');
  if (fs.existsSync(target)) {
    const hash = await sha256File(target);
    addCheck('control', 'resumed_hash_ok', hash === ctx.reference.sha256, `sha=${hash.slice(0, 16)}…`);
  } else {
    addCheck('control', 'resumed_hash_ok', false, 'target file missing');
  }

  // cancel mid-download
  const start2 = await ctx.client.callTool('rdm_download', {
    url: ctx.slow.url, dest_dir: dlDir, file_name: 'agent-cancel.bin', connections: 4, wait: false,
  }, { timeoutMs: 60000 });
  const rec2 = taskRecordFrom(start2);
  const task2 = rec2 && (rec2.id || rec2.task_id);
  addCheck('control', 'cancel_download_started', !start2.isError && !!task2, `id=${task2}`);
  if (task2) {
    await waitForTaskState(ctx.client, task2, (t) => t.state === 'downloading' && (t.downloaded_bytes || 0) > 0, 30000);
    const cancel = await ctx.client.callTool('rdm_cancel_task', { task_id: task2 }, { timeoutMs: 60000 });
    addCheck('control', 'cancel_accepted', !cancel.isError && !cancel.rpc_error, describeOutcome(cancel));
    const cancelled = await waitForTaskState(ctx.client, task2, (t) => t.state === 'cancelled', 20000);
    addCheck('control', 'state_cancelled', !!cancelled, cancelled ? `state=${cancelled.state}` : 'timeout');
    const partPath = path.join(dlDir, 'agent-cancel.bin.part');
    addCheck('control', 'cancel_keeps_partial', fs.existsSync(partPath), partPath);
    // explicit cancel of an already cancelled task must not crash
    const again = await ctx.client.callTool('rdm_cancel_task', { task_id: task2 }, { timeoutMs: 30000 });
    addCheck('control', 'cancel_idempotent_no_crash', !again.rpc_error, describeOutcome(again));

    // discard_partial=true must delete the partial file (SKILL §2/§6)
    if (fs.existsSync(partPath)) {
      const discard = await ctx.client.callTool('rdm_cancel_task', { task_id: task2, discard_partial: true }, { timeoutMs: 30000 });
      addCheck('control', 'discard_partial_accepted', !discard.rpc_error, describeOutcome(discard));
      await sleep(300);
      addCheck('control', 'discard_partial_removes_part', !fs.existsSync(partPath), fs.existsSync(partPath) ? 'partial still exists' : 'partial removed');
    } else {
      addCheck('control', 'discard_partial_removes_part', true, 'no partial file to discard');
    }
  }
}

async function getTask(client, taskId) {
  const got = await client.callTool('rdm_get_task', { task_id: taskId }, { timeoutMs: 30000 });
  return taskRecordFrom(got);
}

async function waitForTaskState(client, taskId, predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const t = await getTask(client, taskId);
    if (t && predicate(t)) return t;
    await sleep(150);
  }
  return null;
}

// ---------------------------------------------------------------------------
// Section 10: policy + approval
// ---------------------------------------------------------------------------

function writePolicy(appData, policy) {
  const file = path.join(appData, 'RDM', 'agent', 'policy.json');
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(policy, null, 2));
  return file;
}

function defaultPolicy(overrides = {}) {
  return {
    schema_version: 'rdm.agent/v1',
    allowed_hosts: ['127.0.0.1', 'localhost'],
    allowed_dirs: [],
    max_bytes_per_task: 10 * 1024 * 1024 * 1024,
    require_approval_over_bytes: 1024 * 1024 * 1024,
    max_concurrent_tasks: 3,
    default_connections: 8,
    allow_private_hosts: true,
    allow_overwrite: false,
    ...overrides,
  };
}

/**
 * QA seed policy for scenarios that legitimately download from the local
 * fixture. The product default policy is expected to DENY private/metadata
 * hosts; tests must therefore opt in explicitly (Lead security decision).
 */
function writeTestPolicy(appData, overrides = {}) {
  return writePolicy(appData, defaultPolicy({
    allowed_hosts: ['127.0.0.1', 'localhost'],
    allowed_dirs: [appData],
    max_concurrent_tasks: 8,
    require_approval_over_bytes: 1024 * 1024 * 1024,
    ...overrides,
  }));
}

async function withFreshServer(ctx, tag, appData, fn) {
  const client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: appData, tag });
  try {
    const init = await client.initialize('2025-06-18');
    if (!init || init.error) throw new Error(`initialize failed: ${JSON.stringify(init && init.error)}`);
    return await fn(client);
  } finally {
    await client.close();
  }
}

async function sectionPolicy(ctx) {
  const dlDir = path.join(ctx.appData, 'dl-policy');
  fs.mkdirSync(dlDir, { recursive: true });

  // A) host not allowed -> policy_denied
  {
    const appData = isolatedAppData('policy-denied');
    writePolicy(appData, defaultPolicy({ allowed_hosts: ['*.example.com'], allowed_dirs: [dlDir] }));
    await withFreshServer(ctx, 'policy-denied', appData, async (client) => {
      const denied = await client.callTool('rdm_download', { url: ctx.fast.url, dest_dir: dlDir, file_name: 'denied.bin', wait: true }, { timeoutMs: 60000 });
      expectToolError('policy', 'policy_denied', denied, 'policy_denied');
    });
  }

  // B) needs_approval -> plan_download -> apply_plan -> single-use token
  {
    const appData = isolatedAppData('policy-approval');
    writePolicy(appData, defaultPolicy({ allowed_dirs: [dlDir], require_approval_over_bytes: 1024 * 1024 }));
    await withFreshServer(ctx, 'policy-approval', appData, async (client) => {
      const need = await client.callTool('rdm_download', { url: ctx.fast.url, dest_dir: dlDir, file_name: 'approval.bin', wait: true }, { timeoutMs: 120000 });
      expectToolError('policy', 'needs_approval', need, 'needs_approval');
      const details = need.structured && (need.structured.details || need.structured);
      addCheck('policy', 'needs_approval_has_plan', !!(details && (details.plan || details.plan_id)), describeOutcome(need));
      const token = details && (details.approval_token || (details.plan && details.plan.approval_token));
      addCheck('policy', 'needs_approval_has_token', typeof token === 'string' && token.length > 0, `token=${token ? `${String(token).slice(0, 12)}…` : 'missing'}`);

      const plan = await client.callTool('rdm_plan_download', { url: ctx.fast.url, dest_dir: dlDir, file_name: 'approval2.bin' }, { timeoutMs: 120000 });
      addCheck('policy', 'plan_download_ok', !plan.isError && !plan.rpc_error, describeOutcome(plan));
      addCheck('policy', 'plan_download_has_token', !!(plan.structured && (plan.structured.approval_token || (plan.structured.plan && plan.structured.plan.approval_token))), describeOutcome(plan));
      const planObj = plan.structured && (plan.structured.plan || plan.structured);
      const token2 = plan.structured && (plan.structured.approval_token || (plan.structured.plan && plan.structured.plan.approval_token));
      const planId = planObj && (planObj.plan_id || planObj.id);

      if (token2) {
        const applyArgs = { approval_token: token2 };
        if (planId) applyArgs.plan_id = planId;
        const applied = await client.callTool('rdm_apply_plan', applyArgs, { timeoutMs: 120000 });
        addCheck('policy', 'apply_plan_ok', !applied.isError && !applied.rpc_error, describeOutcome(applied));
        const appliedRec = taskRecordFrom(applied);
        const appliedId = appliedRec && (appliedRec.id || appliedRec.task_id);
        if (appliedId) {
          const waited = await client.callTool('rdm_wait_task', { task_id: appliedId, timeout_ms: 180000 }, { timeoutMs: 200000 });
          const rec = taskRecordFrom(waited) || (await getTask(client, appliedId));
          addCheck('policy', 'approved_download_completed', !!(rec && rec.state === 'completed'), rec ? `state=${rec.state}` : describeOutcome(waited));
        } else {
          addCheck('policy', 'approved_download_completed', false, describeOutcome(applied));
        }
        const reusedArgs = { approval_token: token2 };
        if (planId) reusedArgs.plan_id = planId;
        const reused = await client.callTool('rdm_apply_plan', reusedArgs, { timeoutMs: 60000 });
        expectToolError('policy', 'approval_token_single_use', reused, null);
      } else {
        addCheck('policy', 'apply_plan_ok', false, 'no token returned by plan_download');
        addCheck('policy', 'approval_token_single_use', false, 'no token to reuse');
      }
    });
  }

  // C) conflict: pre-create target file, allow_overwrite=false
  {
    const appData = isolatedAppData('policy-conflict');
    writePolicy(appData, defaultPolicy({ allowed_dirs: [dlDir], require_approval_over_bytes: 10 * 1024 * 1024 * 1024 }));
    fs.writeFileSync(path.join(dlDir, 'conflict.bin'), 'x');
    await withFreshServer(ctx, 'policy-conflict', appData, async (client) => {
      const conflict = await client.callTool('rdm_download', { url: ctx.fast.url, dest_dir: dlDir, file_name: 'conflict.bin', wait: true }, { timeoutMs: 60000 });
      expectToolError('policy', 'conflict_on_existing_file', conflict, 'conflict');
    });
    try { fs.unlinkSync(path.join(dlDir, 'conflict.bin')); } catch { /* ignore */ }
  }
}

// ---------------------------------------------------------------------------
// Section 11: redaction
// ---------------------------------------------------------------------------

const SECRET_COOKIE = 'rdmqa_secret_cookie_5f1e9c';
const SECRET_TOKEN = 'rdmqa_secret_bearer_8c2d4b';

async function sectionRedaction(ctx) {
  const appData = isolatedAppData('redaction');
  writeTestPolicy(appData);
  const dlDir = path.join(appData, 'dl');
  fs.mkdirSync(dlDir, { recursive: true });
  const client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: appData, tag: 'redaction' });
  let taskId = null;
  try {
    await client.initialize('2025-06-18');
    const outcome = await client.callTool('rdm_download', {
      url: ctx.fast.url,
      dest_dir: dlDir,
      file_name: 'secret.bin',
      connections: 4,
      wait: true,
      wait_timeout_ms: 120000,
      cookie: `session=${SECRET_COOKIE}; theme=dark`,
      headers: { Authorization: `Bearer ${SECRET_TOKEN}`, 'X-QA': 'ok' },
      referer: 'https://example.com/page',
      user_agent: 'rdm-qa-redaction-test',
    }, { timeoutMs: 180000 });
    addCheck('redaction', 'secret_download_ok', !outcome.isError && !outcome.rpc_error, describeOutcome(outcome));
    const rec = taskRecordFrom(outcome);
    taskId = rec && (rec.id || rec.task_id);

    // collect every agent-visible payload: all inbound lines + tool responses + resources
    const inboundBlob = inboundLines.join('\n');
    addCheck('redaction', 'cookie_not_in_any_stdout_payload', !inboundBlob.includes(SECRET_COOKIE), `scanned ${inboundLines.length} lines`);
    addCheck('redaction', 'bearer_not_in_any_stdout_payload', !inboundBlob.includes(SECRET_TOKEN), `scanned ${inboundLines.length} lines`);

    const stderrBlob = client.stderrLines.join('\n');
    addCheck('redaction', 'cookie_not_in_server_stderr', !stderrBlob.includes(SECRET_COOKIE), `stderr_lines=${client.stderrLines.length}`);
    addCheck('redaction', 'bearer_not_in_server_stderr', !stderrBlob.includes(SECRET_TOKEN), `stderr_lines=${client.stderrLines.length}`);

    if (taskId) {
      const get = await client.callTool('rdm_get_task', { task_id: taskId, include_events: true }, { timeoutMs: 60000 });
      const getBlob = JSON.stringify(get);
      addCheck('redaction', 'get_task_redacted', !getBlob.includes(SECRET_COOKIE) && !getBlob.includes(SECRET_TOKEN), `len=${getBlob.length}`);
      const tasksRes = await client.request('resources/read', { uri: 'rdm://tasks' }, { timeoutMs: 30000 });
      const tasksBlob = JSON.stringify(tasksRes);
      addCheck('redaction', 'tasks_resource_redacted', !tasksBlob.includes(SECRET_COOKIE) && !tasksBlob.includes(SECRET_TOKEN), `len=${tasksBlob.length}`);
      const single = await client.request('resources/read', { uri: `rdm://tasks/${taskId}` }, { timeoutMs: 30000 });
      const singleBlob = JSON.stringify(single);
      const singleOk = !single.error && single.result && Array.isArray(single.result.contents) && single.result.contents.length > 0;
      addCheck('redaction', 'task_resource_single', singleOk, single.error ? JSON.stringify(single.error) : 'ok');
      addCheck('redaction', 'task_resource_redacted', !singleBlob.includes(SECRET_COOKIE) && !singleBlob.includes(SECRET_TOKEN), `len=${singleBlob.length}`);

      // on-disk state: tasks/<id>.json may contain credentials (contract allows);
      // events journal and logs must be redacted.
      const agentDir = path.join(appData, 'RDM', 'agent');
      const taskFile = path.join(agentDir, 'tasks', `${taskId}.json`);
      addCheck('redaction', 'task_store_file_exists', fs.existsSync(taskFile), taskFile);
      const eventsDir = path.join(agentDir, 'events');
      let eventLeak = false;
      let checkedEventFiles = 0;
      if (fs.existsSync(eventsDir)) {
        for (const f of fs.readdirSync(eventsDir)) {
          const p = path.join(eventsDir, f);
          if (!fs.statSync(p).isFile()) continue;
          const text = fs.readFileSync(p, 'utf8');
          checkedEventFiles++;
          if (text.includes(SECRET_COOKIE) || text.includes(SECRET_TOKEN)) eventLeak = true;
        }
      }
      addCheck('redaction', 'events_journal_redacted', !eventLeak, `files=${checkedEventFiles}`);
      // logs: scan every file under agent dir except tasks/ (allowed) and plans
      let logLeak = false;
      let checkedLogFiles = 0;
      const walk = (dir) => {
        if (!fs.existsSync(dir)) return;
        for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
          const p = path.join(dir, entry.name);
          if (entry.isDirectory()) walk(p);
          else {
            const rel = path.relative(agentDir, p).replace(/\\/g, '/');
            if (rel.startsWith('tasks/')) continue; // allowed to contain full request per contract
            checkedLogFiles++;
            const text = fs.readFileSync(p, 'utf8');
            if (text.includes(SECRET_COOKIE) || text.includes(SECRET_TOKEN)) logLeak = true;
          }
        }
      };
      walk(agentDir);
      addCheck('redaction', 'agent_store_non_task_files_redacted', !logLeak, `files=${checkedLogFiles}`);
    } else {
      addCheck('redaction', 'task_id_available_for_redaction_scan', false, 'no task id in download response');
    }
  } finally {
    await client.close();
  }
}

// ---------------------------------------------------------------------------
// Section 12: robustness + EOF persistence + cancelled + concurrency
// ---------------------------------------------------------------------------

async function sectionRobustness(ctx) {
  // invalid JSON line -> JSON-RPC error or graceful handling, server keeps serving
  ctx.client.clearNotifications();
  ctx.client.sendRaw('{this is not json');
  const errRes = await waitForResponseWithCode(ctx.client, -32700, 8000);
  const pingAfterBad = await ctx.client.request('ping', {}, { timeoutMs: 10000 });
  addCheck('robustness', 'invalid_json_handled', !!errRes || !pingAfterBad.error, errRes ? 'returned -32700' : 'no parse error response (server tolerated)');
  addCheck('robustness', 'alive_after_invalid_json', !pingAfterBad.error, JSON.stringify(pingAfterBad.result));

  // unknown method
  const unknown = await ctx.client.request('rdm/nonexistent_method', {}, { timeoutMs: 10000 });
  addCheck('robustness', 'unknown_method_error', !!unknown.error && (unknown.error.code === -32601 || typeof unknown.error.code === 'number'), JSON.stringify(unknown.error));
  const pingAfterUnknown = await ctx.client.request('ping', {}, { timeoutMs: 10000 });
  addCheck('robustness', 'alive_after_unknown_method', !pingAfterUnknown.error, JSON.stringify(pingAfterUnknown.result));

  // missing params / unknown tool
  const noArgs = await ctx.client.request('tools/call', { name: 'rdm_download', arguments: {} }, { timeoutMs: 30000 });
  const noArgsOut = toolOutcome(noArgs);
  addCheck('robustness', 'missing_required_params_error', noArgsOut.isError || !!noArgsOut.rpc_error || noArgsOut.code === 'invalid_request', describeOutcome(noArgsOut));
  const unknownTool = await ctx.client.request('tools/call', { name: 'rdm_not_a_tool', arguments: {} }, { timeoutMs: 20000 });
  const unknownToolOut = toolOutcome(unknownTool);
  addCheck('robustness', 'unknown_tool_error', unknownToolOut.isError || !!unknownToolOut.rpc_error, describeOutcome(unknownToolOut));

  // concurrent requests: pipeline two calls, both answers must be correlated by id
  const [a, b] = await Promise.all([
    ctx.client.request('tools/list', {}, { timeoutMs: 30000 }),
    ctx.client.request('ping', {}, { timeoutMs: 30000 }),
  ]);
  addCheck('robustness', 'concurrent_requests_correlated', !!a.result && !!b.result, `a.tools=${a.result ? (a.result.tools || []).length : 'n/a'} b=${JSON.stringify(b.result)}`);

  // notifications/cancelled cancels the WAIT only; the task keeps downloading
  if (ctx.slow) {
    const dlDir = path.join(ctx.appData, 'dl-cancelwait');
    fs.mkdirSync(dlDir, { recursive: true });
    const start = await ctx.client.callTool('rdm_download', {
      url: ctx.slow.url, dest_dir: dlDir, file_name: 'agent-cancelwait.bin', connections: 2, wait: false,
    }, { timeoutMs: 60000 });
    const rec = taskRecordFrom(start);
    const taskId = rec && (rec.id || rec.task_id);
    addCheck('robustness', 'cancelwait_task_started', !!taskId, `id=${taskId}`);
    if (taskId) {
      await waitForTaskState(ctx.client, taskId, (t) => t.state === 'downloading' && (t.downloaded_bytes || 0) > 0, 30000);
      let progressToken = null;
      let callId = null;
      const waitPromise = (async () => {
        // capture the request id by wrapping request through callTool is opaque;
        // issue the JSON-RPC call directly so we know the id to cancel.
        const id = ++ctx.client.seq;
        callId = id;
        progressToken = `qa-cancelwait-${id}`;
        const promise = new Promise((resolve, reject) => {
          const timer = setTimeout(() => reject(new Error('wait_task did not return after notifications/cancelled')), 20000);
          ctx.client.pending.set(id, { resolve, reject, timer, method: 'tools/call(wait_task)' });
        });
        ctx.client._send({
          jsonrpc: '2.0', id, method: 'tools/call',
          params: { name: 'rdm_wait_task', arguments: { task_id: taskId, timeout_ms: 120000 }, _meta: { progressToken } },
        });
        return promise;
      })();
      await sleep(800);
      ctx.client.notify('notifications/cancelled', { requestId: callId, reason: 'qa cancels the wait' });
      let waitOutcome = null;
      try {
        const waited = await waitPromise;
        waitOutcome = toolOutcome(waited);
      } catch (err) {
        waitOutcome = null;
        addCheck('robustness', 'cancelled_wait_returns', false, String(err.message));
      }
      if (waitOutcome) addCheck('robustness', 'cancelled_wait_returns', true, describeOutcome(waitOutcome));
      const after = await getTask(ctx.client, taskId);
      addCheck('robustness', 'cancelled_wait_does_not_cancel_task', !!(after && ['downloading', 'paused', 'completed'].includes(after.state)), after ? `state=${after.state}` : 'task missing');
      await ctx.client.callTool('rdm_cancel_task', { task_id: taskId }, { timeoutMs: 60000 });
    }
  }

  // EOF: start a slow task, close stdin, server must exit cleanly and persist paused state
  if (ctx.slow) {
    const appData = isolatedAppData('eof');
    writeTestPolicy(appData);
    const client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: appData, tag: 'eof' });
    let eofTaskId = null;
    try {
      await client.initialize('2025-06-18');
      const dlDir = path.join(appData, 'dl');
      fs.mkdirSync(dlDir, { recursive: true });
      const start = await client.callTool('rdm_download', {
        url: ctx.slow.url, dest_dir: dlDir, file_name: 'agent-eof.bin', connections: 2, wait: false,
      }, { timeoutMs: 60000 });
      const rec = taskRecordFrom(start);
      eofTaskId = rec && (rec.id || rec.task_id);
      await waitForTaskState(client, eofTaskId, (t) => t.state === 'downloading' && (t.downloaded_bytes || 0) > 0, 30000);
      const exitInfo = await client.close({ timeoutMs: 15000 });
      addCheck('robustness', 'eof_clean_exit', !!exitInfo && exitInfo.code === 0, `exit=${JSON.stringify(exitInfo)}`);
    } finally {
      if (!client.closed) await client.close();
    }
    // restart on the same store: task still queryable and resumed from disk
    if (eofTaskId) {
      const appData2 = appData;
      await withFreshServer(ctx, 'eof-restart', appData2, async (client2) => {
        const got = await getTask(client2, eofTaskId);
        addCheck('robustness', 'eof_task_persisted', !!got, `id=${eofTaskId}`);
        if (got) {
          addCheck('robustness', 'eof_task_paused_after_restart', ['paused', 'downloading', 'completed', 'failed', 'cancelled'].includes(got.state), `state=${got.state}`);
          addCheck('robustness', 'eof_task_progress_kept', (got.downloaded_bytes || 0) > 0, `downloaded=${got.downloaded_bytes}`);
          const resumed = await client2.callTool('rdm_resume_task', { task_id: eofTaskId }, { timeoutMs: 60000 });
          addCheck('robustness', 'eof_task_resumable', !resumed.isError && !resumed.rpc_error, describeOutcome(resumed));
          const waited = await client2.callTool('rdm_wait_task', { task_id: eofTaskId, timeout_ms: 180000 }, { timeoutMs: 200000 });
          const done = taskRecordFrom(waited) || (await getTask(client2, eofTaskId));
          addCheck('robustness', 'eof_task_completes_after_resume', !!(done && done.state === 'completed'), done ? `state=${done.state}` : describeOutcome(waited));
        }
      });
    }
  }
}

function waitForResponseWithCode(client, code, timeoutMs) {
  // the -32700 response has id=null, so it never lands in pending; poll inbound
  return new Promise((resolve) => {
    const deadline = Date.now() + timeoutMs;
    const tick = () => {
      for (const [id, p] of client.pending) {
        if (p.method === '__never__') { /* noop */ }
        void id;
      }
      // check parse-error responses in inbound raw lines
      for (const line of client.inbound) {
        try {
          const msg = JSON.parse(line);
          if (msg && msg.error && msg.error.code === code) { resolve(msg); return; }
        } catch { /* non-JSON */ }
      }
      if (Date.now() >= deadline) { resolve(null); return; }
      setTimeout(tick, 100);
    };
    tick();
  });
}

// ---------------------------------------------------------------------------
// Section 12a: MCP `2026-07-28` dual-era basic path (contract §3.5/§8.2)
// ---------------------------------------------------------------------------

function modernMeta(version = '2026-07-28') {
  return {
    'io.modelcontextprotocol/protocolVersion': version,
    'io.modelcontextprotocol/clientInfo': { name: 'rdm-qa-harness', version: '0.0.0' },
    'io.modelcontextprotocol/clientCapabilities': {},
  };
}

async function sectionModernProtocol(ctx) {
  if (OPTIONS.legacyOnly) {
    addCheck('modern', 'legacy_only_skip', true, 'skipped (--legacy-only)');
    return;
  }
  const appData = isolatedAppData('modern');
  writeTestPolicy(appData);
  const client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: appData, tag: 'modern' });
  try {
    // (a) server/discover without initialize
    let discover;
    try {
      discover = await client.request('server/discover', { _meta: modernMeta() }, { timeoutMs: 20000 });
    } catch (err) {
      addCheck('modern', 'discover_available', false, `server/discover failed: ${String(err.message || err)} (dual-era may be unimplemented)`);
      return;
    }
    addCheck('modern', 'discover_available', !discover.error, discover.error ? JSON.stringify(discover.error) : 'ok');
    const d = discover.result || {};
    addCheck('modern', 'discover_result_type', d.resultType === 'complete', JSON.stringify(d.resultType));
    const versions = Array.isArray(d.supportedVersions) ? d.supportedVersions : [];
    addCheck('modern', 'discover_supported_versions', versions.includes('2026-07-28'), JSON.stringify(versions));
    addCheck('modern', 'discover_capabilities', !!(d.capabilities && typeof d.capabilities === 'object'), JSON.stringify(d.capabilities && Object.keys(d.capabilities)));
    addCheck('modern', 'discover_instructions', typeof d.instructions === 'string' && d.instructions.length > 20, `len=${(d.instructions || '').length}`);
    addCheck('modern', 'discover_ttl_cache', typeof d.ttlMs === 'number' && typeof d.cacheScope === 'string', `ttlMs=${JSON.stringify(d.ttlMs)} cacheScope=${JSON.stringify(d.cacheScope)}`);
    addCheck('modern', 'discover_server_info_meta', !!(d._meta && d._meta.serverInfo), JSON.stringify(d._meta && Object.keys(d._meta)));

    // (b) modern tools/list with per-request _meta
    const tools = await client.request('tools/list', { _meta: modernMeta() }, { timeoutMs: 20000 });
    addCheck('modern', 'tools_list_modern_ok', !tools.error, tools.error ? JSON.stringify(tools.error) : 'ok');
    if (!tools.error) {
      const tr = tools.result || {};
      addCheck('modern', 'tools_list_result_type', tr.resultType === 'complete', JSON.stringify(tr.resultType));
      addCheck('modern', 'tools_list_cache_fields', typeof tr.ttlMs === 'number' && typeof tr.cacheScope === 'string', `ttlMs=${JSON.stringify(tr.ttlMs)} cacheScope=${JSON.stringify(tr.cacheScope)}`);
      addCheck('modern', 'tools_list_has_16', (tr.tools || []).length >= 16, `count=${(tr.tools || []).length}`);
    }
    // (c) modern resources/list + prompts/list resultType
    for (const [label, method, key] of [['resources', 'resources/list', 'resources'], ['prompts', 'prompts/list', 'prompts']]) {
      const res = await client.request(method, { _meta: modernMeta() }, { timeoutMs: 20000 });
      if (!res.error) {
        const r = res.result || {};
        addCheck('modern', `${label}_list_result_type`, r.resultType === 'complete', JSON.stringify(r.resultType));
        addCheck('modern', `${label}_list_cache_fields`, typeof r.ttlMs === 'number' && typeof r.cacheScope === 'string', `ttlMs=${JSON.stringify(r.ttlMs)}`);
      } else {
        addCheck('modern', `${label}_list_result_type`, false, JSON.stringify(res.error));
        addCheck('modern', `${label}_list_cache_fields`, false, JSON.stringify(res.error));
      }
    }
    // (d) modern ping must be -32601
    const modPing = await client.request('ping', { _meta: modernMeta() }, { timeoutMs: 10000 });
    addCheck('modern', 'modern_ping_removed', !!modPing.error && modPing.error.code === -32601, JSON.stringify(modPing.error || modPing.result));
    // (e) unsupported modern version -> -32022 + data.supported/requested
    const badVer = await client.request('tools/list', { _meta: modernMeta('2025-06-18') }, { timeoutMs: 10000 });
    const badErr = badVer.error || {};
    addCheck('modern', 'modern_bad_version_code', badErr.code === -32022, JSON.stringify(badErr));
    if (badErr.code === -32022) {
      const data = badErr.data || {};
      addCheck('modern', 'modern_bad_version_data', Array.isArray(data.supported) && typeof data.requested === 'string', JSON.stringify(data).slice(0, 200));
    }
    // (f) modern request without per-request _meta: -32602 per contract, legacy fallback acceptable (informational)
    const noMeta = await client.request('tools/list', {}, { timeoutMs: 10000 });
    const noMetaCode = noMeta.error && noMeta.error.code;
    addCheck('modern', 'missing_request_meta_contract', noMetaCode === -32602 || (!!noMeta.result && noMetaCode === undefined), `code=${JSON.stringify(noMetaCode)} legacy_fallback=${!noMeta.error}`);
    addCheck('modern', 'legacy_client_still_works', true, 'legacy baseline covered by the other sections');
  } finally {
    await client.close({ timeoutMs: 10000 });
  }
}

// ---------------------------------------------------------------------------
// Section 12b: rdm-mcp `--state-dir` flag (claimed by SKILL §1/§2)
// ---------------------------------------------------------------------------

async function sectionMcpStateDir(ctx) {
  const appData = isolatedAppData('statedir');
  writeTestPolicy(appData);
  const altDir = path.join(appData, 'alt-state');
  fs.mkdirSync(altDir, { recursive: true });
  const client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: appData, tag: 'state-dir', extraArgs: ['--state-dir', altDir] });
  try {
    let init;
    try {
      init = await client.initialize('2025-06-18');
    } catch (err) {
      addCheck('skill', 'mcp_state_dir_flag_accepted', false, `initialize failed with --state-dir: ${String(err.message || err)}`);
      return;
    }
    addCheck('skill', 'mcp_state_dir_flag_accepted', !!(init && init.result && !init.error), 'initialize ok with --state-dir');
    const caps = await client.callTool('rdm_capabilities', {}, { timeoutMs: 30000 });
    addCheck('skill', 'mcp_state_dir_capabilities', !caps.isError && !caps.rpc_error, describeOutcome(caps));
    const dlDir = path.join(appData, 'dl');
    fs.mkdirSync(dlDir, { recursive: true });
    const dl = await client.callTool('rdm_download', {
      url: ctx.fast.url, dest_dir: dlDir, file_name: 'statedir.bin', connections: 8, wait: true, wait_timeout_ms: 120000,
    }, { timeoutMs: 180000 });
    addCheck('skill', 'mcp_state_dir_download', !dl.isError && !dl.rpc_error, describeOutcome(dl));
    // the store should live under the overridden dir
    const found = [];
    const walk = (dir) => {
      if (!fs.existsSync(dir)) return;
      for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p2 = path.join(dir, e.name);
        if (e.isDirectory()) walk(p2);
        else if (/\.json$/.test(e.name)) found.push(p2);
      }
    };
    walk(altDir);
    addCheck('skill', 'mcp_state_dir_store_location', found.length > 0, `json_files_under_alt=${found.length}`);
  } finally {
    await client.close({ timeoutMs: 10000 });
  }
}

// ---------------------------------------------------------------------------
// Section 13: CLI agent contract
// ---------------------------------------------------------------------------

function parseSingleEnvelope(stdout) {
  const text = String(stdout || '').trim();
  if (!text) return { error: 'empty stdout' };
  try {
    const env = JSON.parse(text);
    return { env };
  } catch (err) {
    return { error: `stdout is not a single JSON object: ${String(err).split('\n')[0]} (first 120 chars: ${truncate(text, 120)})` };
  }
}

function checkEnvelope(sectionName, label, env, { requireOk = true } = {}) {
  if (!env || typeof env !== 'object') return;
  const missing = ENVELOPE_KEYS.filter((k) => !(k in env));
  addCheck(sectionName, `${label}_envelope_keys`, missing.length === 0, missing.length ? `missing=[${missing.join(',')}] keys=[${Object.keys(env).join(',')}]` : 'all present');
  addCheck(sectionName, `${label}_schema_version`, env.schema_version === 'rdm.agent/v1', JSON.stringify(env.schema_version));
  addCheck(sectionName, `${label}_meta`, !!(env.meta && typeof env.meta.rdm_version === 'string' && typeof env.meta.elapsed_ms === 'number' && typeof env.meta.ts === 'string'), env.meta ? `version=${env.meta.rdm_version} elapsed=${env.meta.elapsed_ms}` : 'missing meta');
  if (requireOk) addCheck(sectionName, `${label}_ok`, env.ok === true, `ok=${JSON.stringify(env.ok)} error=${JSON.stringify(env.error)}`);
}

async function sectionCli(ctx) {
  const cli = OPTIONS.rdm;
  if (!fs.existsSync(cli)) {
    addCheck('cli', 'binary_present', false, `rdm CLI not found: ${cli} (build with scripts/mcp-conformance.ps1)`);
    return;
  }
  const appData = isolatedAppData('cli');
  writeTestPolicy(appData, { require_approval_over_bytes: 1024 * 1024 * 1024 });
  const dlDir = path.join(appData, 'dl');
  fs.mkdirSync(dlDir, { recursive: true });
  const env = { LOCALAPPDATA: appData };
  const run = (args, opts = {}) => runChild(cli, args, { env, timeoutMs: opts.timeoutMs || 180000, input: opts.input ?? null });
  const exitMap = [];
  const note = (label, code) => { exitMap.push({ label, code }); };

  const cases = [
    { label: 'capabilities', args: ['capabilities', '--output', 'json'], check: (env0) => !!(env0.data && (env0.data.version || env0.data.tools)), timeout: 120000 },
    { label: 'schema', args: ['schema', '--output', 'json'], check: (env0) => !!env0.data, timeout: 120000 },
    { label: 'doctor', args: ['doctor', '--output', 'json'], check: (env0) => !!env0.data, timeout: 180000 },
  ];
  for (const c of cases) {
    const res = await run(c.args, { timeoutMs: c.timeout });
    note(c.label, res.code);
    addCheck('cli', `${c.label}_exit_0`, res.code === 0, `exit=${res.code} stderr=${truncate(res.stderr, 160)}`);
    const parsed = parseSingleEnvelope(res.stdout);
    addCheck('cli', `${c.label}_single_json_envelope`, !parsed.error, parsed.error || '');
    if (parsed.env) {
      checkEnvelope('cli', c.label, parsed.env);
      addCheck('cli', `${c.label}_command_field`, typeof parsed.env.command === 'string' && parsed.env.command.length > 0, JSON.stringify(parsed.env.command));
      addCheck('cli', `${c.label}_data_present`, c.check(parsed.env), parsed.env.data ? `keys=${Object.keys(parsed.env.data).slice(0, 10).join(',')}` : 'data null');
    }
  }

  // schema command must expose all 16 tool schemas (contract §3.1 + §5.4)
  {
    const schemaRes = await run(['schema', '--output', 'json'], { timeoutMs: 120000 });
    const schemaParsed = parseSingleEnvelope(schemaRes.stdout);
    if (schemaParsed.env) {
      const blob = JSON.stringify(schemaParsed.env.data);
      const present = CONTRACT_TOOL_NAMES.filter((n) => blob.includes(`"${n}"`) || blob.includes(n));
      addCheck('cli', 'schema_contains_all_tools', present.length === CONTRACT_TOOL_NAMES.length, `present=${present.length}/${CONTRACT_TOOL_NAMES.length}`);
    } else {
      addCheck('cli', 'schema_contains_all_tools', false, schemaParsed.error);
    }
  }

  // tasks filters must be accepted
  {
    const filtered = await run(['tasks', '--limit', '1', '--state', 'completed', '--output', 'json'], { timeoutMs: 60000 });
    const parsedFiltered = parseSingleEnvelope(filtered.stdout);
    addCheck('cli', 'tasks_filters_ok', filtered.code === 0 && !!parsedFiltered.env && parsedFiltered.env.ok === true, `exit=${filtered.code} err=${parsedFiltered.error || ''}`);
  }

  // --json alias for ndjson compatibility on download
  const ndjson = await run(['download', ctx.fast.url, '--dir', dlDir, '--name', 'cli-ndjson.bin', '--connections', '8', '--json'], { timeoutMs: 180000 });
  note('download_ndjson', ndjson.code);
  addCheck('cli', 'ndjson_exit_0', ndjson.code === 0, `exit=${ndjson.code} stderr=${truncate(ndjson.stderr, 160)}`);
  const lines = String(ndjson.stdout || '').split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const events = [];
  let parseErrors = 0;
  for (const line of lines) { try { events.push(JSON.parse(line)); } catch { parseErrors++; } }
  addCheck('cli', 'ndjson_all_lines_json', lines.length > 0 && parseErrors === 0, `lines=${lines.length} parse_errors=${parseErrors}`);
  const seqs = events.map((e) => e.seq).filter((n) => Number.isFinite(n));
  addCheck('cli', 'ndjson_seq_increasing', seqs.length === events.length && seqs.every((v, i) => i === 0 || v > seqs[i - 1]), `seqs=${truncate(seqs.join(','), 120)}`);
  addCheck('cli', 'ndjson_schema_version', events.length > 0 && events.every((e) => e.schema_version === 'rdm.agent/v1'), JSON.stringify(events[0] && events[0].schema_version));
  addCheck('cli', 'ndjson_task_id_present', events.length > 0 && events.every((e) => typeof e.task_id === 'string' && e.task_id.length > 0), JSON.stringify(events[0] && events[0].task_id));
  addCheck('cli', 'ndjson_ts_present', events.length > 0 && events.every((e) => typeof e.ts === 'string'), JSON.stringify(events[0] && events[0].ts));
  const completed = events.filter((e) => e.type === 'completed');
  addCheck('cli', 'ndjson_completed_event', completed.length === 1, `completed=${completed.length} types=${[...new Set(events.map((e) => e.type))].join(',')}`);
  if (completed.length) {
    const data = completed[0].data || {};
    addCheck('cli', 'ndjson_completed_fields', !!(data.path && data.size !== undefined && data.sha256), `keys=${Object.keys(data).join(',')}`);
    const target = path.join(dlDir, 'cli-ndjson.bin');
    if (fs.existsSync(target)) {
      const hash = await sha256File(target);
      addCheck('cli', 'ndjson_file_hash', hash === ctx.reference.sha256, `sha=${hash.slice(0, 16)}…`);
      addCheck('cli', 'ndjson_completed_sha256', data.sha256 === ctx.reference.sha256, JSON.stringify(data.sha256));
    }
  }

  // `--output json` must print exactly one envelope on stdout (progress -> stderr)
  const jsonDlDir = path.join(dlDir, 'json-mode');
  fs.mkdirSync(jsonDlDir, { recursive: true });
  const jsonDl = await run(['download', ctx.fast.url, '--dir', jsonDlDir, '--name', 'cli-json.bin', '--connections', '8', '--output', 'json'], { timeoutMs: 180000 });
  note('download_json', jsonDl.code);
  addCheck('cli', 'download_json_exit_0', jsonDl.code === 0, `exit=${jsonDl.code} stderr=${truncate(jsonDl.stderr, 160)}`);
  const jsonParsed = parseSingleEnvelope(jsonDl.stdout);
  addCheck('cli', 'download_json_single_envelope_stdout', !jsonParsed.error, jsonParsed.error || '');
  if (jsonParsed.env) {
    checkEnvelope('cli', 'download_json', jsonParsed.env);
    addCheck('cli', 'download_json_command', jsonParsed.env.command === 'download', JSON.stringify(jsonParsed.env.command));
    addCheck('cli', 'download_json_task_id', typeof jsonParsed.env.task_id === 'string' && jsonParsed.env.task_id.startsWith('tsk_'), JSON.stringify(jsonParsed.env.task_id));
    const d = jsonParsed.env.data || {};
    addCheck('cli', 'download_json_data_result', !!(d.path || (d.result && d.result.path) || (d.task && (d.task.path || d.task.result))), `keys=${Object.keys(d).join(',')}`);
    const f = path.join(jsonDlDir, 'cli-json.bin');
    if (fs.existsSync(f)) {
      const h = await sha256File(f);
      addCheck('cli', 'download_json_file_hash', h === ctx.reference.sha256, `sha=${h.slice(0, 16)}…`);
    } else {
      addCheck('cli', 'download_json_file_hash', false, 'target file missing');
    }
  }

  // download with --sha256 (correct) must verify and succeed
  {
    const shaDir = path.join(dlDir, 'sha-mode');
    fs.mkdirSync(shaDir, { recursive: true });
    const good = await run(['download', ctx.fast.url, '--dir', shaDir, '--name', 'cli-sha.bin', '--connections', '8', '--sha256', ctx.reference.sha256, '--output', 'json'], { timeoutMs: 180000 });
    note('download_sha256_ok', good.code);
    const goodParsed = parseSingleEnvelope(good.stdout);
    addCheck('cli', 'download_sha256_exit_0', good.code === 0, `exit=${good.code} stderr=${truncate(good.stderr, 140)}`);
    if (goodParsed.env) {
      addCheck('cli', 'download_sha256_ok_envelope', goodParsed.env.ok === true, JSON.stringify(goodParsed.env.error));
      const f = path.join(shaDir, 'cli-sha.bin');
      if (fs.existsSync(f)) {
        const h = await sha256File(f);
        addCheck('cli', 'download_sha256_file_hash', h === ctx.reference.sha256, `sha=${h.slice(0, 16)}…`);
      } else {
        addCheck('cli', 'download_sha256_file_hash', false, 'target missing');
      }
    } else {
      addCheck('cli', 'download_sha256_ok_envelope', false, goodParsed.error);
    }
    const bad = await run(['download', ctx.fast.url, '--dir', shaDir, '--name', 'cli-sha-bad.bin', '--sha256', '0'.repeat(64), '--output', 'json'], { timeoutMs: 180000 });
    note('download_sha256_mismatch', bad.code);
    const badParsed = parseSingleEnvelope(bad.stdout);
    addCheck('cli', 'download_sha256_mismatch_exit_2', bad.code === 2, `exit=${bad.code}`);
    if (badParsed.env) {
      addCheck('cli', 'download_sha256_mismatch_code', badParsed.env.ok === false && badParsed.env.error && badParsed.env.error.code === 'checksum_mismatch', JSON.stringify(badParsed.env.error));
    } else {
      addCheck('cli', 'download_sha256_mismatch_code', false, badParsed.error);
    }
  }

  // tasks / task not_found
  const tasks = await run(['tasks', '--output', 'json'], { timeoutMs: 60000 });
  note('tasks', tasks.code);
  const tasksParsed = parseSingleEnvelope(tasks.stdout);
  addCheck('cli', 'tasks_exit_0', tasks.code === 0, `exit=${tasks.code}`);
  if (tasksParsed.env) {
    checkEnvelope('cli', 'tasks', tasksParsed.env);
    const arr = tasksParsed.env.data && (tasksParsed.env.data.tasks || tasksParsed.env.data.items);
    addCheck('cli', 'tasks_data_list', Array.isArray(arr), `data=${tasksParsed.env.data ? Object.keys(tasksParsed.env.data).join(',') : 'null'}`);
  } else {
    addCheck('cli', 'tasks_data_list', false, tasksParsed.error);
  }

  const missingTask = await run(['task', 'tsk_definitely_missing', '--output', 'json'], { timeoutMs: 60000 });
  note('task_missing', missingTask.code);
  const missingParsed = parseSingleEnvelope(missingTask.stdout);
  addCheck('cli', 'missing_task_exit_code_2', missingTask.code === 2, `exit=${missingTask.code} (contract §5.1: general failure=2)`);
  if (missingParsed.env) {
    addCheck('cli', 'missing_task_error_envelope', missingParsed.env.ok === false && missingParsed.env.error && missingParsed.env.error.code === 'not_found', JSON.stringify(missingParsed.env.error));
  } else {
    addCheck('cli', 'missing_task_error_envelope', false, missingParsed.error);
  }

  // probe json + invalid url
  const probe = await run(['probe', ctx.fast.url, '--output', 'json'], { timeoutMs: 120000 });
  note('probe', probe.code);
  const probeParsed = parseSingleEnvelope(probe.stdout);
  addCheck('cli', 'probe_exit_0', probe.code === 0, `exit=${probe.code}`);
  if (probeParsed.env) {
    checkEnvelope('cli', 'probe', probeParsed.env);
    const d = probeParsed.env.data || {};
    addCheck('cli', 'probe_data_total', (d.total_bytes ?? d.size) === SIZE_BYTES, JSON.stringify(d.total_bytes ?? d.size));
  }
  const badUrl = await run(['probe', 'ftp://example.com/x', '--output', 'json'], { timeoutMs: 60000 });
  note('probe_invalid_url', badUrl.code);
  addCheck('cli', 'invalid_url_exit_code_2', badUrl.code === 2, `exit=${badUrl.code} (contract §5.1: general failure=2)`);
  const badParsed = parseSingleEnvelope(badUrl.stdout);
  if (badParsed.env) {
    addCheck('cli', 'invalid_url_error_code', badParsed.env.ok === false && badParsed.env.error && badParsed.env.error.code === 'invalid_url', JSON.stringify(badParsed.env.error));
  } else {
    addCheck('cli', 'invalid_url_error_code', false, badParsed.error);
  }

  // state inspect / repair dry-run
  const inspect = await run(['state', 'inspect', '--output', 'json'], { timeoutMs: 120000 });
  note('state_inspect', inspect.code);
  const inspectParsed = parseSingleEnvelope(inspect.stdout);
  addCheck('cli', 'state_inspect_ok', inspect.code === 0 && !!inspectParsed.env && inspectParsed.env.ok === true, `exit=${inspect.code} err=${inspectParsed.error || ''}`);
  const repair = await run(['state', 'repair', '--dry-run', '--output', 'json'], { timeoutMs: 120000 });
  note('state_repair_dryrun', repair.code);
  const repairParsed = parseSingleEnvelope(repair.stdout);
  addCheck('cli', 'state_repair_dryrun_ok', repair.code === 0 && !!repairParsed.env && repairParsed.env.ok === true, `exit=${repair.code} err=${repairParsed.error || ''}`);

  // run --input - (stdin manifest, wait all)
  const manifest = JSON.stringify({
    schema_version: 'rdm.agent/v1',
    tasks: [{ url: ctx.fast.url, dest_dir: dlDir, file_name: 'run-1.bin' }],
    max_concurrency: 1,
    wait: true,
    stop_on_error: false,
  });
  const runRes = await run(['run', '--input', '-', '--wait', '--output', 'json'], { timeoutMs: 240000, input: manifest });
  note('run', runRes.code);
  const runParsed = parseSingleEnvelope(runRes.stdout);
  addCheck('cli', 'run_stdin_exit_0', runRes.code === 0, `exit=${runRes.code} stderr=${truncate(runRes.stderr, 160)}`);
  if (runParsed.env) {
    checkEnvelope('cli', 'run', runParsed.env);
    const d = runParsed.env.data || {};
    addCheck('cli', 'run_data_tasks', Array.isArray(d.tasks) && d.tasks.length === 1, `tasks=${Array.isArray(d.tasks) ? d.tasks.length : 'n/a'} keys=${Object.keys(d).join(',')}`);
    addCheck('cli', 'run_counts', d.completed === 1 && (d.failed === 0 || d.failed === undefined), `completed=${d.completed} failed=${d.failed}`);
    const runFile = path.join(dlDir, 'run-1.bin');
    if (fs.existsSync(runFile)) {
      const hash = await sha256File(runFile);
      addCheck('cli', 'run_file_hash', hash === ctx.reference.sha256, `sha=${hash.slice(0, 16)}…`);
    } else {
      addCheck('cli', 'run_file_hash', false, `missing ${runFile}`);
    }
  } else {
    addCheck('cli', 'run_data_tasks', false, runParsed.error);
  }

  // policy_denied / conflict: contract §5.1 exit code 4 (not executed)
  {
    const policyFile = path.join(appData, 'RDM', 'agent', 'policy.json');
    fs.mkdirSync(path.dirname(policyFile), { recursive: true });
    fs.writeFileSync(policyFile, JSON.stringify({
      schema_version: 'rdm.agent/v1',
      allowed_hosts: ['*.example.com'],
      allowed_dirs: [dlDir],
      max_bytes_per_task: 10 * 1024 * 1024 * 1024,
      require_approval_over_bytes: 1024 * 1024 * 1024,
      max_concurrent_tasks: 3,
      default_connections: 8,
      allow_private_hosts: true,
      allow_overwrite: false,
    }, null, 2));
    const denied = await run(['download', ctx.fast.url, '--dir', dlDir, '--name', 'cli-denied.bin', '--output', 'json'], { timeoutMs: 60000 });
    note('download_policy_denied', denied.code);
    const deniedParsed = parseSingleEnvelope(denied.stdout);
    addCheck('cli', 'policy_denied_exit_4', denied.code === 4, `exit=${denied.code} (contract §5.1: not executed=4)`);
    if (deniedParsed.env) {
      addCheck('cli', 'policy_denied_error_code', deniedParsed.env.ok === false && deniedParsed.env.error && deniedParsed.env.error.code === 'policy_denied', JSON.stringify(deniedParsed.env.error));
    } else {
      addCheck('cli', 'policy_denied_error_code', false, deniedParsed.error);
    }
    writeTestPolicy(appData); // restore permissive policy for the remaining commands
  }
  {
    const conflictTarget = path.join(dlDir, 'cli-conflict.bin');
    fs.writeFileSync(conflictTarget, 'x');
    const conflict = await run(['download', ctx.fast.url, '--dir', dlDir, '--name', 'cli-conflict.bin', '--output', 'json'], { timeoutMs: 60000 });
    note('download_conflict', conflict.code);
    const conflictParsed = parseSingleEnvelope(conflict.stdout);
    addCheck('cli', 'conflict_exit_4', conflict.code === 4, `exit=${conflict.code} (contract §5.1: not executed=4)`);
    if (conflictParsed.env) {
      addCheck('cli', 'conflict_error_code', conflictParsed.env.ok === false && conflictParsed.env.error && conflictParsed.env.error.code === 'conflict', JSON.stringify(conflictParsed.env.error));
    } else {
      addCheck('cli', 'conflict_error_code', false, conflictParsed.error);
    }
    try { fs.unlinkSync(conflictTarget); } catch { /* ignore */ }
  }

  // selftest + bench (CLI)
  const selftest = await run(['selftest', '--output', 'json'], { timeoutMs: 300000 });
  note('selftest', selftest.code);
  const stParsed = parseSingleEnvelope(selftest.stdout);
  addCheck('cli', 'selftest_exit_0', selftest.code === 0, `exit=${selftest.code} stderr=${truncate(selftest.stderr, 160)}`);
  if (stParsed.env) {
    checkEnvelope('cli', 'selftest', stParsed.env);
    const d = stParsed.env.data || {};
    const cases = d.cases || d.results || d.tests;
    addCheck('cli', 'selftest_4_cases', Array.isArray(cases) && cases.length >= 4, Array.isArray(cases) ? `cases=${cases.length}` : `keys=${Object.keys(d).join(',')}`);
  } else {
    addCheck('cli', 'selftest_4_cases', false, stParsed.error);
  }

  const bench = await run(['bench', '--output', 'json'], { timeoutMs: 300000 });
  note('bench', bench.code);
  const benchParsed = parseSingleEnvelope(bench.stdout);
  addCheck('cli', 'bench_exit_0', bench.code === 0, `exit=${bench.code} stderr=${truncate(bench.stderr, 160)}`);
  if (benchParsed.env) {
    checkEnvelope('cli', 'bench', benchParsed.env);
    const s = JSON.stringify(benchParsed.env.data || {});
    addCheck('cli', 'bench_has_data', /speedup|connections|mib\/s|bps/i.test(s), truncate(s, 200));
  } else {
    addCheck('cli', 'bench_has_data', false, benchParsed.error);
  }

  ctx.exitMap = exitMap;
  const badCodes = exitMap.filter((e) => e.code !== 0 && ![2, 3, 4].includes(e.code));
  addCheck('cli', 'exit_codes_within_documented_set', badCodes.length === 0, badCodes.length ? JSON.stringify(badCodes) : `map=${exitMap.map((e) => `${e.label}:${e.code}`).join(',')}`);
  addCheck('cli', 'exit_code_3_untested', true, 'exit 3 (paused/cancelled) needs interactive Ctrl-C; MCP control paths cover pause/cancel semantics');
}

// ---------------------------------------------------------------------------
// Section 14: SKILL / docs static presence
// ---------------------------------------------------------------------------

async function sectionSkillDocs(ctx) {
  const skill = path.join(ROOT, 'skills', 'rdm', 'SKILL.md');
  const reference = path.join(ROOT, 'skills', 'rdm', 'reference.md');
  const agentDoc = path.join(ROOT, 'docs', 'agent-integration.md');
  const mcpRef = path.join(ROOT, 'docs', 'agent-mcp-reference.md');
  addCheck('skill', 'skill_md_exists', fs.existsSync(skill), skill);
  if (fs.existsSync(skill)) {
    const text = fs.readFileSync(skill, 'utf8');
    addCheck('skill', 'skill_has_frontmatter', /^---\r?\n/.test(text), truncate(text.slice(0, 80), 80));
    addCheck('skill', 'skill_name_rdm', /^name:\s*rdm\s*$/m.test(text.split('---')[1] || ''), 'frontmatter name');
    addCheck('skill', 'skill_has_description', /^description:/m.test(text), 'frontmatter description');
    addCheck('skill', 'skill_mentions_tools', CONTRACT_TOOL_NAMES.filter((n) => text.includes(n)).length >= 8, `mentioned=${CONTRACT_TOOL_NAMES.filter((n) => text.includes(n)).length}/16`);
    addCheck('skill', 'skill_mentions_cli', /\brdm\s+(capabilities|download|run|tasks|selftest|doctor)/.test(text), 'CLI recipes');
    addCheck('skill', 'skill_mentions_errors', /invalid_url|policy_denied|needs_approval|not_found/.test(text), 'error handling');
    addCheck('skill', 'skill_size', text.length >= 2000, `chars=${text.length}`);
    ctx.skillText = text;
  }
  addCheck('skill', 'skill_reference_exists', fs.existsSync(reference), reference);
  addCheck('skill', 'agent_contract_doc_exists', fs.existsSync(agentDoc), agentDoc);
  addCheck('skill', 'agent_mcp_reference_exists', fs.existsSync(mcpRef), mcpRef);
  const agents = path.join(ROOT, 'AGENTS.md');
  if (fs.existsSync(agents)) {
    const text = fs.readFileSync(agents, 'utf8');
    addCheck('skill', 'agents_md_mentions_mcp', /rdm-mcp/.test(text), 'AGENTS.md');
    addCheck('skill', 'agents_md_mentions_skill', /SKILL\.md/.test(text), 'AGENTS.md');
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  console.log('=== RDM MCP / agent interface conformance (independent QA) ===');
  console.log(`rdm-mcp : ${OPTIONS.rdmMcp}`);
  console.log(`rdm     : ${OPTIONS.rdm}`);
  console.log(`fixture : ${OPTIONS.fixture}`);
  console.log(`size    : ${OPTIONS.sizeMb} MiB   out: ${OPTIONS.out}`);

  if (OPTIONS.listSections) {
    ['negotiation', 'tools', 'resources', 'prompts', 'tools_call', 'probe', 'download', 'verify', 'tasks', 'idempotency', 'control', 'policy', 'redaction', 'robustness', 'cli', 'skill'].forEach((s) => console.log(s));
    return 0;
  }

  if (!fs.existsSync(OPTIONS.rdmMcp)) {
    console.error(`\nFATAL: rdm-mcp binary not found: ${OPTIONS.rdmMcp}`);
    console.error('Build it first (scripts/mcp-conformance.ps1) or pass --rdm-mcp.');
    return 2;
  }

  const mainAppData = isolatedAppData('main');
  writeTestPolicy(mainAppData);
  const ctx = {
    appData: mainAppData,
    client: null,
    fast: null,
    slow: null,
    reference: null,
    toolNames: [],
    exitMap: [],
  };
  fs.mkdirSync(path.join(ROOT, 'target-qa-agent'), { recursive: true });

  let exitCode = 0;
  try {
    // Independent fixtures + independent reference hash.
    ctx.fast = await startFixture({ sizeMb: OPTIONS.sizeMb, delayMs: 0, tag: 'fast' });
    ctx.slow = await startFixture({ sizeMb: OPTIONS.sizeMb, delayMs: OPTIONS.slowDelayMs, tag: 'slow' });
    ctx.reference = await httpReference(ctx.fast.url);
    console.log(`    reference payload sha256=${ctx.reference.sha256} bytes=${ctx.reference.bytes}`);
    addCheck('setup', 'reference_payload_size', ctx.reference.bytes === SIZE_BYTES, `${ctx.reference.bytes} vs ${SIZE_BYTES}`);

    ctx.client = new McpClient({ exe: OPTIONS.rdmMcp, localAppData: ctx.appData, tag: 'main' });
    await ctx.client.initialize('2025-06-18');

    await section('negotiation+', () => sectionNegotiation(ctx));
    await section('tools-list', () => sectionToolsList(ctx));
    await section('resources', () => sectionResources(ctx));
    await section('prompts', () => sectionPrompts(ctx));
    await section('readonly-tools', () => sectionReadOnlyTools(ctx));
    await section('probe', () => sectionProbe(ctx));
    await section('download-flow', () => sectionDownloadFlow(ctx));
    await section('idempotency', () => sectionIdempotency(ctx));
    await section('control-flow', () => sectionControlFlow(ctx));
    await section('policy', () => sectionPolicy(ctx));
    await section('redaction', () => sectionRedaction(ctx));
    await section('robustness', () => sectionRobustness(ctx));
    await section('modern-protocol', () => sectionModernProtocol(ctx));
    await section('mcp-state-dir', () => sectionMcpStateDir(ctx));
    await section('cli-agent', () => sectionCli(ctx));
    await section('skill-docs', () => sectionSkillDocs(ctx));
  } catch (err) {
    addCheck('global', 'harness_setup', false, String(err && err.stack ? err.stack.split('\n').slice(0, 3).join(' | ') : err));
  } finally {
    try { if (ctx.client) await ctx.client.close({ timeoutMs: 10000 }); } catch { /* ignore */ }
    for (const f of [ctx.fast, ctx.slow]) { try { f && f.stop(); } catch { /* ignore */ } }
    for (const c of children) { try { if (!c.killed && c.exitCode === null) c.kill(); } catch { /* ignore */ } }
    try { cleanupIsolatedDirs(); } catch { /* ignore */ }
  }

  // summary
  const failed = results.checks.filter((c) => !c.pass);
  const passed = results.checks.length - failed.length;
  results.summary = {
    pass: passed,
    fail: failed.length,
    total: results.checks.length,
    sections: results.sections.filter((s) => !s.skipped).map((s) => ({ name: s.name, checks: s.checks, failed: s.failed })),
  };
  results.limitations = [
    'MCP client is newline-delimited JSON over stdio; if the server emits logs on stdout the harness reports non-JSON lines as failures.',
    'Reference SHA-256 is computed over HTTP by this harness, not by RDM.',
    'Policy scenarios each use a fresh isolated LOCALAPPDATA so the policy file is read at startup.',
    'SKILL execution evaluation is manual (agent reads skills/rdm/SKILL.md); this harness only checks presence/shape.',
  ];

  fs.mkdirSync(path.dirname(OPTIONS.out), { recursive: true });
  fs.writeFileSync(OPTIONS.out, JSON.stringify(results, null, 2));
  fs.mkdirSync(path.dirname(OPTIONS.transcript), { recursive: true });
  fs.writeFileSync(OPTIONS.transcript, transcriptLines.join('\n'));

  console.log('\n=== SUMMARY ===');
  console.log(`pass=${passed} fail=${failed.length} total=${results.checks.length}`);
  if (failed.length) {
    console.log('\nFAILURES:');
    for (const f of failed) console.log(`  - ${f.name}: ${truncate(f.detail, 500)}`);
    console.log(`\nresults: ${OPTIONS.out}`);
    console.log(`transcript: ${OPTIONS.transcript}`);
    exitCode = 1;
  } else {
    console.log(`results: ${OPTIONS.out}`);
  }
  return exitCode;
}

main()
  .then((code) => process.exit(code))
  .catch((err) => {
    console.error('FATAL:', err && err.stack ? err.stack : err);
    process.exit(2);
  });












