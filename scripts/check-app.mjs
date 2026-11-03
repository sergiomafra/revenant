#!/usr/bin/env node
// Enforces the interface rules on app/src (the app's version of check-mockups.mjs). Run through `npm run check` in
// app/, or: node scripts/check-app.mjs [app folder]   (exit 1 and one line per failure when a rule is broken)
// Scans src/**/*.{ts,tsx,css}, leaving out tests (*.test.*, test-*) and tokens.css:
//   1. no literal color (#hex, rgb( and friends, a CSS color name in a color property, a Tailwind palette class);
//   2. no emoji (also in the catalogs);
//   3. no visible text written in JSX: a text node with letters, a string literal child, or aria-label / title /
//      placeholder / alt with a string literal (every text comes from src/i18n);
//   4. the catalogs src/i18n/{pt-BR,es,en}.json have the same keys, and every t('literal key') exists in pt-BR;
//   5. --brand-gold only in tokens.css and kit/Logo.tsx.
// TSX is parsed with rolldown's parser (oxc), a build tool already in app/node_modules (installed by `npm ci`).
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const defaultApp = path.resolve(here, '..', 'app');
const appRequire = createRequire(path.join(defaultApp, 'package.json'));
const { parseAst } = await import(pathToFileURL(appRequire.resolve('rolldown/parseAst')).href);

const LANGS = ['pt-BR', 'es', 'en'];
const TEXT_ATTRS = new Set(['aria-label', 'title', 'placeholder', 'alt']);
const CSS_COLOR_NAMES = (
  'aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown burlywood ' +
  'cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan darkgoldenrod darkgray ' +
  'darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen ' +
  'darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey dodgerblue ' +
  'firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray green greenyellow grey honeydew ' +
  'hotpink indianred indigo ivory khaki lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan ' +
  'lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue ' +
  'lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon mediumaquamarine ' +
  'mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen mediumturquoise ' +
  'mediumvioletred midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive olivedrab orange ' +
  'orangered orchid palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum ' +
  'powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna ' +
  'silver skyblue slateblue slategray slategrey snow springgreen steelblue tan teal thistle tomato turquoise violet ' +
  'wheat white whitesmoke yellow yellowgreen'
).split(' ');
const hexRe = /(?<![&\w])(?<!href=["'])#[0-9a-fA-F]{3,8}\b/g;
const fnRe = /(?<![\w-])(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch)\(/g;
// A color-bearing property (CSS or a JS style object) and its value, up to the end of the declaration.
const propRe = new RegExp(
  '(?<![\\w-])([a-zA-Z-]*(?:color|Color|background|fill|stroke|border|Border|outline|Outline|shadow|Shadow)' +
    '[a-zA-Z-]*)\\s*:\\s*([^;}\\n,]*)',
  'g',
);
const nameRe = new RegExp(`(?<![\\w-])(${CSS_COLOR_NAMES.join('|')})(?![\\w-])`, 'i');
const twRe = new RegExp(
  '(?<![\\w-])(?:bg|text|border|ring|fill|stroke|outline|divide|accent|caret|decoration|shadow|from|via|to|placeholder)-' +
    '(?:white|black|(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|' +
    'indigo|violet|purple|fuchsia|pink|rose)-\\d{2,3})(?![\\w-])',
  'g',
);
const emojiRe = /\p{Extended_Pictographic}/gu;
const letterRe = /\p{L}/u;

const walk = (dir) =>
  fs.existsSync(dir)
    ? fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
        e.isDirectory() ? (e.name === 'node_modules' ? [] : walk(path.join(dir, e.name))) : [path.join(dir, e.name)])
    : [];
const lineOf = (text, idx) => text.slice(0, idx).split('\n').length;

// "a.b.c" -> value for every leaf of a nested catalog.
function flatten(obj, prefix = '', out = new Map()) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === 'object') flatten(v, key, out);
    else out.set(key, v);
  }
  return out;
}

// Calls fn(node, parent) for every AST node.
function visit(node, fn, parent = null) {
  if (Array.isArray(node)) {
    for (const n of node) visit(n, fn, parent);
    return;
  }
  if (!node || typeof node !== 'object') return;
  if (typeof node.type === 'string') fn(node, parent);
  for (const [k, v] of Object.entries(node)) {
    if (k !== 'type' && v && typeof v === 'object') visit(v, fn, typeof node.type === 'string' ? node : parent);
  }
}

const isText = (n) =>
  (n?.type === 'Literal' && typeof n.value === 'string' && letterRe.test(n.value)) ||
  (n?.type === 'TemplateLiteral' && n.quasis.some((q) => letterRe.test(q.value.cooked ?? q.value.raw)));

export function checkApp(appDir = defaultApp) {
  const failures = [];
  const fail = (rule, msg) => failures.push(`rule ${rule}: ${msg}`);
  const src = path.join(appDir, 'src');

  // Catalogs: same keys in the three languages, no emoji.
  const catalogs = {};
  for (const lang of LANGS) {
    const file = path.join(src, 'i18n', `${lang}.json`);
    if (!fs.existsSync(file)) {
      fail(4, `missing catalog src/i18n/${lang}.json`);
      continue;
    }
    const text = fs.readFileSync(file, 'utf8');
    try {
      catalogs[lang] = flatten(JSON.parse(text));
    } catch (e) {
      fail(4, `src/i18n/${lang}.json is not JSON (${e.message})`);
      continue;
    }
    for (const m of text.matchAll(emojiRe)) fail(2, `src/i18n/${lang}.json:${lineOf(text, m.index)} emoji "${m[0]}"`);
    for (const [k, v] of catalogs[lang]) {
      if (typeof v !== 'string') fail(4, `src/i18n/${lang}.json: "${k}" is not text`);
    }
  }
  const base = catalogs['pt-BR'];
  for (const lang of LANGS.slice(1)) {
    const other = catalogs[lang];
    if (!base || !other) continue;
    for (const k of base.keys()) if (!other.has(k)) fail(4, `src/i18n/${lang}.json misses "${k}"`);
    for (const k of other.keys()) if (!base.has(k)) fail(4, `src/i18n/${lang}.json has "${k}", which pt-BR does not`);
  }

  const files = walk(src)
    .filter((f) => /\.(ts|tsx|css)$/.test(f))
    .filter((f) => !/\.test\.[^/\\]+$/.test(f) && !/^test-/.test(path.basename(f)));
  for (const file of files) {
    const rel = path.relative(appDir, file).split(path.sep).join('/');
    const base_ = path.basename(file);
    const text = fs.readFileSync(file, 'utf8');
    const isTokens = base_ === 'tokens.css';

    if (!isTokens) {
      for (const re of [hexRe, fnRe]) {
        for (const m of text.matchAll(re)) fail(1, `${rel}:${lineOf(text, m.index)} literal color "${m[0]}"`);
      }
      for (const m of text.matchAll(propRe)) {
        const named = m[2].match(nameRe);
        if (named) fail(1, `${rel}:${lineOf(text, m.index)} color name "${named[1]}" in ${m[1]}`);
      }
      if (!rel.endsWith('.css')) {
        for (const m of text.matchAll(twRe)) fail(1, `${rel}:${lineOf(text, m.index)} palette class "${m[0]}"`);
      }
      if (text.includes('--brand-gold') && !(rel.endsWith('/kit/Logo.tsx') || rel === 'src/kit/Logo.tsx')) {
        fail(5, `${rel} uses --brand-gold (only tokens.css and kit/Logo.tsx may)`);
      }
    }
    for (const m of text.matchAll(emojiRe)) fail(2, `${rel}:${lineOf(text, m.index)} emoji "${m[0]}"`);

    if (rel.endsWith('.css')) continue;
    let ast;
    try {
      ast = parseAst(text, { lang: rel.endsWith('.tsx') ? 'tsx' : 'ts' }, file);
    } catch (e) {
      fail(3, `${rel} does not parse (${String(e.message).split('\n')[0]})`);
      continue;
    }
    visit(ast, (node, parent) => {
      const at = `${rel}:${lineOf(text, node.start ?? 0)}`;
      if (node.type === 'JSXText' && letterRe.test(node.value)) {
        fail(3, `${at} text "${node.value.trim()}" written in JSX (use the catalog)`);
      } else if (
        node.type === 'JSXExpressionContainer' &&
        (parent?.type === 'JSXElement' || parent?.type === 'JSXFragment') &&
        isText(node.expression)
      ) {
        fail(3, `${at} text written in JSX (use the catalog)`);
      } else if (node.type === 'JSXAttribute' && TEXT_ATTRS.has(node.name?.name)) {
        const v = node.value?.type === 'JSXExpressionContainer' ? node.value.expression : node.value;
        if (isText(v)) fail(3, `${at} ${node.name.name} with literal text (use the catalog)`);
      } else if (node.type === 'CallExpression' && base) {
        const c = node.callee;
        const isT = (c.type === 'Identifier' && c.name === 't') ||
          (c.type === 'MemberExpression' && !c.computed && c.property?.name === 't');
        const arg = node.arguments?.[0];
        if (isT && arg?.type === 'Literal' && typeof arg.value === 'string' && !base.has(arg.value)) {
          fail(4, `${at} t('${arg.value}') is not in src/i18n/pt-BR.json`);
        }
      }
    });
  }
  return failures;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dir = process.argv[2] ? path.resolve(process.argv[2]) : defaultApp;
  const failures = checkApp(dir);
  for (const f of failures) console.error(f);
  if (!failures.length) console.log(`check-app: ${path.relative(process.cwd(), dir) || '.'} is clean`);
  process.exit(failures.length ? 1 : 0);
}
