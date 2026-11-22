#!/usr/bin/env node
'use strict';
/* rcargo port shim for `{{FUNCTION}}` (Node 18+, no dependencies; Linux and macOS).
 *
 * In your app (the only edit to the original source; delete the line to undo):
 *
 *   const { port } = require('<path to this shims folder>/rcargo_port.cjs');
 *   const yourFunction = port(function yourFunction(a, b) { ... });
 *
 * Behaviour of a wrapped function (positional arguments only):
 *   RCARGO_PORT_RECORD=<file>  run the original and append the call to <file> (golden cases)
 *   RCARGO_PORT_DISABLE=1      always run the original
 *   otherwise                  ask the Rust binary; fall back to the original when the binary is
 *                              missing, arguments are not plain JSON data, or the port reports `fatal`.
 * Results are plain JSON. Integers beyond 2^53 lose precision. When the port reports an error, the
 * original runs so its own exception is thrown (errors are rare and usually cheap).
 *
 * Command line (MODULE is a .js/.mjs/.cjs/.ts file; FUNCTION may be `name` or `obj.name`):
 *   node rcargo_port.cjs oracle MODULE FUNCTION CANDIDATES.jsonl CASES.jsonl
 *   node rcargo_port.cjs bench  MODULE FUNCTION CASES.jsonl
 */
const fs = require('fs');
const os = require('os');
const path = require('path');
const cp = require('child_process');

const BIN = process.env.RCARGO_PORT_BIN || path.join(__dirname, '..', 'target', 'release', '{{BIN}}');
const MAX_CASES = parseInt(process.env.RCARGO_PORT_MAX_CASES || '500', 10);

function plain(v) {
  if (v === null || typeof v === 'boolean' || typeof v === 'string') return true;
  if (typeof v === 'number') return Number.isFinite(v);
  if (Array.isArray(v)) return v.every(plain);
  if (typeof v === 'object') {
    const p = Object.getPrototypeOf(v);
    return (p === Object.prototype || p === null) && Object.values(v).every(plain);
  }
  return false;
}

// Synchronous request/response over two FIFOs: node has no sync pipe API, and the wrapped
// function must stay synchronous. The child sees EOF (and exits) when this process ends.
let proc = null, wfd = -1, rfd = -1, broken = false;
const sleeper = new Int32Array(new SharedArrayBuffer(4));

function start() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rcport-'));
  const fin = path.join(dir, 'in');
  const fout = path.join(dir, 'out');
  try {
    cp.execFileSync('mkfifo', [fin, fout]);
    const C = fs.constants;
    wfd = fs.openSync(fin, C.O_RDWR);
    const childIn = fs.openSync(fin, C.O_RDONLY);
    rfd = fs.openSync(fout, C.O_RDONLY | C.O_NONBLOCK);
    const childOut = fs.openSync(fout, C.O_WRONLY);
    proc = cp.spawn(BIN, [], { stdio: [childIn, childOut, 'inherit'] });
    proc.on('error', () => { broken = true; });
    proc.unref();
    fs.closeSync(childIn);
    fs.closeSync(childOut);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

function stop() {
  broken = true;
  try { if (proc) proc.kill(); } catch (_) {}
  for (const fd of [wfd, rfd]) { try { if (fd >= 0) fs.closeSync(fd); } catch (_) {} }
  proc = null; wfd = rfd = -1;
}

function ask(request) {
  if (!proc) start();
  const out = Buffer.from(request + '\n');
  for (let off = 0; off < out.length; ) off += fs.writeSync(wfd, out, off);
  const chunks = [];
  const chunk = Buffer.alloc(65536);
  const deadline = Date.now() + 120000;
  for (;;) {
    let n;
    try {
      n = fs.readSync(rfd, chunk, 0, chunk.length, null);
    } catch (e) {
      if (e.code !== 'EAGAIN') throw e;
      if (Date.now() > deadline) throw new Error('port binary timed out');
      Atomics.wait(sleeper, 0, 0, 0.05);
      continue;
    }
    if (n === 0) throw new Error('port binary exited');
    chunks.push(Buffer.from(chunk.subarray(0, n)));
    if (chunk[n - 1] === 10) break;
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}
process.on('exit', () => { try { if (proc) proc.kill(); } catch (_) {} });

function accelerated(fn, args) {
  if (broken || !fs.existsSync(BIN) || !plain(args)) return fn(...args);
  let res;
  try {
    res = ask(JSON.stringify({ args }));
  } catch (_) {
    stop();
    return fn(...args);
  }
  if ('ok' in res) return res.ok;
  // 'err': fall through, so the original throws its own exception.
  return fn(...args);
}

function caseLine(key, err, result, ns) {
  const ok = result === undefined ? null : result;
  if (err === null && !plain(ok)) return null;
  const expect = err !== null ? { err: String(err && err.message !== undefined ? err.message : err) } : { ok };
  return `{"args":${key},"expect":${JSON.stringify(expect)},"ns":${ns}}`;
}

const seen = new Map();
function record(fn, args, file) {
  let set = seen.get(file);
  if (!set) {
    set = new Set();
    seen.set(file, set);
    if (fs.existsSync(file)) {
      for (const l of fs.readFileSync(file, 'utf8').split('\n')) if (l.trim()) set.add(JSON.stringify(JSON.parse(l).args));
    }
  }
  if (!plain(args)) return fn(...args);
  const key = JSON.stringify(args);
  if (set.size >= MAX_CASES || set.has(key)) return fn(...args);
  const t0 = process.hrtime.bigint();
  let result, err = null;
  try { result = fn(...JSON.parse(key)); } catch (e) { err = e; }
  const ns = process.hrtime.bigint() - t0;
  const line = caseLine(key, err, result, ns);
  if (line !== null) {
    fs.mkdirSync(path.dirname(path.resolve(file)), { recursive: true });
    fs.appendFileSync(file, line + '\n');
    set.add(key);
  }
  if (err !== null) throw err;
  return result;
}

function port(fn) {
  const wrapper = function (...args) {
    if (process.env.RCARGO_PORT_DISABLE === '1') return fn.apply(this, args);
    const file = process.env.RCARGO_PORT_RECORD;
    if (file) return record(fn.bind(this), args, file);
    return accelerated(fn.bind(this), args);
  };
  Object.defineProperty(wrapper, 'name', { value: fn.name });
  wrapper.original = fn;
  return wrapper;
}

async function load(module, fn) {
  const { pathToFileURL } = require('url');
  const mod = await import(pathToFileURL(path.resolve(module)).href);
  let obj = mod;
  let parent = mod;
  for (const part of fn.split('.')) {
    parent = obj;
    obj = obj[part] !== undefined ? obj[part] : (obj.default || {})[part];
  }
  if (typeof obj !== 'function') throw new Error(`${fn} is not an exported function of ${module}`);
  return (...a) => obj.apply(parent, a);
}

const readCases = (f) => fs.readFileSync(f, 'utf8').split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));

async function oracle(module, fn, candidates, out) {
  process.env.RCARGO_PORT_DISABLE = '1';
  delete process.env.RCARGO_PORT_RECORD;
  const f = await load(module, fn);
  const have = new Set(fs.existsSync(out) ? readCases(out).map((c) => JSON.stringify(c.args)) : []);
  fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
  let added = 0, errors = 0;
  for (const c of readCases(candidates)) {
    const key = JSON.stringify(c.args);
    if (have.has(key)) continue;
    const t0 = process.hrtime.bigint();
    let result, err = null;
    try { result = f(...JSON.parse(key)); } catch (e) { err = e; }
    const ns = process.hrtime.bigint() - t0;
    const line = caseLine(key, err, result, ns);
    if (line === null) continue;
    fs.appendFileSync(out, line + '\n');
    have.add(key);
    added++;
    if (err !== null) errors++;
  }
  console.error(`oracle: ${added} new cases (${errors} are errors)`);
}

async function bench(module, fn, casesFile) {
  process.env.RCARGO_PORT_DISABLE = '1';
  const f = await load(module, fn);
  if (!fs.existsSync(BIN)) { console.error(`no port binary at ${BIN}: run \`rcargo port build\` first`); process.exit(1); }
  const calls = readCases(casesFile).map((c) => c.args);
  if (!calls.length) { console.error(`no cases in ${casesFile}`); process.exit(1); }
  const reqs = calls.map((a) => JSON.stringify({ args: a }));
  const best = (run) => {
    let b = Infinity;
    for (let i = 0; i < 3; i++) { const t0 = process.hrtime.bigint(); run(); b = Math.min(b, Number(process.hrtime.bigint() - t0) / 1e9); }
    return b;
  };
  const tOrig = best(() => { for (const a of calls) { try { f(...a); } catch (_) {} } });
  const tPort = best(() => { for (const r of reqs) ask(r); });
  console.log(`original ${(tOrig * 1e3).toFixed(1)}ms, port ${(tPort * 1e3).toFixed(1)}ms: ${(tOrig / tPort).toFixed(1)}x per call set (${calls.length} calls, best of 3, bridge included)`);
  if (tPort >= tOrig) console.log('the port is not faster end to end: do not enable it (the call crosses the bridge too often, or the work is too small)');
}

module.exports = { port };

if (require.main === module) {
  const a = process.argv.slice(2);
  const done = (p) => p.catch((e) => { console.error(e && e.stack || e); process.exit(1); });
  if (a[0] === 'oracle' && a.length === 5) done(oracle(...a.slice(1)));
  else if (a[0] === 'bench' && a.length === 4) done(bench(...a.slice(1)));
  else { console.error('usage: rcargo_port.cjs oracle MODULE FUNCTION CANDIDATES.jsonl CASES.jsonl | bench MODULE FUNCTION CASES.jsonl'); process.exit(2); }
}
