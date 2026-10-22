// Torpor runner installer (spec 24). Run by `torpor runner install` after you confirmed:
//   node install.mjs <dest> <version> <integrity> <registry>
// It downloads playwright-core <version> from the registry, checks the package against the published integrity hash BEFORE it unpacks
// anything, and moves it into <dest>/node_modules. It never runs npm and never runs an install script of the package.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const [dest, version, integrity, registry] = process.argv.slice(2);
const MAX_BYTES = 64 * 1024 * 1024;
const NAME = 'playwright-core';

async function main() {
  fs.mkdirSync(dest, { recursive: true, mode: 0o700 });
  // No redirects: the one host the question named is the only host asked, whatever it answers.
  const res = await fetch(`${registry}/${NAME}/-/${NAME}-${version}.tgz`, { redirect: 'error', signal: AbortSignal.timeout(180000) }).catch((e) => {
    if (e && e.cause && e.cause.message === 'unexpected redirect') throw new Error('the registry sent a redirect, so nothing was installed');
    throw e;
  });
  if (!res.ok) throw new Error(`the registry answered ${res.status}`);
  const chunks = [];
  let size = 0;
  for await (const c of res.body) {
    size += c.length;
    if (size > MAX_BYTES) throw new Error('the download is larger than expected, so nothing was installed');
    chunks.push(c);
  }
  const tgz = Buffer.concat(chunks);
  if (`sha512-${crypto.createHash('sha512').update(tgz).digest('base64')}` !== integrity) {
    throw new Error('the download does not match the published hash, so nothing was installed');
  }
  const stage = fs.mkdtempSync(path.join(dest, '.staging-'));
  try {
    const file = path.join(stage, 'package.tgz');
    const out = path.join(stage, 'package');
    fs.writeFileSync(file, tgz);
    fs.mkdirSync(out);
    const t = spawnSync('tar', ['-xzf', file, '-C', out, '--strip-components=1'], { stdio: ['ignore', 'ignore', 'pipe'], encoding: 'utf8' });
    if (t.error || t.status !== 0) throw new Error(`the package could not be unpacked (${t.error ? t.error.message : String(t.stderr).trim().slice(0, 200)})`);
    const pkg = JSON.parse(fs.readFileSync(path.join(out, 'package.json'), 'utf8'));
    if (pkg.name !== NAME || pkg.version !== version) throw new Error('the package is not the one that was asked for, so nothing was installed');
    const modules = path.join(dest, 'node_modules');
    fs.mkdirSync(modules, { recursive: true });
    fs.rmSync(path.join(modules, NAME), { recursive: true, force: true });
    fs.renameSync(out, path.join(modules, NAME)); // the last step: a folder with a package.json means "installed"
    console.log(`installed ${NAME} ${version} (${(size / 1048576).toFixed(1)} MB)`);
  } finally {
    fs.rmSync(stage, { recursive: true, force: true });
  }
}

try {
  await main();
} catch (e) {
  console.error(String((e && e.message) || e));
  process.exit(1);
}
