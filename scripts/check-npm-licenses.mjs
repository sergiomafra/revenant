#!/usr/bin/env node
// Checks the licenses in an npm package-lock.json (v3). Node only, no dependencies.
// Usage: node scripts/check-npm-licenses.mjs <folder with package-lock.json>   (exit 1 and one line per failure)
// Packages that ship inside the app must use an allowed license; build-only packages (dev) may use any open
// license except GPL/AGPL (LGPL is out too: it contains "GPL").
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const RUNTIME_ALLOWED = new Set([
  'MIT', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib', 'Unicode-3.0', 'Unlicense', 'CC0-1.0',
  'BSL-1.0', 'MPL-2.0',
]);

const runtimeOk = (id) => RUNTIME_ALLOWED.has(id);
const devOk = (id) => !/GPL/i.test(id);

// Parses an SPDX expression (OR, AND, parentheses) and evaluates it with `idOk`.
function evalExpr(expr, idOk) {
  const tokens = expr.match(/\(|\)|[^\s()]+/g) ?? [];
  let pos = 0;
  const parseOr = () => {
    let v = parseAnd();
    while (tokens[pos] === 'OR') { pos++; const r = parseAnd(); v = v || r; }
    return v;
  };
  const parseAnd = () => {
    let v = parseAtom();
    while (tokens[pos] === 'AND') { pos++; const r = parseAtom(); v = v && r; }
    return v;
  };
  const parseAtom = () => {
    const t = tokens[pos++];
    if (t === undefined) throw new Error('unexpected end');
    if (t === '(') {
      const v = parseOr();
      if (tokens[pos++] !== ')') throw new Error('missing )');
      return v;
    }
    return idOk(t.replace(/\+$/, ''));
  };
  try {
    const v = parseOr();
    return pos === tokens.length ? v : false;
  } catch {
    return false;
  }
}

export function checkLock(lock) {
  const failures = [];
  for (const [key, pkg] of Object.entries(lock.packages ?? {})) {
    if (key === '' || pkg.link) continue;
    const dev = pkg.dev === true || pkg.devOptional === true;
    const license = typeof pkg.license === 'string' ? pkg.license.trim() : '';
    const name = key.replace(/^.*node_modules\//, '');
    if (!license) {
      failures.push(`${name}: no license declared`);
    } else if (!evalExpr(license, dev ? devOk : runtimeOk)) {
      failures.push(`${name}: license "${license}" not allowed${dev ? ' (build tool)' : ' (ships in the app)'}`);
    }
  }
  return failures;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dir = process.argv[2];
  if (!dir) {
    console.error('usage: node scripts/check-npm-licenses.mjs <folder with package-lock.json>');
    process.exit(1);
  }
  const file = path.join(dir, 'package-lock.json');
  if (!fs.existsSync(file)) {
    console.error(`missing ${file}`);
    process.exit(1);
  }
  const failures = checkLock(JSON.parse(fs.readFileSync(file, 'utf8')));
  for (const f of failures) console.error(f);
  process.exit(failures.length ? 1 : 0);
}
