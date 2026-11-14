// The GitHub REST client the analysis reads with: read-only, GET only. It signs in either with a token it is given
// (GITHUB_TOKEN) or as a GitHub App installed on the repository (GITHUB_APP_ID and the App's private key), and it
// never prints either. Every request is a GET; the one redirect it follows (a job's log) is fetched without the token.

import { createSign } from 'node:crypto';

const API = 'https://api.github.com';

export class GitHub {
  /**
   * @param {object} options
   * @param {string} [options.token] a token that can read the repository
   * @param {string} [options.appId] a GitHub App's id; with appKey, the client signs in as its installation
   * @param {string} [options.appKey] the App's private key, PEM
   * @param {number} [options.concurrency] how many requests may be in flight
   * @param {Function} [options.fetch] for tests
   */
  constructor({ token, appId, appKey, concurrency = 6, fetch: fetchImpl = globalThis.fetch, log = () => {} } = {}) {
    this.token = token ?? null;
    this.appId = appId ?? null;
    this.appKey = appKey ?? null;
    this.fetch = fetchImpl;
    this.log = log;
    this.slots = concurrency;
    this.waiting = [];
    this.installation = new Map();
    this.requests = 0;
  }

  async acquire() {
    if (this.slots > 0) {
      this.slots -= 1;
      return;
    }
    await new Promise((resolve) => this.waiting.push(resolve));
  }

  release() {
    const next = this.waiting.shift();
    if (next) next();
    else this.slots += 1;
  }

  /** The App's JWT: RS256 over {iat, exp, iss}, valid 9 minutes, back-dated 60 s for clock skew. */
  appJwt(now = Math.floor(Date.now() / 1000)) {
    const encode = (value) => Buffer.from(JSON.stringify(value)).toString('base64url');
    const body = `${encode({ alg: 'RS256', typ: 'JWT' })}.${encode({ iat: now - 60, exp: now + 540, iss: String(this.appId) })}`;
    const signature = createSign('RSA-SHA256').update(body).sign(this.appKey).toString('base64url');
    return `${body}.${signature}`;
  }

  /** The header that authorises a request for `repo` (owner/name). */
  async authorization(repo) {
    if (this.token) return `Bearer ${this.token}`;
    if (!this.appId || !this.appKey) return null;
    const held = this.installation.get(repo);
    if (held && held.expires - Date.now() > 5 * 60 * 1000) return `Bearer ${held.token}`;
    const jwt = this.appJwt();
    const headers = { authorization: `Bearer ${jwt}`, accept: 'application/vnd.github+json', 'x-github-api-version': '2022-11-28' };
    const found = await this.fetch(`${API}/repos/${repo}/installation`, { headers });
    if (!found.ok) throw new Error(`the App is not installed on ${repo}: GitHub answered ${found.status}`);
    const { id } = await found.json();
    // Creating an installation token is the one POST the analysis makes. The token is narrowed to this one
    // repository and to read permissions only.
    const minted = await this.fetch(`${API}/app/installations/${id}/access_tokens`, {
      method: 'POST',
      headers,
      body: JSON.stringify({
        repositories: [repo.split('/')[1]],
        permissions: { actions: 'read', contents: 'read', metadata: 'read' },
      }),
    });
    if (!minted.ok) throw new Error(`no installation token for ${repo}: GitHub answered ${minted.status}`);
    const { token, expires_at: expiresAt } = await minted.json();
    this.installation.set(repo, { token, expires: Date.parse(expiresAt) });
    return `Bearer ${token}`;
  }

  /** GET `path` (relative to the API) for `repo`, as JSON. Retries rate limits and server errors a few times. */
  async get(repo, path, { accept = 'application/vnd.github+json', raw = false } = {}) {
    const url = path.startsWith('https://') ? path : `${API}${path}`;
    for (let attempt = 0; ; attempt += 1) {
      await this.acquire();
      let response;
      try {
        const auth = await this.authorization(repo);
        const headers = { accept, 'x-github-api-version': '2022-11-28', 'user-agent': 'arca-ci-analysis' };
        if (auth) headers.authorization = auth;
        this.requests += 1;
        response = await this.fetch(url, { headers, redirect: raw ? 'manual' : 'follow' });
        if (raw && response.status >= 300 && response.status < 400) {
          const location = response.headers.get('location');
          if (!location) return null;
          // The log is on blob storage: fetched without the token.
          const blob = await this.fetch(location);
          return blob.ok ? await blob.text() : null;
        }
      } catch (error) {
        // A connection that broke is tried again, as a server error is.
        if (attempt < 4) {
          this.log(`GET ${path} failed (${error.message}); again in ${2 * 2 ** attempt} s`);
          await new Promise((resolve) => setTimeout(resolve, 2000 * 2 ** attempt));
          continue;
        }
        throw error;
      } finally {
        this.release();
      }
      if (response.ok) return raw ? response.text() : response.json();
      const limited =
        response.status === 429 ||
        (response.status === 403 && response.headers.get('x-ratelimit-remaining') === '0');
      if ((limited || response.status >= 500) && attempt < 4) {
        const reset = Number(response.headers.get('x-ratelimit-reset') ?? 0) * 1000;
        const retryAfter = Number(response.headers.get('retry-after') ?? 0) * 1000;
        const wait = Math.min(Math.max(retryAfter, reset - Date.now(), 2000 * 2 ** attempt), 120_000);
        this.log(`GitHub answered ${response.status} for ${path}; again in ${Math.round(wait / 1000)} s`);
        await new Promise((resolve) => setTimeout(resolve, wait));
        continue;
      }
      if (response.status === 404 || response.status === 410) return null;
      throw new Error(`GitHub answered ${response.status} for GET ${path}`);
    }
  }

  /** Every page of a list endpoint; `key` names the array in each page (null when the page is the array). */
  async all(repo, path, key, { max = Infinity } = {}) {
    const out = [];
    const join = path.includes('?') ? '&' : '?';
    for (let page = 1; out.length < max; page += 1) {
      const body = await this.get(repo, `${path}${join}per_page=100&page=${page}`);
      if (!body) break;
      const items = key ? body[key] ?? [] : body;
      out.push(...items);
      if (items.length < 100) break;
    }
    return out.slice(0, max);
  }
}
