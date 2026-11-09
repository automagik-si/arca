import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import {
  APPROVAL_REFUSAL_CODES, approvalErrorText, approvalRefusalText,
} from '../src/render.js';

const UNCLEAR = 'Arca could not finish this. Check Steps to see whether it ran.';

// crates/arca/src/handlers.rs refusal_code, plus the NoSuchPlan answer from no_such_plan().
const KERNEL_CODES = [
  'Malformed', 'NotCanonical', 'NoVerifier', 'BadSignature', 'NotHardwareKey',
  'NoUserPresence', 'NoUserVerification', 'WrongOrigin', 'WrongRp', 'SignCountRegressed',
  'WrongNamespace', 'WrongHost', 'WrongAction', 'WrongTarget', 'WrongParams', 'IssuedAhead',
  'Expired', 'TtlTooLong', 'Replayed', 'Untrusted', 'Storage', 'Unresolved',
];
const ALL_CODES = [...KERNEL_CODES, 'NoSuchPlan'];

// The action branch runs exactly these handler sources after the passkey is spent. Their refusal
// codes are snake_case and must never overlap the PascalCase approval refusals.
const HANDLER_SOURCES = ['handlers.rs', 'service_exec.rs', 'service_wiring.rs'];
const HANDLER_LITERALS = new URL('../../../crates/arca/src/', import.meta.url);

async function realHandlerCodes() {
  const sources = await Promise.all(HANDLER_SOURCES.map((name) => readFile(new URL(name, HANDLER_LITERALS), 'utf8')));
  const codes = new Set();
  for (const source of sources) {
    for (const match of source.matchAll(/\bcode:\s*"([a-z][a-z0-9_]*)"/g)) codes.add(match[1]);
    for (const match of source.matchAll(/\bRefused\(\s*"([a-z][a-z0-9_]*)"\s*\)/g)) codes.add(match[1]);
    for (const match of source.matchAll(/\bRefused\(\s*((?:"[a-z][a-z0-9_]*"\s*\|\s*)*"[a-z][a-z0-9_]*")\s*\)/g)) {
      for (const literal of match[1].matchAll(/"([a-z][a-z0-9_]*)"/g)) codes.add(literal[1]);
    }
  }
  return codes;
}

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

test('the real kernel handler codes are disjoint from the approval set and read as the action error', async () => {
  const codes = await realHandlerCodes();
  for (const code of ['refused', 'not_found', 'adopted', 'owner_approval', 'exec', 'target', 'stopped', 'hung']) {
    assert.ok(codes.has(code), `${code} is a real handler code`);
  }
  for (const code of codes) {
    assert.match(code, /^[a-z][a-z0-9_]*$/, code);
    assert.equal(APPROVAL_REFUSAL_CODES.includes(code), false, `${code} is not an approval refusal`);
    const sentence = approvalErrorText({ refusal: code, message: `service ${code}` });
    assert.equal(sentence, `The change was approved, but Arca could not run it: Service ${code}.`);
    assert.doesNotMatch(sentence, /did not accept this approval/, code);
  }
  for (const code of APPROVAL_REFUSAL_CODES) assert.equal(codes.has(code), false, `${code} is not a handler code`);
});

test('any other non-empty refusal or message is the action error after the passkey was spent', () => {
  assert.equal(
    approvalErrorText({ refusal: 'FutureRefusal', message: 'a later handler refused it' }),
    'The change was approved, but Arca could not run it: A later handler refused it.',
  );
  assert.equal(
    approvalErrorText({ refusal: 'not_found', message: 'The service nats: not found' }),
    'The change was approved, but Arca could not run it: The service nats: not found.',
  );
  assert.equal(
    approvalErrorText({ message: 'the service nats: not found.' }),
    'The change was approved, but Arca could not run it: The service nats: not found.',
  );
});

test('an empty or unreadable body is unclear, never approved and never a retry', () => {
  for (const body of [null, undefined, {}, { message: '' }, { message: '   ' }, { refusal: 42 }]) {
    const sentence = approvalErrorText(body);
    assert.equal(sentence, UNCLEAR, JSON.stringify(body));
    assert.doesNotMatch(sentence, /Try again|approved|did not accept/i);
  }
});

test('a known approval refusal never becomes the approved wording', () => {
  for (const code of ALL_CODES.filter((code) => code !== 'NoSuchPlan')) {
    const sentence = approvalErrorText({ refusal: code, message: 'a handler message' });
    assert.equal(sentence, approvalRefusalText(code), code);
  }
  assert.equal(approvalErrorText({ refusal: 'NoSuchPlan', message: 'gone' }), approvalRefusalText('NoSuchPlan'));
});
