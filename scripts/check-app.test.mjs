// Tests for check-app.mjs. Run: node --test scripts/check-app.test.mjs
// (needs `npm ci` in app/: the checker parses with rolldown).
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { checkApp } from './check-app.mjs';

const CATALOG = { app: { name: 'Revenant' }, settings: { title: 'Ajustes' } };

// A throwaway app folder: src/i18n with three catalogs plus the given files (path -> text).
function appWith(files, catalogs = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'check-app-'));
  const all = {
    'src/i18n/pt-BR.json': JSON.stringify(catalogs['pt-BR'] ?? CATALOG),
    'src/i18n/es.json': JSON.stringify(catalogs.es ?? CATALOG),
    'src/i18n/en.json': JSON.stringify(catalogs.en ?? CATALOG),
    ...files,
  };
  for (const [f, text] of Object.entries(all)) {
    fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
    fs.writeFileSync(path.join(dir, f), text);
  }
  return dir;
}
// Fails, and for the expected rule (not, say, because the example does not parse).
const fails = (rule, files, catalogs) => {
  const got = checkApp(appWith(files, catalogs));
  assert.ok(got.length > 0 && got.every((f) => f.startsWith(`rule ${rule}:`)), got.join('\n') || 'no failure');
};
const passes = (files, catalogs) => assert.deepEqual(checkApp(appWith(files, catalogs)), []);

const CLEAN = `import { useTranslation } from 'react-i18next';
export function Title({ code }: { code: string }) {
  const { t } = useTranslation();
  const list: Array<string> = [code];
  return (
    <h3 className="ttl" aria-label={t('settings.title')}>
      {t('settings.title')} {list.length > 0 ? code : null}
    </h3>
  );
}
`;

test('a clean file passes', () =>
  passes({ 'src/Title.tsx': CLEAN, 'src/a.css': '.x{color:var(--ink);border:1px solid transparent}' }));
test('color: #fff fails', () => fails(1, { 'src/a.css': '.x{color:#fff}' }));
test('rgb( in a style object fails', () => fails(1, { 'src/a.tsx': 'export const s = { color: "rgb(1,2,3)" };' }));
test('a CSS color name in a color property fails', () => fails(1, { 'src/a.css': '.x{background:white}' }));
test('a literal color in tokens.css is allowed', () => passes({ 'src/styles/tokens.css': ':root{--ink:#000}' }));
test('a literal color in a test file is allowed', () => passes({ 'src/a.test.tsx': 'const c = "#fff";' }));
test('an emoji fails', () => fails(2, { 'src/a.ts': 'export const s = "ok \u{1F600}";' }));
test('an emoji in a catalog fails', () =>
  fails(2, {}, { 'pt-BR': { app: { name: 'R \u{1F600}' }, settings: { title: 'A' } } }));
test('<p>Olá</p> fails', () => fails(3, { 'src/a.tsx': 'export const A = () => <p>Olá</p>;' }));
test('a string literal child fails', () => fails(3, { 'src/a.tsx': "export const A = () => <p>{'Olá'}</p>;" }));
test('aria-label="Fechar" fails', () => fails(3, { 'src/a.tsx': 'export const A = () => <button aria-label="Fechar" />;' }));
test('title={"Fechar"} fails', () => fails(3, { 'src/a.tsx': 'export const A = () => <button title={"Fechar"} />;' }));
test('placeholder and alt literals fail', () => {
  fails(3, { 'src/a.tsx': 'export const A = () => <input placeholder="Nome" />;' });
  fails(3, { 'src/a.tsx': 'export const A = () => <img alt="Foto" />;' });
});
test('generics and arrows are not JSX text', () =>
  passes({ 'src/a.ts': 'export const f = (x: Promise<string>) => x;\nconst y = 1 < 2 && 3 > 1;' }));
test('a catalog key missing in es fails', () =>
  fails(4, {}, { es: { app: { name: 'Revenant' }, settings: {} } }));
test('an extra key in en fails', () =>
  fails(4, {}, { en: { app: { name: 'Revenant', extra: 'x' }, settings: { title: 'Settings' } } }));
test("t('…') with a key missing in pt-BR fails", () =>
  fails(4, { 'src/a.tsx': "export const A = ({ t }: { t: (k: string) => string }) => <p>{t('settings.nope')}</p>;" }));
test('--brand-gold outside tokens.css and Logo.tsx fails', () => fails(5, { 'src/a.css': '.x{stroke:var(--brand-gold)}' }));
test('--brand-gold in Logo.tsx is allowed', () =>
  passes({ 'src/kit/Logo.tsx': "export const s = { '--d': 'var(--brand-gold)' };" }));
