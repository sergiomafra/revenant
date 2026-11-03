#!/usr/bin/env node
// Enforces the Part 2.0 mockup rules on docs/superpowers/mockups/m2/. Node only, no dependencies.
// Usage: node scripts/check-mockups.mjs   (exit 1 and one line per failure when a rule is broken)
// MOCKUPS_DIR=<dir> checks another folder (used to test the checker itself).
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = process.env.MOCKUPS_DIR
  ? path.resolve(process.env.MOCKUPS_DIR)
  : path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'docs/superpowers/mockups/m2');
const failures = [];
const fail = (rule, msg) => failures.push(`rule ${rule}: ${msg}`);
const read = (f) => fs.readFileSync(path.join(root, f), 'utf8');
const exists = (f) => fs.existsSync(path.join(root, f));
const walk = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]);

if (!fs.existsSync(root)) {
  console.error(`missing folder ${root}`);
  process.exit(1);
}
const all = walk(root).map((f) => path.relative(root, f)).filter((f) => !f.startsWith('.'));
const htmlFiles = all.filter((f) => f.endsWith('.html'));
const pages = htmlFiles.filter((f) => f !== 'index.html');
const lineOf = (text, idx) => text.slice(0, idx).split('\n').length;

// 1. Every page links tokens.css, kit.css, kit.js and calls Kit.init(.
for (const need of ['tokens.css', 'kit.css', 'kit.js', 'index.html']) {
  if (!exists(need)) fail(1, `${need} is missing`);
}
for (const p of pages) {
  const t = read(p);
  if (!/<link[^>]+href="tokens\.css"/.test(t)) fail(1, `${p} does not link tokens.css`);
  if (!/<link[^>]+href="kit\.css"/.test(t)) fail(1, `${p} does not link kit.css`);
  if (!/<script[^>]+src="kit\.js"/.test(t)) fail(1, `${p} does not load kit.js`);
  if (!/Kit\.init\(/.test(t)) fail(1, `${p} does not call Kit.init(`);
}

// 2. No literal color outside tokens.css (embedded brand SVGs between brand:start/end are exempt).
// Exempt numeric entities (&#8212;) and in-page anchors (href="#add"); real colors in CSS/attributes still match.
const colorRe = /(?<!&)(?<!href=["'])#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(/g;
for (const f of all.filter((f) => /\.(html|css|js)$/.test(f) && f !== 'tokens.css')) {
  let t = read(f);
  if (f === 'kit.js') t = t.replace(/\/\* brand:start \*\/[\s\S]*?\/\* brand:end \*\//g, (m) => m.replace(/[^\n]/g, ' '));
  for (const m of t.matchAll(colorRe)) fail(2, `${f}:${lineOf(t, m.index)} literal color "${m[0]}"`);
}

// 3. No emoji anywhere.
for (const f of all.filter((f) => /\.(html|css|js|mjs|svg|md)$/.test(f))) {
  const t = read(f);
  for (const m of t.matchAll(/\p{Extended_Pictographic}/gu)) fail(3, `${f}:${lineOf(t, m.index)} emoji "${m[0]}"`);
}

// 4. No jargon or engine codes in visible HTML (technical details are exempt).
// Drops Kit.tech( ... ) calls by matching parentheses while skipping string contents.
function stripKitTech(src) {
  let out = '';
  let i = 0;
  const key = 'Kit.tech(';
  while (i < src.length) {
    if (src.startsWith(key, i)) {
      let depth = 1;
      let j = i + key.length;
      let quote = null;
      while (j < src.length && depth > 0) {
        const c = src[j];
        if (quote) {
          if (c === '\\') j++;
          else if (c === quote) quote = null;
        } else if (c === '"' || c === "'" || c === '`') quote = c;
        else if (c === '(') depth++;
        else if (c === ')') depth--;
        j++;
      }
      i = j;
    } else out += src[i++];
  }
  return out;
}
const engineCodes = ('device_gone permission_denied device_not_found system_disk open_failed device_not_responding ' +
  'sample_unreadable unmount_failed destination_too_small destination_on_source destination_unknown ' +
  'destination_file_too_large session_other_source session_corrupt write_failed read_failed').split(' ');
// Letter-aware boundaries (plain \b does not see "ã" as a letter).
const jargonRe = /(?<![\p{L}\p{N}_])(setor|setores|cluster|clusters|carving|escultura|sessão|sessões)(?![\p{L}\p{N}_])/giu;
for (const f of htmlFiles) {
  // index.html is the builders' map: it shows the engine codes plainly on purpose (the only file exempt from rule 4).
  if (f.endsWith('index.html')) continue;
  let t = read(f);
  t = t.replace(/<details\s+class="tech"[^>]*>[\s\S]*?<\/details>/g, (m) => m.replace(/[^\n]/g, ' '));
  t = stripKitTech(t);
  t = t.replace(/data-code="[^"]*"|data-code='[^']*'/g, (m) => m.replace(/[^\n]/g, ' '));
  for (const m of t.matchAll(jargonRe)) fail(4, `${f}:${lineOf(t, m.index)} jargon "${m[0]}"`);
  for (const code of engineCodes) {
    const re = new RegExp(`(?<![A-Za-z0-9_])${code}(?![A-Za-z0-9_])`, 'g');
    for (const m of t.matchAll(re)) fail(4, `${f}:${lineOf(t, m.index)} engine code "${code}" outside technical details`);
  }
}

// 5. Screens 1..13: each exactly once as "id: N, name:" in a page, and each in the index table with a link.
const seen = new Map();
for (const p of pages) {
  const t = read(p);
  for (const m of t.matchAll(/\bid:\s*(\d+)\s*,\s*name\s*:/g)) {
    const n = Number(m[1]);
    if (!seen.has(n)) seen.set(n, []);
    seen.get(n).push(p);
  }
}
const missing = [];
for (let n = 1; n <= 13; n++) {
  const where = seen.get(n) || [];
  if (where.length === 0) missing.push(n);
  else if (where.length > 1) fail(5, `screen ${n} appears ${where.length} times (${where.join(', ')})`);
}
if (missing.length) fail(5, `missing screens (expected "id: N, name: ..." inside Kit.init): ${missing.join(', ')}`);
for (const n of seen.keys()) if (n < 1 || n > 13) fail(5, `unknown screen id ${n}`);
if (exists('index.html')) {
  const idx = read('index.html');
  for (let n = 1; n <= 13; n++) {
    const row = idx.match(new RegExp(`<tr[^>]*data-map="${n}"[^>]*>[\\s\\S]*?</tr>`));
    if (!row) { fail(5, `index.html has no row with data-map="${n}"`); continue; }
    const href = row[0].match(/<a[^>]+href="([^"#?]+\.html)(?:[#?][^"]*)?"/);
    if (!href) { fail(5, `index.html row ${n} has no link to a page`); continue; }
    if (exists(href[1]) && !(seen.get(n) || []).includes(href[1])) {
      fail(5, `index.html row ${n} links to ${href[1]}, which does not define screen ${n}`);
    }
  }
  const dup = [...idx.matchAll(/data-map="(\d+)"/g)].map((m) => m[1]);
  if (new Set(dup).size !== dup.length) fail(5, 'index.html has a duplicate data-map');
}

// 6. --brand-gold only in tokens.css, kit.js and app.html.
for (const f of all.filter((f) => /\.(html|css|js)$/.test(f))) {
  if (['tokens.css', 'kit.js', 'app.html'].includes(f)) continue;
  if (read(f).includes('--brand-gold')) fail(6, `${f} uses --brand-gold`);
}

// 7. No data-source-disk inside a data-destination-option.
for (const f of htmlFiles) {
  const t = read(f);
  const tags = [...t.matchAll(/<([a-zA-Z][\w-]*)\b([^<>]*?)>/g)];
  for (const m of tags) {
    if (!/\bdata-destination-option\b/.test(m[2])) continue;
    if (/\bdata-source-disk\b/.test(m[2])) fail(7, `${f}:${lineOf(t, m.index)} same element has both attributes`);
    // Walk forward, counting open/close tags of the same name, to find the element's block.
    const name = m[1].toLowerCase();
    const selfClosing = /\/\s*$/.test(m[2]) || ['input', 'img', 'br', 'hr'].includes(name);
    if (selfClosing) continue;
    let depth = 1;
    const re = new RegExp(`<(/?)${name}\\b[^<>]*?(/?)>`, 'gi');
    re.lastIndex = m.index + m[0].length;
    let end = t.length;
    for (let x; (x = re.exec(t)); ) {
      if (x[2]) continue;
      depth += x[1] ? -1 : 1;
      if (depth === 0) { end = x.index; break; }
    }
    const block = t.slice(m.index + m[0].length, end);
    const inner = block.search(/\bdata-source-disk\b/);
    if (inner >= 0) fail(7, `${f}:${lineOf(t, m.index + m[0].length + inner)} data-source-disk inside data-destination-option`);
  }
}

// 8. tokens.css defines every token in :root, :root[data-theme="dark"] and the prefers-color-scheme block.
function blockAfter(text, startRe) {
  const m = startRe.exec(text);
  if (!m) return null;
  const open = text.indexOf('{', m.index + m[0].length - 1);
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '{') depth++;
    else if (text[i] === '}' && --depth === 0) return text.slice(open + 1, i);
  }
  return null;
}
if (exists('tokens.css')) {
  const t = read('tokens.css');
  const decl = (b) => new Set([...(b || '').matchAll(/(--[\w-]+)\s*:/g)].map((m) => m[1]));
  const light = blockAfter(t, /(^|\n):root\s*\{/);
  const dark = blockAfter(t, /:root\[data-theme="dark"\]\s*\{/);
  const media = blockAfter(t, /@media\s*\(prefers-color-scheme:\s*dark\)\s*\{/);
  const auto = media && blockAfter(media, /:root:not\(\[data-theme="light"\]\)\s*\{/);
  const sets = { ':root': decl(light), ':root[data-theme="dark"]': decl(dark), 'prefers-color-scheme block': decl(auto) };
  for (const [label, b] of [[':root', light], [':root[data-theme="dark"]', dark], ['prefers-color-scheme block', auto]]) {
    if (b === null) fail(8, `tokens.css has no ${label} block`);
  }
  const required = ('paper surface surface-2 line line-soft ink ink-2 ink-3 accent accent-soft accent-ink ok ok-bg warn ' +
    'warn-bg bad bad-bg img-bg shadow brand-gold tile tile-ink tile-gold overlay photo-tag on-photo info info-bg ' +
    'win-close win-min win-max font').split(' ').map((n) => `--${n}`);
  for (const tok of required) {
    for (const [label, s] of Object.entries(sets)) if (!s.has(tok)) fail(8, `tokens.css: required ${tok} is not defined in ${label}`);
  }
  const values = (b) => new Map([...(b || '').matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
  const dv = values(dark), av = values(auto);
  for (const [tok, v] of dv) {
    if (av.has(tok) && av.get(tok) !== v) fail(8, `tokens.css: ${tok} differs between data-theme="dark" (${v}) and prefers-color-scheme (${av.get(tok)})`);
  }
  const union = new Set(Object.values(sets).flatMap((s) => [...s]));
  for (const tok of [...union].sort()) {
    for (const [label, s] of Object.entries(sets)) if (!s.has(tok)) fail(8, `tokens.css: ${tok} is not defined in ${label}`);
  }
}

if (failures.length) {
  console.error(failures.join('\n'));
  console.error(`\n${failures.length} failure(s)`);
  process.exit(1);
}
console.log('check-mockups: all rules pass');
