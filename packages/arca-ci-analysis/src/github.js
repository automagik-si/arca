// The GitHub REST client the analysis reads with: read-only, GET only. It signs in with a token it is given (an App
// installation token; a person's token only when asked, see access()) or as a GitHub App installed on the repository
// (GITHUB_APP_ID and the App's private key), and it never prints either. Every request is a GET; the one redirect it follows (a job's log) is fetched without the token.

import { createSign } from 'node:crypto';
import { readFile } from 'node:fs/promises';

const API = 'https://api.github.com';

/**
 * What a token is, from its prefix: `installation` (a GitHub App installation, `ghs_`), whose rate limit is the App's
 * own on that installation; `user` (`gho_` from `gh auth token`, `ghp_`, `github_pat_`, `ghu_`), whose limit is shared
 * with everything that person runs; or `unknown` (an older 40-hex token, read as a person's).
 */
export function tokenKind(token) {
  if (!token) return null;
  if (token.startsWith('ghs_')) return 'installation';
  if (/^(gho_|ghp_|ghu_|github_pat_)/.test(token)) return 'user';
  return 'unknown';
}

/**
 * How a run signs in, from its arguments and the environment, without a request: an App installation token (from
 * `args.tokenFile` or GITHUB_INSTALLATION_TOKEN), else the App's id and key (it mints one per repository), else
 * GITHUB_TOKEN, which must be an installation token unless `args.allowUserToken` says a person's own token may be
 * spent. Returns `{ token?, appId?, appKey?, how }`; `how` names the way in words and never holds a secret.
 */
export async function access(args, env = process.env, read = (path) => readFile(path, 'utf8')) {
  // Whatever names a token, a person's token is checked the same way: GITHUB_INSTALLATION_TOKEN=$(gh auth token) is
  // still a person's token.
  const checked = (token, where) => {
    if (tokenKind(token) === 'installation') return { token, how: 'an App installation token' };
    if (!args.allowUserToken) {
      throw new Error(
        `${where} holds a person's token, not an App installation token, and its rate limit is shared with everything that person runs; give an App installation token (--installation-token-file or GITHUB_INSTALLATION_TOKEN) or the App's id and key, or pass --allow-user-token`,
      );
    }
    return { token, how: "a person's token (--allow-user-token)" };
  };
  if (args.tokenFile) {
    const fromFile = (await read(args.tokenFile)).trim();
    if (fromFile) return checked(fromFile, '--installation-token-file');
  }
  if (env.GITHUB_INSTALLATION_TOKEN) return checked(env.GITHUB_INSTALLATION_TOKEN, 'GITHUB_INSTALLATION_TOKEN');
  const appKey = env.GITHUB_APP_KEY_B64 ? Buffer.from(env.GITHUB_APP_KEY_B64, 'base64').toString('utf8') : env.GITHUB_APP_KEY;
  if (env.GITHUB_APP_ID && appKey) return { appId: env.GITHUB_APP_ID, appKey, how: `the GitHub App ${env.GITHUB_APP_ID}` };
  if (!env.GITHUB_TOKEN) return { how: 'no token (public repositories only, 60 requests an hour)' };
  return checked(env.GITHUB_TOKEN, 'GITHUB_TOKEN');
}

/** The request budget of a run is spent: the collector keeps what it has read so far. */
export class BudgetSpent extends Error {
  constructor(budget) {
    super(`the request budget of ${budget} is spent`);
    this.code = 'budget';
  }
}

/** GitHub's rate limit would not reset in the time the client may wait. */
export class RateLimited extends Error {
  constructor(resetAt) {
    super(`GitHub's rate limit is spent until ${new Date(resetAt).toISOString()}`);
    this.code = 'rate-limit';
  }
}

export class GitHub {
  /**
   * @param {object} options
   * @param {string} [options.token] a token that can read the repository
   * @param {string} [options.appId] a GitHub App's id; with appKey, the client signs in as its installation
   * @param {string} [options.appKey] the App's private key, PEM
   * @param {number} [options.concurrency] how many requests may be in flight
   * @param {number} [options.budget] the most REST requests this client makes; the next one throws BudgetSpent
   * @param {number} [options.maxWaitMs] the most time it waits in all for rate limits to reset
   * @param {Function} [options.fetch] for tests
   * @param {Function} [options.sleep] for tests: (ms) => Promise
   */
  constructor({
    token,
    appId,
    appKey,
    concurrency = 6,
    budget = Infinity,
    maxWaitMs = 70 * 60 * 1000,
    fetch: fetchImpl = globalThis.fetch,
    sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
    now = () => Date.now(),
    log = () => {},
  } = {}) {
    this.token = token ?? null;
    this.appId = appId ?? null;
    this.appKey = appKey ?? null;
    this.fetch = fetchImpl;
    this.sleep = sleep;
    this.now = now;
    this.log = log;
    this.slots = concurrency;
    this.waiting = [];
    this.installation = new Map();
    this.requests = 0;
    this.budget = budget;
    this.maxWaitMs = maxWaitMs;
    this.waited = 0;
  }

  /** Counts one REST request against the budget, or throws when the budget is spent. */
  spend() {
    if (this.requests >= this.budget) throw new BudgetSpent(this.budget);
    this.requests += 1;
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
    // A 5xx or 429 while signing in is tried again by get(), as any request is; any other answer is final.
    const refused = (what, status) =>
      Object.assign(new Error(`${what} ${repo}: GitHub answered ${status}`), { code: status >= 500 || status === 429 ? 'retry' : 'auth' });
    this.spend();
    const found = await this.fetch(`${API}/repos/${repo}/installation`, { headers });
    if (!found.ok) throw refused(found.status >= 500 || found.status === 429 ? 'could not look up the App on' : 'the App is not installed on', found.status);
    const { id } = await found.json();
    // Creating an installation token is the one POST the analysis makes. The token is narrowed to this one
    // repository and to read permissions only.
    this.spend();
    const minted = await this.fetch(`${API}/app/installations/${id}/access_tokens`, {
      method: 'POST',
      headers,
      body: JSON.stringify({
        repositories: [repo.split('/')[1]],
        permissions: { actions: 'read', contents: 'read', metadata: 'read' },
      }),
    });
    if (!minted.ok) throw refused('no installation token for', minted.status);
    const { token, expires_at: expiresAt } = await minted.json();
    this.installation.set(repo, { token, expires: Date.parse(expiresAt) });
    return `Bearer ${token}`;
  }

  /**
   * GET `path` (relative to the API) for `repo`, as JSON. A server error or a broken connection is tried again after
   * 2, 4, 8 and 16 s. A rate limit waits for its reset, which costs no attempt, as long as the waits in all stay under
   * `maxWaitMs`; past that it throws RateLimited. Each request counts against the budget (BudgetSpent).
   */
  async get(repo, path, { accept = 'application/vnd.github+json', raw = false } = {}) {
    const url = path.startsWith('https://') ? path : `${API}${path}`;
    for (let attempt = 0; ; attempt += 1) {
      await this.acquire();
      let response;
      try {
        const auth = await this.authorization(repo);
        const headers = { accept, 'x-github-api-version': '2022-11-28', 'user-agent': 'arca-ci-analysis' };
        if (auth) headers.authorization = auth;
        this.spend();
        response = await this.fetch(url, { headers, redirect: raw ? 'manual' : 'follow' });
        if (raw && response.status >= 300 && response.status < 400) {
          const location = response.headers.get('location');
          if (!location) return null;
          // The log is on blob storage: fetched without the token.
          const blob = await this.fetch(location);
          return blob.ok ? await blob.text() : null;
        }
      } catch (error) {
        // A spent budget or a refused sign-in is not tried again; a connection that broke is, as a server error is.
        if (error.code !== 'budget' && error.code !== 'auth' && attempt < 4) {
          this.log(`GET ${path} failed (${error.message}); again in ${2 * 2 ** attempt} s`);
          await this.sleep(2000 * 2 ** attempt);
          continue;
        }
        throw error;
      } finally {
        this.release();
      }
      if (response.ok) return raw ? response.text() : response.json();
      const retryAfter = Number(response.headers.get('retry-after') ?? 0) * 1000;
      const limited =
        response.status === 429 ||
        (response.status === 403 && (response.headers.get('x-ratelimit-remaining') === '0' || retryAfter > 0));
      if (limited) {
        const reset = Number(response.headers.get('x-ratelimit-reset') ?? 0) * 1000;
        const wait = retryAfter || (reset ? Math.max(reset - this.now() + 5000, 1000) : 60_000);
        if (this.waited + wait > this.maxWaitMs) throw new RateLimited(reset || this.now() + wait);
        this.waited += wait;
        this.log(`GitHub's rate limit for ${path}; again in ${Math.round(wait / 1000)} s`);
        await this.sleep(wait);
        attempt -= 1; // waiting for the reset costs no attempt
        continue;
      }
      if (response.status >= 500 && attempt < 4) {
        const wait = Math.max(retryAfter, 2000 * 2 ** attempt);
        this.log(`GitHub answered ${response.status} for ${path}; again in ${Math.round(wait / 1000)} s`);
        await this.sleep(wait);
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
