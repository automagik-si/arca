// What the analysis reads in a job's log: each cache restore and whether it hit, how the cache was compressed, and the
// tests the job ran (which failed, and the slow ones with their times). The text itself is not kept.

/** A test slower than this is kept by name; faster ones are only counted. */
export const SLOW_TEST_MS = 2000;

const STAMP = /^\d{4}-\d\d-\d\dT[\d:.]+Z ?/;
// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-9;]*[A-Za-z]/g;

export function parseLog(text) {
  const cache = [];
  const failed = new Set();
  const slow = new Map();
  const suites = [];
  let tests = 0;
  let testMs = 0;
  let group = null;
  let pendingKey = null;
  let pendingTar = null;
  let pendingSize = null;
  let lastTap = null;

  const addTest = (name, status, ms) => {
    name = name.trim();
    if (!name || name.endsWith(':')) return;
    tests += 1;
    if (ms != null) testMs += ms;
    if (status === 'fail') failed.add(name);
    else if (ms != null && ms >= SLOW_TEST_MS) slow.set(name, Math.max(ms, slow.get(name) ?? 0));
  };

  for (const raw of text.split('\n')) {
    const line = raw.replace(STAMP, '').replace(ANSI, '').replace(/\r$/, '');
    let m;
    if ((m = /^##\[group\]Run (.+)$/.exec(line))) {
      group = m[1].trim();
      pendingKey = null;
      continue;
    }
    if (group && (m = /^ {2}key: (.+)$/.exec(line))) pendingKey = m[1].trim();

    // Caches: actions/cache and every setup action built on it, oven-sh/setup-bun, Swatinem/rust-cache.
    if ((m = /--use-compress-program (\S+)|(?:^|\s)-z(?:\s|$)|\.tzst|\.tgz/.exec(line)) && /\btar\b/.test(line)) {
      pendingTar = /zstd|\.tzst/.test(line) ? 'zstd' : 'gzip';
      const last = cache[cache.length - 1];
      if (last && last.step === group && last.outcome !== 'miss' && last.compression === null) {
        last.compression = pendingTar;
        pendingTar = null;
      }
    }
    if ((m = /^Cache Size: ~[\d.]+ MB \((\d+) B\)/.exec(line))) {
      pendingSize = Number(m[1]);
      const last = cache[cache.length - 1];
      if (last && last.step === group && last.outcome !== 'miss' && last.bytes === null) {
        last.bytes = pendingSize;
        pendingSize = null;
      }
    }
    if ((m = /^Cache restored from key: (.+)$/.exec(line))) {
      const key = m[1].trim();
      const exact = pendingKey ? key === pendingKey : null;
      cache.push(entry(group, exact === false ? 'partial' : 'hit', key, pendingSize, pendingTar));
      pendingTar = pendingSize = null;
      continue;
    }
    if ((m = /^Cache hit for(?: restore-key)?: (.+)$/.exec(line))) {
      cache.push(entry(group, /restore-key/.test(line) ? 'partial' : 'hit', m[1].trim(), pendingSize, pendingTar));
      pendingTar = pendingSize = null;
      continue;
    }
    if ((m = /^Cache not found for (?:input )?keys?: (.+)$/.exec(line)) || /^(?:Cache miss|No cache found\.?)/.test(line)) {
      cache.push(entry(group, 'miss', m ? m[1].trim() : null, null, null));
      continue;
    }
    if ((m = /^Restored from cache key "([^"]+)"(?: full match: (true|false))?/.exec(line))) {
      cache.push(entry(group, m[2] === 'false' ? 'partial' : 'hit', m[1], null, null));
      continue;
    }

    // Tests.
    if ((m = /^\s*\((pass|fail)\) (.+?)(?: \[([\d.]+)(ms|s)\])?$/.exec(line))) {
      addTest(m[2], m[1], ms(m[3], m[4]));
      continue;
    }
    if ((m = /^\s*([✔✖✓✕×✘]) (.+?)(?: \(([\d.]+) ?(ms|s|m)\))?$/.exec(line))) {
      const bad = /[✖✕×✘]/.test(m[1]);
      addTest(m[2].replace(/^\d+ /, ''), bad ? 'fail' : 'pass', ms(m[3], m[4]));
      continue;
    }
    if ((m = /^test (\S+) \.\.\. (ok|FAILED|ignored)/.exec(line))) {
      if (m[2] !== 'ignored') addTest(m[1], m[2] === 'ok' ? 'pass' : 'fail', null);
      continue;
    }
    if ((m = /^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed.*finished in ([\d.]+)s/.exec(line))) {
      suites.push({ passed: Number(m[1]), failed: Number(m[2]), ms: Math.round(Number(m[3]) * 1000), group });
      continue;
    }
    if ((m = /^\s*(PASS|FAIL) \[\s*([\d.]+)s\] (.+)$/.exec(line))) {
      addTest(m[3], m[1] === 'PASS' ? 'pass' : 'fail', Number(m[2]) * 1000);
      continue;
    }
    if ((m = /^\s*--- (PASS|FAIL): (\S+) \(([\d.]+)s\)/.exec(line))) {
      addTest(m[2], m[1] === 'PASS' ? 'pass' : 'fail', Number(m[3]) * 1000);
      continue;
    }
    if ((m = /^(FAILED|PASSED) (\S+::\S+)/.exec(line))) {
      addTest(m[2], m[1] === 'PASSED' ? 'pass' : 'fail', null);
      continue;
    }
    if ((m = /^\s*(not ok|ok) \d+ - (.+?)(?: # .*)?$/.exec(line))) {
      lastTap = { name: m[2], status: m[1] === 'ok' ? 'pass' : 'fail' };
      continue;
    }
    if (lastTap && (m = /^\s*duration_ms: ([\d.]+)/.exec(line))) {
      addTest(lastTap.name, lastTap.status, Number(m[1]));
      lastTap = null;
    }
  }
  return {
    cache,
    tests: {
      count: tests,
      ms: Math.round(testMs),
      failed: [...failed].slice(0, 50),
      slow: [...slow.entries()]
        .sort((a, b) => b[1] - a[1])
        .slice(0, 30)
        .map(([name, ms]) => ({ name, ms: Math.round(ms) })),
    },
    suites,
  };
}

function entry(group, outcome, key, bytes, compression) {
  return { step: group, outcome, key, bytes: bytes ?? null, compression: compression ?? null };
}

function ms(value, unit) {
  if (value == null) return null;
  const n = Number(value);
  if (unit === 's') return n * 1000;
  if (unit === 'm') return n * 60000;
  return n;
}
