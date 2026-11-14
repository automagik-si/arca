import assert from 'node:assert/strict';
import { generateKeyPairSync, createVerify } from 'node:crypto';
import { test } from 'node:test';
import { GitHub } from '../src/github.js';

function fakeFetch(routes, seen) {
  return async (url, init = {}) => {
    seen.push({ url, method: init.method ?? 'GET', auth: init.headers?.authorization ?? null, redirect: init.redirect });
    const route = routes(url, init);
    return {
      ok: route.status >= 200 && route.status < 300,
      status: route.status,
      headers: new Map(Object.entries(route.headers ?? {})),
      json: async () => route.body,
      text: async () => route.body,
    };
  };
}

test('a GitHub App signs in with an RS256 JWT and a read-only installation token for one repository', async () => {
  const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const pem = privateKey.export({ type: 'pkcs1', format: 'pem' });
  const seen = [];
  let minted;
  const gh = new GitHub({
    appId: '42',
    appKey: pem,
    fetch: fakeFetch((url, init) => {
      if (url.endsWith('/repos/acme/app/installation')) return { status: 200, body: { id: 7 } };
      if (url.endsWith('/app/installations/7/access_tokens')) {
        minted = JSON.parse(init.body);
        return { status: 201, body: { token: 'inst-token', expires_at: new Date(Date.now() + 3600e3).toISOString() } };
      }
      return { status: 200, body: { ok: 1 } };
    }, seen),
  });
  const jwt = gh.appJwt(1_000_000);
  const [head, claims, signature] = jwt.split('.');
  assert.ok(createVerify('RSA-SHA256').update(`${head}.${claims}`).verify(publicKey, Buffer.from(signature, 'base64url')));
  assert.deepEqual(JSON.parse(Buffer.from(claims, 'base64url')), { iat: 999_940, exp: 1_000_540, iss: '42' });

  assert.deepEqual(await gh.get('acme/app', '/repos/acme/app'), { ok: 1 });
  assert.deepEqual(minted, { repositories: ['app'], permissions: { actions: 'read', contents: 'read', metadata: 'read' } });
  assert.equal(seen.at(-1).auth, 'Bearer inst-token');
  await gh.get('acme/app', '/repos/acme/app');
  assert.equal(seen.filter((s) => s.method === 'POST').length, 1, 'the token is reused');
});

test('a job log is fetched from its redirect without the token', async () => {
  const seen = [];
  const gh = new GitHub({
    token: 'secret-token',
    fetch: fakeFetch((url) => {
      if (url.includes('/logs')) return { status: 302, headers: { location: 'https://blob.example/log' } };
      return { status: 200, body: 'the log' };
    }, seen),
  });
  assert.equal(await gh.get('acme/app', '/repos/acme/app/actions/jobs/1/logs', { raw: true }), 'the log');
  assert.equal(seen[0].auth, 'Bearer secret-token');
  assert.equal(seen[0].redirect, 'manual');
  assert.equal(seen[1].url, 'https://blob.example/log');
  assert.equal(seen[1].auth, null);
  assert.ok(seen.every((s) => s.method === 'GET'));
});

test('a list is read page by page until a short page', async () => {
  const seen = [];
  const gh = new GitHub({
    token: 't',
    fetch: fakeFetch((url) => {
      const page = Number(new URL(url).searchParams.get('page'));
      return { status: 200, body: { items: Array.from({ length: page === 1 ? 100 : 3 }, (_, k) => k) } };
    }, seen),
  });
  assert.equal((await gh.all('acme/app', '/x?a=1', 'items')).length, 103);
  assert.match(seen[0].url, /\/x\?a=1&per_page=100&page=1$/);
});
