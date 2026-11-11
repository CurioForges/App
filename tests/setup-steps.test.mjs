import test from 'node:test';
import assert from 'node:assert/strict';
import {PAGE_ORDER, STEPS, orderedSteps} from '../src/setup-steps.mjs';

test('every wizard page is shown, and only existing pages are shown', () => {
  assert.equal(new Set(PAGE_ORDER).size, PAGE_ORDER.length, 'PAGE_ORDER has no duplicates');
  assert.deepEqual(
    [...PAGE_ORDER].sort(),
    STEPS.map(step => step.id).sort(),
    'PAGE_ORDER and STEPS describe the same page set',
  );
  assert.equal(orderedSteps().length, STEPS.length);
});

test('the first-run order is weights, token, then data', () => {
  // Global choices come first; the data directory belongs to one installation.
  assert.deepEqual(PAGE_ORDER, ['models', 'token', 'data']);
  assert.ok(STEPS.every(step => step.title && step.caption && step.count > 0));
});
