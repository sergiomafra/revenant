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
test('devOptional CC-BY-4.0 fails (can ship)', () => bad({ license: 'CC-BY-4.0', devOptional: true }));
test('devOptional GPL-3.0 fails', () => bad({ license: 'GPL-3.0', devOptional: true }));
test('devOptional MIT passes', () => ok({ license: 'MIT', devOptional: true }));
test('license as object', () => ok({ license: { type: 'MIT' } }));
test('license as object, bad', () => bad({ license: { type: 'GPL-3.0' } }));
test('legacy licenses array is OR', () => ok({ licenses: [{ type: 'GPL-3.0' }, { type: 'MIT' }] }));
test('legacy licenses array, all bad', () => bad({ licenses: [{ type: 'GPL-3.0' }, { type: 'CC-BY-4.0' }] }));
test('lowercase or/and accepted', () => ok({ license: 'gpl-3.0 or MIT' }));
test('lowercase and fails', () => bad({ license: 'MIT and GPL-3.0' }));
test('WITH uses the base license', () => ok({ license: 'Apache-2.0 WITH LLVM-exception' }));
test('WITH on a bad base fails', () => bad({ license: 'GPL-2.0 WITH Classpath-exception-2.0' }));
test('AND binds tighter than OR', () => ok({ license: 'MIT OR GPL-3.0 AND GPL-2.0' }));
test('link entries are skipped', () => ok({ link: true }));
test('AGPL dev fails', () => bad({ license: 'AGPL-3.0-only', dev: true }));
test('nested expression with parentheses', () => ok({ license: '(GPL-3.0 OR (MIT AND ISC))' }));
