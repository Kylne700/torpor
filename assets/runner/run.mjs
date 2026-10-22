// Torpor browser runner (spec 16). Torpor copies this file next to node_modules/playwright-core and runs it with node.
// stdin: one JSON object {nonce, port, chromium, steps}. stdout: protocol lines, one JSON object per line (same lines as the http runner).
// It opens only pages of the app on 127.0.0.1: every other request is aborted, so a check never reaches the internet.
import fs from 'node:fs';

const ACTION_MS = 5000; // spec 15: action and expect timeouts
const CHECK_MS = 30000; // the whole step list
const WAIT_HTTP_MS = 10000;
const LAUNCH_MS = 30000;
const MAX_HOPS = 5;
const BODY_CAP = 1 << 20;
const POLL_MS = 100;

function emit(o) {
  const b = Buffer.from(JSON.stringify(o) + '\n');
  for (let off = 0; off < b.length; ) {
    try {
      off += fs.writeSync(1, b, off);
    } catch (e) {
      if (e.code !== 'EAGAIN') throw e;
    }
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const short = (s, n = 200) => (String(s).length > n ? String(s).slice(0, n) + '...' : String(s));

/** A step that did not hold: a symptom of the app (a FAIL). */
class Fail extends Error {}
/** The runner cannot judge this run (UNRUNNABLE): never a verdict on the app. */
class Fatal extends Error {}

const LOOPBACK_V4 = /^127(\.\d{1,3}){3}$/;
const isLoopback = (h) => h === 'localhost' || h === '[::1]' || h === '::1' || LOOPBACK_V4.test(h);

function describe(l) {
  switch (l.k) {
    case 'role': return l.name != null ? `${l.v} "${l.name}"` : l.v;
    case 'label': return `label "${l.v}"`;
    case 'text': return `text "${l.v}"`;
    case 'text_re': return `text matching /${l.v}/`;
    default: return `test id "${l.v}"`;
  }
}

function locate(page, l) {
  switch (l.k) {
    case 'role': return l.name != null ? page.getByRole(l.v, { name: l.name }) : page.getByRole(l.v);
    case 'label': return page.getByLabel(l.v);
    case 'text': return page.getByText(l.v);
    case 'text_re': return page.getByText(new RegExp(l.v));
    default: return page.getByTestId(l.v);
  }
}

// ---- http steps: the same rules as the Rust runner (src/runner.rs), but with the browser's cookies

function jsonAt(v, path) {
  let cur = v;
  for (const seg of path.replace(/\[/g, '.').replace(/\]/g, '').split('.').filter(Boolean)) {
    if (cur === null || typeof cur !== 'object') return undefined;
    if (Array.isArray(cur)) {
      if (!/^\d+$/.test(seg) || Number(seg) >= cur.length) return undefined;
      cur = cur[Number(seg)];
    } else {
      if (!Object.prototype.hasOwnProperty.call(cur, seg)) return undefined;
      cur = cur[seg];
    }
  }
  return cur;
}

const typeOf = (x) => (x === null ? 'null' : Array.isArray(x) ? 'array' : typeof x);

function checkExpect(e, label, status, body) {
  if (e.status != null && status !== e.status) return `${label} answered ${status}, expected ${e.status}`;
  if (e.status_in && e.status_in.length && !e.status_in.includes(status)) return `${label} answered ${status}, expected one of [${e.status_in.join(', ')}]`;
  if (e.length_range) {
    const [lo, hi] = e.length_range;
    if (body.length < lo || body.length > hi) return `${label} answered ${body.length} bytes, expected ${lo} to ${hi}`;
  }
  const text = body.toString('utf8');
  if (e.body_contains != null && !text.includes(e.body_contains)) return `${label} answer does not contain the expected text`;
  const has = e.json_has || [];
  const types = e.json_type || [];
  if (has.length || types.length) {
    let v;
    try {
      v = JSON.parse(text);
    } catch {
      return `${label} answer is not JSON`;
    }
    for (const p of has) if (jsonAt(v, p) === undefined) return `${label} answer has no field ${p}`;
    for (const [p, t] of types) {
      const x = jsonAt(v, p);
      if (x === undefined) return `${label} answer has no field ${p}`;
      if (typeOf(x) !== t) return `${label} field ${p} is ${typeOf(x)}, expected ${t}`;
    }
  }
  return null;
}

// ---- the run

async function main() {
  const input = JSON.parse(fs.readFileSync(0, 'utf8'));
  const { nonce, port, steps } = input;
  const base = `http://127.0.0.1:${port}`;
  let started = false;
  const err = (msg) => emit({ p: 'ERROR', where: started ? 'check' : 'setup', msg: short(msg) });

  let chromium;
  try {
    ({ chromium } = await import('playwright-core'));
  } catch (e) {
    return err(`the browser runner could not be loaded (${short(e && e.message)}): torpor runner install`);
  }
  let exe = input.chromium;
  if (!exe) {
    try {
      exe = chromium.executablePath();
    } catch {
      exe = null;
    }
    if (!exe || !fs.existsSync(exe)) return err('no Chromium found: torpor runner install');
  }

  const blocked = new Set();
  let wentOut = null; // set when a page itself tried to leave 127.0.0.1: the check cannot be judged
  const errors = [];
  let browser;
  let page;
  let context;
  try {
    browser = await chromium.launch({
      executablePath: exe,
      headless: true,
      timeout: LAUNCH_MS,
      // Belt and braces next to the route below: no name resolves except localhost, no proxy, no WebRTC through the network.
      args: ['--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1 , EXCLUDE localhost', '--no-proxy-server', '--force-webrtc-ip-handling-policy=disable_non_proxied_udp'],
    });
    context = await browser.newContext({ serviceWorkers: 'block', acceptDownloads: false });
    await context.route(/.*/, (route) => {
      const req = route.request();
      let u = null;
      try {
        u = new URL(req.url());
      } catch {}
      if (u && (u.protocol === 'http:' || u.protocol === 'https:') && isLoopback(u.hostname)) return route.continue();
      blocked.add(req.url());
      if (req.isNavigationRequest() && req.frame() === page.mainFrame()) wentOut = `non-loopback: the page went to ${u ? u.host : 'another address'}`;
      return route.abort();
    });
    await context.routeWebSocket(/.*/, (ws) => {
      let u = null;
      try {
        u = new URL(ws.url());
      } catch {}
      if (u && isLoopback(u.hostname)) ws.connectToServer();
      else ws.close();
    });
    page = await context.newPage();
    page.on('console', (m) => {
      if (m.type() === 'error' && !blocked.has(m.location().url)) errors.push(m.text());
    });
    page.on('pageerror', (e) => errors.push(String((e && e.message) || e)));
  } catch (e) {
    await Promise.race([browser ? browser.close().catch(() => {}) : null, sleep(3000)]);
    return err(`the browser did not start (${short(e && e.message)})`);
  }
  emit({ p: 'SETUP_OK' });
  emit({ p: 'CHECK_START' });
  started = true;

  const closed = /has been closed|Protocol error|Browser closed|Target closed|Connection closed/;
  const lost = (e) => closed.test(String((e && e.message) || e));

  /** Poll `fn` (returns null when it holds, else the reason it does not) until it holds or the expect time is up. */
  async function until(fn, ms = ACTION_MS) {
    const end = Date.now() + ms;
    let why = 'did not hold';
    for (;;) {
      try {
        const r = await fn();
        if (r === null) return;
        why = r;
      } catch (e) {
        if (lost(e) || wentOut) throw new Fatal(wentOut || 'the browser stopped');
        if (e instanceof SyntaxError) throw new Fatal(`a pattern in the check is not valid (${short(e.message)})`);
      }
      if (Date.now() >= end) throw new Fail(why);
      await sleep(POLL_MS);
    }
  }

  async function act(verb, l, fn) {
    const L = locate(page, l);
    try {
      await fn(L);
    } catch (e) {
      if (wentOut) throw new Fatal(wentOut);
      const m = String((e && e.message) || e);
      if (lost(e)) throw new Fatal('the browser stopped');
      if (e instanceof SyntaxError) throw new Fatal(`a pattern in the check is not valid (${short(e.message)})`);
      const n = /resolved to (\d+) elements/.exec(m);
      if (/strict mode violation/.test(m)) throw new Fail(`${verb}: ${describe(l)} matches ${n ? n[1] : 'several'} elements`);
      if (e && e.name === 'TimeoutError') {
        let found = 0;
        try {
          found = await L.count();
        } catch {}
        throw new Fail(`${verb}: ${describe(l)} ${found === 0 ? 'was not found' : 'was not ready'} within ${ACTION_MS / 1000} s`);
      }
      throw new Fatal(`${verb} failed (${short(m, 120)})`);
    }
  }

  async function fetchOnce(method, path, data) {
    let url = base + path;
    let meth = method;
    let body = data;
    for (let hop = 0; hop <= MAX_HOPS; hop++) {
      let r;
      try {
        r = await context.request.fetch(url, { method: meth, data: body, maxRedirects: 0, failOnStatusCode: false, timeout: ACTION_MS, headers: { accept: '*/*' } });
      } catch (e) {
        if (lost(e)) throw new Fatal('the browser stopped');
        const why = e && e.name === 'TimeoutError' ? `did not answer within ${ACTION_MS / 1000} s` : `could not be reached (${short((e && e.message) || e, 80).split('\n')[0]})`;
        throw new Fail(`${meth} ${path} ${why}`);
      }
      const status = r.status();
      const loc = r.headers()['location'];
      if ([301, 302, 303, 307, 308].includes(status) && loc) {
        const next = new URL(loc, url);
        if (next.protocol !== 'http:' || !isLoopback(next.hostname)) throw new Fatal(`non-loopback: the app redirected to ${next.host || next.protocol}`);
        if (next.port !== String(port)) throw new Fatal(`the app redirected to another port (${next.port}); a check only talks to its own app`);
        url = next.href;
        path = next.pathname + next.search;
        if (status !== 307 && status !== 308) {
          meth = 'GET';
          body = undefined;
        }
        continue;
      }
      const buf = await r.body();
      if (buf.length > BODY_CAP) throw new Fatal('the answer is too large to check');
      return { status, buf, path, meth };
    }
    throw new Fail(`${path} redirected more than ${MAX_HOPS} times`);
  }

  async function runStep(st) {
    switch (st.op) {
      case 'goto': {
        let res = null;
        try {
          res = await page.goto(base + st.path, { timeout: ACTION_MS, waitUntil: 'load' });
        } catch (e) {
          if (wentOut) throw new Fatal(wentOut);
          const m = String((e && e.message) || e);
          if (lost(e)) throw new Fatal('the browser stopped');
          const net = /net::ERR_[A-Z_]+/.exec(m);
          if (net) throw new Fail(`goto ${st.path} failed (${net[0]})`);
          if (e && e.name === 'TimeoutError') throw new Fail(`goto ${st.path} did not finish loading within ${ACTION_MS / 1000} s`);
          throw new Fatal(`goto failed (${short(m, 120)})`);
        }
        if (wentOut) throw new Fatal(wentOut);
        if (res && res.status() >= 500) throw new Fail(`goto ${st.path} answered ${res.status()}`);
        return false;
      }
      case 'click': await act('click', st.loc, (L) => L.click({ timeout: ACTION_MS })); return false;
      case 'fill': await act('fill', st.loc, (L) => L.fill(st.value, { timeout: ACTION_MS })); return false;
      case 'press': await act('press', st.loc, (L) => L.press(st.key, { timeout: ACTION_MS })); return false;
      case 'select': await act('select', st.loc, (L) => L.selectOption(st.value, { timeout: ACTION_MS })); return false;
      case 'check': await act('check', st.loc, (L) => L.check({ timeout: ACTION_MS })); return false;
      case 'uncheck': await act('uncheck', st.loc, (L) => L.uncheck({ timeout: ACTION_MS })); return false;
      case 'wait_http': {
        const end = Date.now() + WAIT_HTTP_MS;
        for (;;) {
          try {
            const r = await fetchOnce('GET', st.path);
            if (r.status >= 200 && r.status < 300) return false;
          } catch (e) {
            if (e instanceof Fatal) throw e;
          }
          if (Date.now() >= end) throw new Fail(`GET ${st.path} did not answer 2xx within ${WAIT_HTTP_MS / 1000} s`);
          await sleep(250);
        }
      }
      case 'expect_visible': {
        const L = locate(page, st.loc);
        await until(async () => {
          if ((await L.filter({ visible: true }).count()) > 0) return null;
          return `${describe(st.loc)} ${(await L.count()) === 0 ? 'was not found' : 'is not visible'}`;
        });
        return true;
      }
      case 'expect_hidden': {
        const L = locate(page, st.loc);
        await until(async () => ((await L.filter({ visible: true }).count()) === 0 ? null : `${describe(st.loc)} is still visible`));
        return true;
      }
      case 'expect_text': {
        const L = locate(page, st.loc);
        const want = st.contains.replace(/\s+/g, ' ').trim();
        await until(async () => {
          const texts = (await L.allInnerTexts()).map((t) => t.replace(/\s+/g, ' ').trim());
          if (texts.some((t) => t.includes(want))) return null;
          return `${describe(st.loc)} ${texts.length === 0 ? 'was not found' : `does not contain "${short(want, 80)}"`}`;
        });
        return true;
      }
      case 'expect_url': {
        const re = st.path_re != null ? new RegExp(st.path_re) : null;
        await until(async () => {
          const at = new URL(page.url()).pathname;
          if (re ? re.test(at) : at === st.path) return null;
          return `the page is at ${at}, expected ${re ? `a path matching /${st.path_re}/` : st.path}`;
        });
        return true;
      }
      case 'expect_count': {
        const L = locate(page, st.loc);
        const ok = (n) => (st.n != null ? n === st.n : (st.min == null || n >= st.min) && (st.max == null || n <= st.max));
        const want = st.n != null ? String(st.n) : `${st.min ?? 0} to ${st.max ?? 'any'}`;
        await until(async () => {
          const n = await L.count();
          return ok(n) ? null : `found ${n} of ${describe(st.loc)}, expected ${want}`;
        });
        return true;
      }
      case 'expect_console_clean': {
        if (errors.length) throw new Fail(`the page logged a console error: ${short(errors[0])}${errors.length > 1 ? ` (and ${errors.length - 1} more)` : ''}`);
        return true;
      }
      case 'http': {
        const r = await fetchOnce(st.method, st.path, st.json);
        const msg = checkExpect(st.expect || {}, `${r.meth} ${r.path}`, r.status, r.buf);
        if (msg) throw new Fail(msg);
        return true;
      }
      default:
        throw new Fatal(`the runner does not know the step ${st.op}`);
    }
  }

  const t0 = Date.now();
  let passes = 0;
  for (const st of steps) {
    if (Date.now() - t0 > CHECK_MS) {
      emit({ p: 'ASSERT_FAIL', i: st.i, msg: `the check took more than ${CHECK_MS / 1000} s` });
      break;
    }
    try {
      if (await runStep(st)) {
        passes += 1;
        emit({ p: 'ASSERT_PASS', i: st.i });
      }
      if (wentOut) throw new Fatal(wentOut);
    } catch (e) {
      if (e instanceof Fail) {
        emit({ p: 'ASSERT_FAIL', i: st.i, msg: short(e.message) });
        break;
      }
      err(e instanceof Fatal ? e.message : `the runner failed (${short((e && e.message) || e, 120)})`);
      return finish(browser);
    }
  }
  emit({ p: 'done', n: passes, token: nonce });
  return finish(browser);
}

async function finish(browser) {
  await Promise.race([browser ? browser.close().catch(() => {}) : null, sleep(3000)]);
}

try {
  await main();
} catch (e) {
  try {
    emit({ p: 'ERROR', where: 'check', msg: short(`the browser runner failed (${(e && e.message) || e})`) });
  } catch {}
}
process.exit(0);
