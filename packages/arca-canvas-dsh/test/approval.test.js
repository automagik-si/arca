import assert from 'node:assert/strict';
import test from 'node:test';
import {
  APPROVAL_REFUSAL_CODES, approvalErrorText, approvalRefusalText,
} from '../src/render.js';

const NEUTRAL = 'Arca did not accept this approval. Try again from the card.';

// crates/arca/src/handlers.rs refusal_code, plus the NoSuchPlan answer from no_such_plan().
const KERNEL_CODES = [
  'Malformed', 'NotCanonical', 'NoVerifier', 'BadSignature', 'NotHardwareKey',
  'NoUserPresence', 'NoUserVerification', 'WrongOrigin', 'WrongRp', 'SignCountRegressed',
  'WrongNamespace', 'WrongHost', 'WrongAction', 'WrongTarget', 'WrongParams', 'IssuedAhead',
  'Expired', 'TtlTooLong', 'Replayed', 'Untrusted', 'Storage', 'Unresolved',
];
const ALL_CODES = [...KERNEL_CODES, 'NoSuchPlan'];

test('every kernel refusal code has a plain sentence and none claims the change was approved', () => {
  assert.deepEqual([...APPROVAL_REFUSAL_CODES].sort(), [...ALL_CODES].sort());
  assert.equal(APPROVAL_REFUSAL_CODES.length, 23);
  for (const code of ALL_CODES) {
    const sentence = approvalRefusalText(code);
    assert.equal(typeof sentence, 'string', code);
    assert.ok(sentence.trim().length > 10, `${code} has a sentence`);
    assert.match(sentence, /^[A-Z].*\.$/, `${code} is a plain sentence`);
    assert.doesNotMatch(sentence, /The change was approved/i, code);
    assert.doesNotMatch(sentence, /could not run it/i, code);
  }
});

test('NoSuchPlan keeps the no-longer-waiting sentence that points at Steps', () => {
  assert.equal(
    approvalRefusalText('NoSuchPlan'),
    'This proposal is no longer waiting. It may have been approved or declined elsewhere. Check Steps.',
  );
});

test('an unknown refusal code is neutral and never claims the change was approved', () => {
  for (const body of [
    { refusal: 'SomeFutureRefusal', message: 'a new kernel says no' },
    { refusal: 'some_future_refusal' },
    { refusal: 42 },
  ]) {
    const sentence = approvalErrorText(body);
    assert.equal(sentence, NEUTRAL, JSON.stringify(body));
    assert.doesNotMatch(sentence, /approved/i);
  }
});

test('an empty or unreadable body is neutral and never claims the change was approved', () => {
  for (const body of [null, undefined, {}, { message: '' }, { message: '   ' }]) {
    const sentence = approvalErrorText(body);
    assert.equal(sentence, NEUTRAL, JSON.stringify(body));
    assert.doesNotMatch(sentence, /approved/i);
  }
});

test("an action's own message after a spent approval says approved but could not run", () => {
  assert.equal(
    approvalErrorText({ message: 'The service nats: not found' }),
    'The change was approved, but Arca could not run it: The service nats: not found.',
  );
  assert.equal(
    approvalErrorText({ message: 'the service nats: not found.' }),
    'The change was approved, but Arca could not run it: The service nats: not found.',
  );
});
