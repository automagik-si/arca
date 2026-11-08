const list = (value) => Array.isArray(value) ? value : [];
const text = (value, fallback = '') => value == null ? fallback : String(value);
const count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
const byName = (a, b) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });
const INTERNAL_PROJECT = 'arcak-svc';

export function humanize(value) {
  const raw = text(value, 'Unnamed');
  return raw.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

export function projectLabel(value) {
  const id = text(value, 'Unnamed project');
  if (id === 'default') return 'Default';
  if (id === 'bench') return 'Bench';
  if (id.startsWith('arcak-adopt-')) return `Adopted ${humanize(id.slice('arcak-adopt-'.length))}`;
  return humanize(id);
}

export function instanceKindLabel(kind) {
  const value = text(kind).toLowerCase();
  if (value === 'virtual-machine' || value === 'vm') return 'VM';
  if (value === 'container') return 'Container';
  return humanize(value || 'Instance');
}

export function statusTone(status) {
  const value = text(status).trim().toLowerCase();
  if (['running', 'ready', 'active', 'started', 'ok', 'up', 'reachable', 'applied', 'committed', 'confirmed'].includes(value)) return 'ok';
  if (['stopped', 'exited', 'off', 'disabled', 'retained', 'reverted'].includes(value)) return 'neutral';
  if (['error', 'failed', 'unreachable', 'unavailable', 'down', 'missing', 'rejected'].includes(value)) return 'bad';
  return value ? 'warn' : 'unknown';
}

export function ownerLabel(owner) {
  const value = text(owner?.class ?? owner, 'unknown');
  return ['kernel', 'retained'].includes(value) ? 'Managed by Arca' : 'External';
}

function finite(value) {
  // null is a distinct fact from zero: a missing reading must stay missing.
  if (value === null || value === undefined || value === '') return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function clampPercent(value) {
  return Math.max(0, Math.min(100, value));
}

function formatNumber(value, digits = 1) {
  if (!Number.isFinite(value)) return '';
  return `${Math.round(value * 10 ** digits) / 10 ** digits}`;
}

export function formatGb(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '';
  const gb = value / 1e9;
  if (gb >= 10) return String(Math.round(gb));
  return String(Math.round(gb * 10) / 10);
}

export function isHostOnline(host = {}) {
  return Boolean(host.machine) && host.machine.incusReachable !== false;
}

export function hostCpu(host = {}) {
  const cpu = host.metrics?.latest?.cpu;
  const cores = finite(cpu?.cores);
  const usedPct = finite(cpu?.usedPct);
  const reason = typeof cpu?.reason === 'string' ? cpu.reason.trim() : '';
  if (cores === null && usedPct === null && !reason) return null;
  // A reason or a zero core count means the host totals are unknown. The load percentage cannot be trusted
  // against an unknown capacity, so it is reported as unknown and no bar is drawn.
  const known = !reason && cores !== null && cores > 0;
  const percent = known && usedPct !== null ? clampPercent(usedPct) : null;
  return {
    cores,
    usedPct: percent,
    usedCores: percent !== null && cores !== null ? (percent / 100) * cores : null,
    reason,
  };
}

export function hostMemory(host = {}) {
  const memory = host.metrics?.latest?.memory;
  const totalBytes = finite(memory?.totalBytes);
  const availableBytes = finite(memory?.availableBytes);
  const reportedUsed = finite(memory?.usedBytes);
  const reason = typeof memory?.reason === 'string' ? memory.reason.trim() : '';
  if (totalBytes === null && availableBytes === null && reportedUsed === null && !reason) return null;
  const derivedUsed = totalBytes !== null && availableBytes !== null ? Math.max(0, totalBytes - availableBytes) : null;
  // The host's own used figure wins when present; the total-available derivation is only a fallback.
  const usedBytes = reportedUsed ?? derivedUsed;
  return {
    totalBytes,
    availableBytes,
    usedBytes,
    usedPct: !reason && usedBytes !== null && totalBytes !== null && totalBytes > 0
      ? clampPercent((usedBytes / totalBytes) * 100) : null,
    reason,
  };
}

export function cpuLabel(cpu) {
  if (!cpu) return '';
  // A remote host may report no core count; a reason or a zero count is unknown capacity, never "0 cores".
  if (cpu.reason || cpu.cores === 0) return 'cores n/a';
  // A null load is unknown, never zero: no bar and no invented percentage.
  if (cpu.usedPct === null) return 'n/a';
  if (cpu.usedCores !== null && cpu.cores !== null) return `${formatNumber(cpu.usedCores)} / ${count(cpu.cores, 'core')}`;
  if (cpu.cores !== null) return count(cpu.cores, 'core');
  return `${formatNumber(cpu.usedPct)}% used`;
}

export function memoryLabel(memory) {
  if (!memory) return '';
  if (memory.reason || memory.totalBytes === null) return 'n/a';
  if (memory.usedBytes !== null) return `${formatGb(memory.usedBytes)} / ${formatGb(memory.totalBytes)} GB`;
  return `${formatGb(memory.totalBytes)} GB total`;
}

export function fleetSummary(hosts = []) {
  const rows = list(hosts);
  const parts = [count(rows.length, 'machine')];
  const cores = rows.reduce((sum, host) => sum + (hostCpu(host)?.cores ?? 0), 0);
  if (cores > 0) parts.push(count(cores, 'core'));
  const bytes = rows.reduce((sum, host) => sum + (hostMemory(host)?.totalBytes ?? 0), 0);
  if (bytes > 0) parts.push(`${formatGb(bytes)} GB`);
  return parts.join(' · ');
}

function normalizeHosts(envelope) {
  if (Array.isArray(envelope.hosts)) return envelope.hosts;
  return [{ ...envelope, name: text(envelope.nodeName, 'Machine') }];
}

function instanceKey(hostName, project, instance) {
  return `${text(hostName)}|${text(project, 'default')}|${text(instance)}`;
}

function instanceUrl(instance) {
  const candidates = [
    instance?.url, instance?.webUrl, instance?.endpoint,
    ...list(instance?.urls).map((value) => typeof value === 'string' ? value : value?.url),
  ];
  for (const candidate of candidates) {
    if (typeof candidate !== 'string' || !candidate) continue;
    try {
      const parsed = new URL(candidate);
      if ((parsed.protocol === 'http:' || parsed.protocol === 'https:') && !parsed.username && !parsed.password) return candidate;
    } catch { /* not a URL */ }
  }
  return '';
}

// The service ACL target is an implementation id (`arcak-svc-svc-<name>`). It is presented as a firewall on
// the service, and the internal project id never reaches the DOM.
function aclLabel(value) {
  return text(value)
    .replace(/\barcak-svc-svc-([a-z0-9-]+)\b/gi, (_, name) => `${humanize(name)} firewall`)
    .replace(/arcak-svc/gi, 'Arca service');
}

function targetLabel(target) {
  if (typeof target === 'string') return aclLabel(target);
  if (!target || typeof target !== 'object') return '';
  const kind = text(target.kind);
  const name = aclLabel(target.name);
  if (kind && name) return `${kind} ${name}`;
  if (name) return name;
  if (kind && target.project) {
    // The internal project is an implementation detail; never print its raw id.
    const project = text(target.project);
    return `${kind} in ${project === INTERNAL_PROJECT ? 'Arca service' : projectLabel(project)}`;
  }
  return kind;
}

function rollup(cards, unknown = 0) {
  if (!cards.length) return unknown > 0
    ? { tone: 'warn', text: 'Host data unavailable' }
    : { tone: 'neutral', text: 'Nothing live yet' };
  const tones = cards.map((card) => card.tone);
  const bad = tones.filter((tone) => tone === 'bad').length;
  const warn = tones.filter((tone) => tone === 'warn').length;
  const running = cards.filter((card) => card.status.toLowerCase() === 'running').length;
  const ready = cards.filter((card) => card.status.toLowerCase() === 'ready').length;
  const stopped = cards.filter((card) => card.status.toLowerCase() === 'stopped').length;
  const tone = bad ? 'bad' : warn || unknown ? 'warn' : tones.every((value) => value === 'neutral') ? 'neutral' : 'ok';
  const parts = [];
  if (running) parts.push(`${running} running`);
  if (ready) parts.push(`${ready} ready`);
  if (stopped) parts.push(`${stopped} stopped`);
  if (bad) parts.push(`${bad} unavailable`);
  else if (warn) parts.push(`${warn} unknown`);
  if (unknown) parts.push(`${unknown} unknown`);
  return { tone, text: parts.join(' · ') || 'Live' };
}

function buildMachine(host) {
  const cpu = hostCpu(host);
  const memory = hostMemory(host);
  const online = isHostOnline(host);
  return {
    name: text(host.name, text(host.nodeName, 'Machine')),
    label: text(host.name, text(host.nodeName, 'Machine')),
    address: text(host.address).trim(),
    online,
    status: online ? 'Online' : 'Offline',
    tone: online ? 'ok' : 'bad',
    cpu,
    cpuLabel: cpuLabel(cpu),
    cpuReason: cpu?.reason ?? '',
    memory,
    memoryLabel: memoryLabel(memory),
    memoryReason: memory?.reason ?? '',
    instanceCount: host.instancesKnown === false ? null : list(host.instances).length,
    instancesKnown: host.instancesKnown !== false,
    error: host.error ?? null,
    metricsAt: host.metrics?.latest?.at ?? null,
  };
}

// A service Open link is composed only from a real configured LAN address and a real service port. The
// container's bridge address and the service's published dash are never turned into a URL.
function serviceUrl(address, lanPort, state) {
  const host = text(address).trim();
  const port = finite(lanPort);
  // A proxy device on a stopped container is not a listener. Only a running service gets an Open link.
  if (text(state).toLowerCase() !== 'running') return '';
  if (!host || port === null || port < 1 || port > 65535) return '';
  return `http://${host.includes(':') ? `[${host}]` : host}:${Math.trunc(port)}`;
}

function serviceTargets(hosts) {
  const targets = new Map();
  for (const host of hosts) {
    for (const service of list(host.services)) {
      const name = text(service?.name).trim();
      if (!name) continue;
      targets.set(instanceKey(host.name, INTERNAL_PROJECT, name), {
        name,
        state: text(service.state),
        ipv4: text(service.ipv4),
        ports: list(service.ports).map(finite).filter((port) => port !== null),
        lanPort: finite(service.lanPort),
        published: service.published ?? null,
        url: serviceUrl(host.address, service.lanPort, service.state),
      });
    }
  }
  return targets;
}

function instanceCard(hostName, instance, services) {
  const status = text(instance.status, 'Unknown');
  const service = services?.get(instanceKey(hostName, instance.project, instance.name)) ?? null;
  return {
    kind: 'instance',
    key: instanceKey(hostName, instance.project, instance.name),
    host: hostName,
    incusProject: text(instance.project, 'default'),
    name: text(instance.name, 'Unnamed instance'),
    status,
    tone: statusTone(status),
    type: instanceKindLabel(instance.kind),
    owner: ownerLabel(instance.owner),
    privileged: Boolean(instance.privileged),
    service,
    url: instanceUrl(instance) || service?.url || '',
  };
}

function databaseCard(hostName, instance, database) {
  const status = text(database.status, 'Unknown');
  return {
    kind: 'database',
    key: `${instanceKey(hostName, instance.project, instance.name)}|db:${text(database.name ?? database.entry, 'db')}`,
    host: hostName,
    incusProject: text(instance.project, 'default'),
    instance: text(instance.name),
    name: text(database.name ?? database.entry, 'Database'),
    status,
    tone: statusTone(status),
    type: 'PostgreSQL',
    address: text(database.address ?? database.host),
    port: finite(database.port),
    links: list(database.links),
    url: instanceUrl(database),
  };
}

function databaseTargets(hosts) {
  const targets = new Map();
  for (const host of hosts) {
    const instances = list(host.instances);
    for (const database of list(host.databases)) {
      const explicit = text(database.instance ?? database.container ?? database.droid);
      let matches = explicit ? instances.filter((instance) => text(instance.name) === explicit) : [];
      if (!matches.length) {
        matches = instances.filter((instance) => text(instance.name) === text(database.name) || text(instance.name) === text(database.entry));
      }
      if (matches.length !== 1) continue;
      targets.set(instanceKey(host.name, matches[0].project, matches[0].name), { host, instance: matches[0], database });
    }
  }
  return targets;
}

function uniqueName(base, used, hostName) {
  if (!used.has(base)) return base;
  const withHost = `${base} (${hostName})`;
  if (!used.has(withHost)) return withHost;
  let index = 2;
  while (used.has(`${withHost} ${index}`)) index++;
  return `${withHost} ${index}`;
}

export function buildDashboard(envelope = {}) {
  const hosts = normalizeHosts(envelope);
  const machines = hosts.map(buildMachine);
  const usedNames = new Set();
  const claimed = new Set();
  const ownerProjects = [];
  const serviceProjects = [];
  const fallbackProjects = [];
  const databaseCards = databaseTargets(hosts);
  const serviceCards = serviceTargets(hosts);

  for (const config of list(envelope.projects)) {
    const name = text(config.name).trim();
    if (!name || usedNames.has(name)) continue;
    const cards = [];
    let missing = 0;
    let unknown = 0;
    for (const member of list(config.members)) {
      const host = hosts.find((candidate) => candidate.name === member.host);
      const instance = list(host?.instances).find((candidate) =>
        text(candidate.project, 'default') === text(member.incusProject) && text(candidate.name) === text(member.instance));
      if (!instance) {
        if (host && host.instancesKnown === false) unknown++;
        else missing++;
        continue;
      }
      const card = instanceCard(host.name, instance, serviceCards);
      cards.push(card);
      claimed.add(card.key);
    }
    for (const card of [...cards]) {
      const target = databaseCards.get(card.key);
      if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
    }
    usedNames.add(name);
    const roll = rollup(cards, unknown);
    ownerProjects.push({
      id: `owner:${name}`,
      kind: 'owner',
      name,
      label: name,
      rawName: '',
      host: null,
      domain: text(config.domain).trim(),
      cards,
      hosts: [...new Set(cards.map((card) => card.host))],
      status: roll.text,
      tone: roll.tone,
      missing,
      unknown,
    });
  }

  // arcak-svc is Arca's own project, never a project of its own. Each service the
  // owner has not claimed becomes a first-class project named after the instance.
  for (const host of hosts) {
    const unlisted = list(host.instances)
      .filter((instance) => text(instance.project, 'default') === INTERNAL_PROJECT
        && !claimed.has(instanceKey(host.name, instance.project, instance.name)))
      .sort((a, b) => byName(humanize(text(a.name)), humanize(text(b.name))));
    for (const instance of unlisted) {
      const cards = [instanceCard(host.name, instance, serviceCards)];
      const target = databaseCards.get(cards[0].key);
      if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
      const name = uniqueName(humanize(text(instance.name)), usedNames, host.name);
      usedNames.add(name);
      const roll = rollup(cards);
      serviceProjects.push({
        id: `service:${text(host.name)}/${text(instance.name)}`,
        kind: 'service',
        name,
        label: name,
        rawName: '',
        host: text(host.name),
        domain: '',
        cards,
        hosts: [text(host.name)],
        status: roll.text,
        tone: roll.tone,
        missing: 0,
        unknown: 0,
      });
    }
  }

  for (const host of hosts) {
    const projectIds = new Set(list(host.projects).map((value) => text(value)).filter(Boolean));
    for (const instance of list(host.instances)) projectIds.add(text(instance.project, 'default'));
    for (const project of [...projectIds].sort(byName)) {
      if (project === INTERNAL_PROJECT) continue;
      const unlisted = list(host.instances).filter((instance) =>
        text(instance.project, 'default') === project && !claimed.has(instanceKey(host.name, instance.project, instance.name)));
      if (!unlisted.length) continue;
      const cards = unlisted.sort((a, b) => byName(text(a.name), text(b.name))).map((instance) => instanceCard(host.name, instance, serviceCards));
      for (const card of [...cards]) {
        const target = databaseCards.get(card.key);
        if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
      }
      const base = projectLabel(project);
      const name = uniqueName(base, usedNames, host.name);
      usedNames.add(name);
      const roll = rollup(cards);
      fallbackProjects.push({
        id: `incus:${text(host.name)}/${project}`,
        kind: 'incus',
        name,
        label: name,
        rawName: project,
        host: text(host.name),
        domain: '',
        cards,
        hosts: [text(host.name)],
        status: roll.text,
        tone: roll.tone,
        missing: 0,
        unknown: 0,
      });
    }
  }

  const projects = [...ownerProjects, ...serviceProjects, ...fallbackProjects];
  const configuredDefault = text(envelope.defaultProject).trim();
  const defaultProject = projects.find((project) => project.name === configuredDefault)
    ?? projects.find((project) => project.kind === 'owner')
    ?? projects.find((project) => project.kind === 'service')
    ?? projects.find((project) => project.kind === 'incus')
    ?? null;

  return {
    hosts: machines,
    projects,
    defaultProject,
    summary: fleetSummary(hosts),
    plans: hosts.reduce((sum, host) => sum + list(host.plans).length, 0),
    journal: journalFeed(hosts),
  };
}

export function journalFeed(hosts = [], limit = 8) {
  const records = [];
  for (const host of list(hosts)) {
    for (const [index, record] of list(host.journal).entries()) {
      if (!record || typeof record !== 'object') continue;
      records.push({
        key: `${text(host.name)}:${record.id ?? index}`,
        host: text(host.name, 'Machine'),
        entry: text(record.entry, 'activity'),
        state: text(record.state, 'Unknown'),
        tone: statusTone(record.state),
        target: targetLabel(record.target),
        plugin: text(record.plugin),
        at: finite(record.at_ms),
      });
    }
  }
  records.sort((a, b) => (b.at ?? 0) - (a.at ?? 0));
  return records.slice(0, limit);
}

export function projectHash(project) {
  return project ? `#project/${encodeURIComponent(project.name)}` : '';
}

export function parseProjectHash(hash) {
  const match = /^#project\/(.+)$/.exec(text(hash));
  if (!match) return null;
  try { return decodeURIComponent(match[1]); } catch { return null; }
}

export function selectProject(projects = [], name) {
  const rows = list(projects);
  return rows.find((project) => project.name === name) ?? null;
}

export function relativeAge(at, now = Date.now()) {
  if (!Number.isFinite(at)) return '';
  const elapsed = Math.max(0, now - at);
  if (elapsed < 60_000) return `${Math.floor(elapsed / 1000)}s`;
  if (elapsed < 3_600_000) return `${Math.floor(elapsed / 60_000)}m`;
  if (elapsed < 86_400_000) return `${Math.floor(elapsed / 3_600_000)}h`;
  return `${Math.floor(elapsed / 86_400_000)}d`;
}
