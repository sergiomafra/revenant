// Tests for check-npm-licenses.mjs. Run: node --test scripts/check-npm-licenses.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { checkLock } from './check-npm-licenses.mjs';

const lock = (pkg) => ({ lockfileVersion: 3, packages: { '': { name: 'app' }, 'node_modules/x': pkg } });
const ok = (pkg) => assert.deepEqual(checkLock(lock(pkg)), []);
const bad = (pkg) => assert.equal(checkLock(lock(pkg)).length, 1);

test('MIT dependency passes', () => ok({ license: 'MIT' }));
test('MPL-2.0 dependency passes', () => ok({ license: 'MPL-2.0' }));
test('GPL-3.0 dependency fails', () => bad({ license: 'GPL-3.0' }));
test('LGPL-2.1 dev fails', () => bad({ license: 'LGPL-2.1', dev: true }));
test('CC-BY-4.0 dev passes', () => ok({ license: 'CC-BY-4.0', dev: true }));
test('CC-BY-4.0 dependency fails', () => bad({ license: 'CC-BY-4.0' }));
test('(MIT OR GPL-3.0) dependency passes', () => ok({ license: '(MIT OR GPL-3.0)' }));
test('(MIT AND GPL-3.0) dependency fails', () => bad({ license: '(MIT AND GPL-3.0)' }));
test('missing license fails', () => bad({}));
test('devOptional counts as dev', () => ok({ license: 'CC-BY-4.0', devOptional: true }));
test('link entries are skipped', () => ok({ link: true }));
test('AGPL dev fails', () => bad({ license: 'AGPL-3.0-only', dev: true }));
test('nested expression with parentheses', () => ok({ license: '(GPL-3.0 OR (MIT AND ISC))' }));
