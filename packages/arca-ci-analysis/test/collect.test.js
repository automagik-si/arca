// What reading GitHub costs and what happens when it stops: B1 to B4 of the first real run (a repository with 6000
// runs in 14 days spent a person's whole rate limit, waited too little for it and kept nothing).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { analyze } from '../src/analyze.js';
import { collect, DEFAULTS, fitBudget, requestPlan } from '../src/collect.js';
import { access, BudgetSpent, GitHub, RateLimited, tokenKind } from '../src/github.js';
import { markdown } from '../src/report.js';

const NOW = Date.parse('2026-11-14T12:00:00Z');

function response(status, body, headers = {}) {
  return { ok: status >= 200 && status < 300, status, headers: new Map(Object.entries(headers)), json: async () => body, text: async () => body };
}

function client(answers, options = {}) {
  const calls = [];
  const slept = [];
  const gh = new GitHub({
    token: 'ghs_test',
    now: () => NOW,
    sleep: async (ms) => slept.push(ms),
    fetch: async (url) => {
      calls.push(url);
      return answers(url, calls.length);
    },
    ...options,
  });
  return { gh, calls, slept };
}

test('B1: a rate limit waits for its reset, however far, and costs no attempt; past maxWaitMs it stops', { timeout: 5000 }, async () => {
  const reset = String((NOW + 30 * 60 * 1000) / 1000);
  const limited = response(403, {}, { 'x-ratelimit-remaining': '0', 'x-ratelimit-reset': reset });
  // Six rate limits in a row, more than the four attempts a server error gets.
  const { gh, slept } = client((url, n) => (n <= 6 ? limited : response(200, { ok: 1 })), { maxWaitMs: 4 * 60 * 60 * 1000 });
  assert.deepEqual(await gh.get('acme/app', '/repos/acme/app'), { ok: 1 });
  assert.deepEqual(slept, Array(6).fill(30 * 60 * 1000 + 5000));

  const short = client(() => limited, { maxWaitMs: 10 * 60 * 1000 });
  await assert.rejects(short.gh.get('acme/app', '/repos/acme/app'), RateLimited);
  assert.deepEqual(short.slept, [], 'it does not wait for a reset it cannot reach');
});

test('B2: a server error backs off 2, 4, 8, 16 s, whatever the rate-limit reset says', async () => {
  const reset = String((NOW + 50 * 60 * 1000) / 1000);
  const { gh, slept } = client((url, n) => (n <= 2 ? response(502, {}, { 'x-ratelimit-reset': reset, 'x-ratelimit-remaining': '4000' }) : response(200, [])));
  assert.deepEqual(await gh.get('acme/app', '/repos/acme/app/actions/runs/1/jobs'), []);
  assert.deepEqual(slept, [2000, 4000]);

  const down = client(() => response(503, {}));
  await assert.rejects(down.gh.get('acme/app', '/x'), /GitHub answered 503/);
  assert.deepEqual(down.slept, [2000, 4000, 8000, 16000]);
});

test('B4: the budget is a hard stop, counted per request', async () => {
  const { gh, calls } = client(() => response(200, {}), { budget: 2 });
  await gh.get('acme/app', '/a');
  await gh.get('acme/app', '/b');
  await assert.rejects(gh.get('acme/app', '/c'), BudgetSpent);
  assert.equal(calls.length, 2);
  assert.equal(gh.requests, 2);
});

test('B4: a person\'s token is not used unless asked; an installation token or the App is', async () => {
  assert.equal(tokenKind('gho_abc'), 'user');
  assert.equal(tokenKind('ghp_abc'), 'user');
  assert.equal(tokenKind('github_pat_abc'), 'user');
  assert.equal(tokenKind('ghs_abc'), 'installation');
  assert.equal(tokenKind('0123456789abcdef0123456789abcdef01234567'), 'unknown');

  await assert.rejects(access({}, { GITHUB_TOKEN: 'gho_from_gh_auth_token' }), /GITHUB_TOKEN holds a person's token.*--allow-user-token/);
  // The same check, whichever input names the token: GITHUB_INSTALLATION_TOKEN=$(gh auth token) is a person's token.
  const file = async () => 'gho_in_a_file\n';
  await assert.rejects(access({ tokenFile: 't.txt' }, {}, file), /--installation-token-file holds a person's token/);
  await assert.rejects(access({}, { GITHUB_INSTALLATION_TOKEN: 'ghp_in_the_env' }), /GITHUB_INSTALLATION_TOKEN holds a person's token/);
  assert.deepEqual(await access({ tokenFile: 't.txt', allowUserToken: true }, {}, file), { token: 'gho_in_a_file', how: "a person's token (--allow-user-token)" });
  assert.deepEqual(await access({ allowUserToken: true }, { GITHUB_INSTALLATION_TOKEN: 'ghp_in_the_env' }), { token: 'ghp_in_the_env', how: "a person's token (--allow-user-token)" });
  await assert.rejects(access({}, { GITHUB_TOKEN: '0123456789abcdef0123456789abcdef01234567' }), /a person's token/);
  assert.equal((await access({ allowUserToken: true }, { GITHUB_TOKEN: 'gho_x' })).token, 'gho_x');
  assert.equal((await access({}, { GITHUB_TOKEN: 'ghs_x' })).token, 'ghs_x');
  // The installation token wins over the others; the file is read, never printed.
  const read = async (path) => (path === 'token.txt' ? 'ghs_from_file\n' : '');
  const fromFile = await access({ tokenFile: 'token.txt' }, { GITHUB_TOKEN: 'gho_x', GITHUB_INSTALLATION_TOKEN: 'ghs_env' }, read);
  assert.deepEqual(fromFile, { token: 'ghs_from_file', how: 'an App installation token' });
  assert.equal((await access({}, { GITHUB_INSTALLATION_TOKEN: 'ghs_env', GITHUB_TOKEN: 'gho_x' })).token, 'ghs_env');
  const app = await access({}, { GITHUB_APP_ID: '7', GITHUB_APP_KEY_B64: Buffer.from('PEM').toString('base64'), GITHUB_TOKEN: 'gho_x' });
  assert.deepEqual(app, { appId: '7', appKey: 'PEM', how: 'the GitHub App 7' });
  for (const way of [fromFile, app]) assert.doesNotMatch(way.how, /ghs_|gho_|PEM/);
});

test('B4: the plan bounds the requests before the first one, and the budget shrinks runs and logs to fit', () => {
  assert.equal(requestPlan(), 1 + 7 + 3 + 300 + 20 + 2 + 100);
  assert.equal(requestPlan({ days: 14, app: true }), 1 + 14 + 3 + 300 + 20 + 2 + 100 + 2);
  assert.deepEqual(fitBudget(1, 600), DEFAULTS, 'one repository fits the default budget as it is');
  const three = fitBudget(3, 600, { days: 14 });
  assert.ok(requestPlan(three) * 3 <= 600, `${requestPlan(three)} a repository`);
  assert.ok(three.maxRuns >= 20 && three.maxRuns < 300);
  assert.throws(() => fitBudget(20, 600, { days: 14 }), /a budget of 600 requests is too small for 20 repositories/);
});

/** A repository with `perDay` completed runs a day, each with one job, served by the fake GitHub below. */
function repository(perDay, days) {
  const runs = [];
  for (let day = 0; day < days; day += 1) {
    for (let k = 0; k < perDay; k += 1) {
      const at = NOW - day * 86400e3 - (k + 1) * 600e3;
      runs.push({ id: runs.length + 1, workflow_id: 1, name: 'ci', path: '.github/workflows/ci.yml', event: 'push', head_sha: `s${runs.length}`, head_branch: 'main', run_attempt: 1, status: 'completed', conclusion: 'success', created_at: new Date(at).toISOString(), run_started_at: new Date(at).toISOString(), updated_at: new Date(at + 60e3).toISOString(), pull_requests: [] });
    }
  }
  return (url) => {
    const path = url.replace('https://api.github.com', '');
    if (path === '/repos/acme/app') return response(200, { default_branch: 'main', private: false });
    const created = /created=([^&]+)/.exec(path);
    if (created) {
      const [from, to] = decodeURIComponent(created[1]).split('..').map(Date.parse);
      const page = Number(/[?&]page=(\d+)/.exec(path)[1]);
      const inRange = runs.filter((run) => Date.parse(run.created_at) >= from && Date.parse(run.created_at) <= to + 999);
      return response(200, { workflow_runs: inRange.slice((page - 1) * 100, page * 100) });
    }
    const jobs = /runs\/(\d+)\/jobs/.exec(path);
    if (jobs) {
      const run = runs[Number(jobs[1]) - 1];
      return response(200, { jobs: [{ id: run.id * 10, run_attempt: 1, name: 'build', conclusion: 'success', created_at: run.created_at, started_at: run.created_at, completed_at: run.updated_at, labels: ['ubuntu-latest'], steps: [] }] });
    }
    if (path.includes('/contents/')) return response(200, { content: Buffer.from('on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make\n').toString('base64') });
    if (path.includes('/cache/usage')) return response(200, { active_caches_size_in_bytes: 0, active_caches_count: 0 });
    if (path.includes('/caches')) return response(200, { actions_caches: [] });
    return response(404, {});
  };
}

test('B4: a busy repository is sampled: the newest maxRuns runs, and the window is the span they cover', async () => {
  const { gh, calls } = client(repository(40, 14));
  const data = await collect(gh, 'acme/app', { days: 14, maxRuns: 50, until: new Date(NOW).toISOString() });
  assert.equal(data.runs.length, 50);
  assert.equal(data.window.sampled.maxRuns, 50);
  assert.equal(data.window.sampled.daysAsked, 14);
  assert.ok(data.window.days > 1 && data.window.days < 2, `${data.window.days} days`);
  assert.ok(calls.length <= requestPlan({ days: 14, maxRuns: 50 }), `${calls.length} requests`);
  assert.equal(calls.filter((url) => /\/runs\/\d+\/jobs/.test(url)).length, 50, 'one jobs request a run kept, none for the rest');
  assert.match(markdown(analyze(data)), /Sampled: the newest 50 completed runs of the 14 days asked/);
});

test('B3: when the budget runs out part-way, the dataset keeps what was read and says what is missing', async () => {
  const { gh } = client(repository(10, 3), { budget: 20 });
  const data = await collect(gh, 'acme/app', { days: 3, until: new Date(NOW).toISOString() });
  assert.match(data.partial.reason, /the request budget of 20 is spent/);
  assert.ok(data.partial.runsWithoutJobs > 0);
  assert.ok(data.runs.length > 0, 'the runs whose jobs were read are kept');
  assert.equal(data.runs.length + data.partial.runsWithoutJobs, 30);
  assert.equal(new Set(data.jobs.map((job) => job.runId)).size, data.runs.length, 'no run without its jobs');
  const report = analyze(data);
  assert.match(markdown(report), /\*\*Partial:\*\* not everything was read \(the request budget of 20 is spent\)/);
});

test('L3: signing in as the App is tried again on a 5xx or a 429, within the budget; a 404 is final', async () => {
  const { generateKeyPairSync } = await import('node:crypto');
  const pem = generateKeyPairSync('rsa', { modulusLength: 2048 }).privateKey.export({ type: 'pkcs1', format: 'pem' });
  const signIn = (lookup) =>
    client(
      (url, n) => {
        if (url.endsWith('/installation')) return lookup(n);
        if (url.endsWith('/access_tokens')) return response(201, { token: 'ghs_minted', expires_at: new Date(NOW + 3600e3).toISOString() });
        return response(200, { ok: 1 });
      },
      { token: undefined, appId: '7', appKey: pem },
    );
  const flaky = signIn((n) => (n === 1 ? response(502, {}) : response(200, { id: 9 })));
  assert.deepEqual(await flaky.gh.get('acme/app', '/repos/acme/app'), { ok: 1 });
  assert.deepEqual(flaky.slept, [2000]);
  assert.equal(flaky.gh.requests, 4, 'the failed lookup counts against the budget');

  const busy = signIn(() => response(429, {}));
  await assert.rejects(busy.gh.get('acme/app', '/repos/acme/app'), (error) => /could not look up the App on acme\/app: GitHub answered 429/.test(error.message));
  assert.equal(busy.slept.length, 4);

  const absent = signIn(() => response(404, {}));
  await assert.rejects(absent.gh.get('acme/app', '/repos/acme/app'), /the App is not installed on acme\/app: GitHub answered 404/);
  assert.deepEqual(absent.slept, [], 'not installed is not tried again');
});

test('L2: a day whose listing keeps failing is skipped and named; a run whose jobs keep failing is skipped, not the rest', async () => {
  const serve = repository(10, 7);
  const thirdDay = new Date(NOW - 2 * 86400e3).toISOString().slice(0, 13);
  const { gh } = client((url) => {
    if (url.includes('created=') && decodeURIComponent(url).includes(thirdDay)) return response(502, {});
    if (url.includes('/runs/5/jobs')) return response(502, {});
    return serve(url);
  });
  const data = await collect(gh, 'acme/app', { days: 7, until: new Date(NOW).toISOString() });
  assert.equal(data.partial.daysUnlisted, 1);
  assert.equal(data.partial.runsWithoutJobs, 1);
  assert.match(data.partial.reason, /GitHub kept failing on 2 request\(s\)/);
  assert.equal(data.runs.length, 60 - 1, 'six days listed, one run without its jobs');
  assert.ok(data.logs && data.caches.activeCount === 0, 'the caches and logs are still read');
  assert.match(markdown(analyze(data)), /Not read: the runs of 1 day\(s\), the jobs of 1 run\(s\)/);
});

test('L2: when the budget stops the listing, the window is the days it got to, and the note says so', async () => {
  const { gh } = client(repository(5, 7), { budget: 4 });
  const data = await collect(gh, 'acme/app', { days: 7, until: new Date(NOW).toISOString() });
  assert.equal(data.partial.daysUnlisted, 4, 'the repository and three days listed, then the budget');
  assert.equal(data.window.days, 3);
  assert.match(markdown(analyze(data)), /the runs of 4 day\(s\).*the window is the 3 days the listing got to/);
});
