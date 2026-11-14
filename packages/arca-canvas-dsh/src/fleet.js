// The Fleet updates view: one row per configured host with its Arca OS version, update channel, check frequency, automatic
// updates, last successful check, last update and last error kind, and at most one warning. Everything here is read from
// each host's `os_update` answer (snapshot.osUpdate); nothing is decided or changed on a node.

const HOUR = 3_600_000;
const MINUTE = 60_000;
const DAY = 86_400_000;

// The OS classifies a failed check; these are only its words for the owner.
export const ERROR_KIND_TEXT = {
  network: 'could not reach the update server',
  parse: 'the update data was damaged',
  signature: "the server's signature was invalid",
  trust: "the server's key is not trusted",
  other: 'the last check failed',
};

export const FLEET_TEXT = {
  trust: 'Its update key is not trusted, so it will not update until this is fixed.',
  unknown: 'This node does not report update state.',
  failed: 'Could not read the update state just now.',
  restart: 'An update is ready and waiting for a restart.',
  automaticOff: 'Automatic updates are off on this node.',
  // The update server's newest version for a channel is not in the update document yet (a later OS field such as
  // `available_version`), so a node is compared with the newest version its own fleet reports on the same channel.
  behindDetail: 'Compared with the newest version on your fleet.',
};

const text = (value) => (typeof value === 'string' && value.trim() ? value.trim() : null);
const finite = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : null);

// Go's duration text as incus-osd writes `check_frequency` ("6h", "30m", "1h30m0s"). `null` for "never" or anything else.
export function frequencyMs(value) {
  const raw = text(value);
  if (!raw || raw === 'never' || !/^(\d+(\.\d+)?(h|m|s|ms))+$/.test(raw)) return null;
  let total = 0;
  for (const [, amount, , unit] of raw.matchAll(/(\d+(\.\d+)?)(ms|h|m|s)/g)) {
    total += Number(amount) * { h: HOUR, m: MINUTE, s: 1000, ms: 1 }[unit];
  }
  return total > 0 ? total : null;
}

export function plainAge(at, now) {
  if (finite(at) === null) return '';
  const elapsed = Math.max(0, now - at);
  if (elapsed < MINUTE) return 'just now';
  const unit = (amount, name) => `${amount} ${name}${amount === 1 ? '' : 's'} ago`;
  if (elapsed < HOUR) return `${Math.floor(elapsed / MINUTE)} min ago`;
  if (elapsed < DAY) return unit(Math.floor(elapsed / HOUR), 'hour');
  return unit(Math.floor(elapsed / DAY), 'day');
}

function defaultDate(at) {
  try {
    return new Intl.DateTimeFormat(undefined, {
      day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
    }).format(new Date(at));
  } catch {
    return new Date(at).toISOString().slice(0, 16).replace('T', ' ');
  }
}

// Arca OS versions are date stamps ("202611010000"); a numeric-aware compare also orders dotted versions ("5.10" > "5.9").
export function compareVersions(a, b) {
  return String(a).localeCompare(String(b), 'en', { numeric: true, sensitivity: 'base' });
}

// How a host's update read went: `ok` with an answer, `missing` when the kernel has no `os_update` (an older kernel, or a
// snapshot without the field), `failed` when the read did not complete just now.
export function updateRead(host = {}) {
  if (host.osUpdate && typeof host.osUpdate === 'object') return 'ok';
  if (host.osUpdateRead === 'failed') return 'failed';
  return 'missing';
}

function newestByChannel(hosts) {
  const newest = new Map();
  for (const host of hosts) {
    if (updateRead(host) !== 'ok' || host.osUpdate.known !== true) continue;
    const channel = text(host.osUpdate.channel);
    const version = text(host.osUpdate.version);
    if (!channel || !version) continue;
    const current = newest.get(channel);
    if (!current || compareVersions(version, current) > 0) newest.set(channel, version);
  }
  return newest;
}

function trustDetail(trust) {
  if (!trust || typeof trust !== 'object') return '';
  const parts = [];
  const embedded = text(trust.embeddedCaFingerprint);
  const presented = text(trust.presentedCaFingerprint);
  if (embedded) parts.push(`Expected key ${embedded}.`);
  if (presented) parts.push(`The server presented ${presented}.`);
  const failures = finite(trust.failures);
  if (failures !== null && failures > 1) parts.push(`${failures} checks in a row.`);
  return parts.join(' ');
}

function blankRow(name, read, message) {
  return {
    name, read, known: false,
    version: '—', channel: '—', automatic: '—', lastCheck: '—', lastCheckStale: false, lastUpdate: '—', lastError: '—',
    warning: { kind: read === 'failed' ? 'failed' : 'unknown', tone: 'neutral', text: message, detail: '' },
    asOf: null,
  };
}

function buildRow(host, newest, now, formatDate) {
  const name = text(host.name) ?? 'Machine';
  const read = updateRead(host);
  if (read === 'failed') return blankRow(name, read, FLEET_TEXT.failed);
  if (read === 'missing') return blankRow(name, read, FLEET_TEXT.unknown);
  const state = host.osUpdate;
  const asOf = finite(state.asOf);
  if (state.known !== true) {
    return { ...blankRow(name, read, FLEET_TEXT.unknown), version: 'not an Arca OS node', asOf };
  }
  const version = text(state.version);
  const channel = text(state.channel);
  const frequency = text(state.checkFrequency);
  const automatic = typeof state.automatic === 'boolean' ? state.automatic : null;
  const lastSuccessAt = finite(state.lastSuccessAt);
  const every = frequencyMs(frequency);
  const staleAfter = every === null ? null : Math.max(2 * every, HOUR);
  const stale = staleAfter !== null && lastSuccessAt !== null && now - lastSuccessAt > staleAfter;
  const updatedVersion = text(state.lastUpdatedVersion);
  const updatedAt = finite(state.lastUpdatedAt);
  const errorKind = text(state.lastErrorKind);
  const trust = state.trust && typeof state.trust === 'object' ? state.trust : null;

  const row = {
    name, read, known: true,
    version: version ?? '—',
    channel: [channel ?? '—', frequency ? (frequency === 'never' ? 'never' : `every ${frequency}`) : null]
      .filter(Boolean).join(' · '),
    automatic: automatic === null ? '—' : automatic ? 'On' : 'Off',
    lastCheck: lastSuccessAt !== null ? plainAge(lastSuccessAt, now) : 'never',
    lastCheckStale: stale,
    lastUpdate: updatedVersion
      ? [updatedVersion, updatedAt !== null ? plainAge(updatedAt, now) : null].filter(Boolean).join(' · ')
      : 'none seen yet',
    lastError: errorKind && ERROR_KIND_TEXT[errorKind] ? ERROR_KIND_TEXT[errorKind] : '—',
    warning: null,
    asOf,
  };

  // At most one warning, in the owner's priority. A warning never stops a node: it keeps running and retrying.
  const head = channel ? newest.get(channel) : null;
  if (errorKind === 'trust' || (finite(trust?.failures) ?? 0) > 0) {
    row.warning = { kind: 'trust', tone: 'bad', text: FLEET_TEXT.trust, detail: trustDetail(trust) };
  } else if (state.needsReboot === true || text(state.pendingVersion)) {
    const pending = text(state.pendingVersion);
    row.warning = { kind: 'restart', tone: 'warn', text: FLEET_TEXT.restart, detail: pending ? `Version ${pending} is waiting.` : '' };
  } else if (version && head && compareVersions(version, head) < 0) {
    row.warning = {
      kind: 'behind', tone: 'warn',
      text: `Behind the fleet's newest version (${head}) on ${channel}.`,
      detail: FLEET_TEXT.behindDetail,
    };
  } else if (automatic === false) {
    row.warning = { kind: 'automatic-off', tone: 'warn', text: FLEET_TEXT.automaticOff, detail: '' };
  } else if (stale) {
    row.warning = { kind: 'stale', tone: 'warn', text: `Has not checked since ${formatDate(lastSuccessAt)}.`, detail: '' };
  }
  return row;
}

// The Fleet view's model: one row per host in config order, the newest read time among them and how many report.
export function buildFleet(hosts = [], { now = Date.now(), formatDate = defaultDate } = {}) {
  const list = Array.isArray(hosts) ? hosts : [];
  const newest = newestByChannel(list);
  const rows = list.map((host) => buildRow(host ?? {}, newest, now, formatDate));
  const reported = rows.filter((row) => row.known).length;
  const times = rows.map((row) => row.asOf).filter((at) => at !== null);
  return {
    rows,
    reported,
    total: rows.length,
    asOf: times.length ? Math.max(...times) : null,
    count: `${reported} of ${rows.length} node${rows.length === 1 ? '' : 's'} report update state`,
  };
}
