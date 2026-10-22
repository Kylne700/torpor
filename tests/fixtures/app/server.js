'use strict';
// Fixture app for Torpor's tests (spec 25): one file, no dependencies.
// A login page, /api/login, /api/me, and one seeded regression: edit the line below to switch it.
const http = require('http');
const fs = require('fs');
const path = require('path');

const BROKEN = false;

const at = process.argv.indexOf('--port');
const PORT = Number(at > 0 ? process.argv[at + 1] : process.env.PORT) || 3000;
const sessions = new Map();
let hits = 0;

function send(res, code, body, type = 'application/json', extra = {}) {
  res.writeHead(code, { 'content-type': type, ...extra });
  res.end(typeof body === 'string' ? body : JSON.stringify(body));
}

function slurp(file) {
  try {
    return fs.readFileSync(file, 'utf8');
  } catch {
    return '';
  }
}

function session(req) {
  const m = /(?:^|;\s*)sid=([a-z0-9]+)/.exec(req.headers.cookie || '');
  return m ? sessions.get(m[1]) : undefined;
}

const LOGIN = '<!doctype html><title>Sign in</title><form><label for="email">Email</label>' +
  '<input id="email" name="email"><button type="button">Sign in</button></form>';

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  if (url.pathname !== '/api/hits') hits += 1;
  if (url.pathname === '/') return send(res, 200, '<h1>Fixture app</h1>', 'text/html');
  if (url.pathname === '/login') return send(res, 200, LOGIN, 'text/html');
  if (url.pathname === '/api/login' && req.method === 'POST') {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      let email = '';
      try {
        email = String(JSON.parse(body).email || '');
      } catch {}
      if (!email) return send(res, 400, { error: 'email needed' });
      const token = Math.random().toString(36).slice(2) + Date.now().toString(36);
      sessions.set(token, email);
      send(res, 200, { ok: true }, 'application/json', { 'set-cookie': `sid=${token}; Path=/; HttpOnly` });
    });
    return;
  }
  if (url.pathname === '/api/me') {
    const email = session(req);
    if (!email || BROKEN) return send(res, 401, { error: 'not signed in' });
    return send(res, 200, { email });
  }
  if (url.pathname === '/api/flaky') {
    // Fails on every third request counted in the file named by FLAKY_FILE (the counter outlives one run).
    const file = process.env.FLAKY_FILE;
    const n = file ? Number(slurp(file) || 0) + 1 : 1;
    if (file) fs.writeFileSync(file, String(n));
    return n % 3 === 0 ? send(res, 500, { error: 'flaked' }) : send(res, 200, { ok: true });
  }
  if (url.pathname === '/api/isolated') {
    // 200 only when no SENTINEL- text is visible: not in the environment, not in the .env files, not in the home folder.
    const leaks = [];
    if (/SENTINEL-/.test(JSON.stringify(process.env))) leaks.push('env');
    for (const f of ['.env', '.env.local', '.env.example', '.env.torpor']) if (/SENTINEL-/.test(slurp(f))) leaks.push(f);
    if (/SENTINEL-/.test(slurp(path.join(process.env.HOME || '/nonexistent', '.aws', 'credentials')))) leaks.push('home');
    return leaks.length ? send(res, 500, { leaks }) : send(res, 200, { isolated: true });
  }
  if (url.pathname === '/api/whoami') {
    return send(res, 200, {
      port: PORT,
      cwd: process.cwd(),
      home: process.env.HOME,
      tmpdir: process.env.TMPDIR,
      env: Object.keys(process.env).sort(),
      dotenv: slurp('.env'),
      files: fs.readdirSync('.').sort(),
    });
  }
  if (url.pathname === '/api/redirect-out') return send(res, 302, '', 'text/plain', { location: 'http://example.invalid/steal' });
  if (url.pathname === '/api/redirect-self') return send(res, 302, '', 'text/plain', { location: '/api/me' });
  if (url.pathname === '/api/hits') return send(res, 200, { hits });
  if (url.pathname === '/api/slow') {
    return setTimeout(() => send(res, 200, { slow: true }), Number(url.searchParams.get('ms')) || 1000);
  }
  send(res, 404, { error: 'not found' });
});

server.listen(PORT, '127.0.0.1');
// A fixture that outlives its test would hold a port: it ends by itself.
setTimeout(() => process.exit(0), Number(process.env.APP_LIFETIME_MS) || 600000);
