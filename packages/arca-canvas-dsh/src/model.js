const list = (value) => Array.isArray(value) ? value : [];
const text = (value, fallback = '') => value == null ? fallback : String(value);
const count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
const byName = (a, b) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });
const INTERNAL_PROJECT = 'arcak-svc';

// The kernel names an action proposal `act-` plus the first twelve hex digits of its arguments digest.
export function proposalId(value) {
  const id = value && typeof value === 'object' ? text(value.id) : '';
  return /^act-[0-9a-f]{12}$/.test(id) ? id : '';
}

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

// Memory and VRAM are binary: a 16 GiB card reads "16 GB", not 17.2. Disks keep decimal gigabytes
// because that is the number printed on a drive. Both helpers keep the visible unit "GB".
const MEMORY_GIGABYTE = 1024 ** 3;
const DISK_GIGABYTE = 1e9;

function formatGigabyteValue(bytes, base) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '';
  const gb = value / base;
  if (gb >= 10) return String(Math.round(gb));
  return String(Math.round(gb * 10) / 10);
}

export function formatGb(bytes) {
  return formatGigabyteValue(bytes, MEMORY_GIGABYTE);
}

export function formatDiskGb(bytes) {
  return formatGigabyteValue(bytes, DISK_GIGABYTE);
}

// A watch-only host refuses service reads because it is not allowed to write. That is the expected
// state of the machine, so the services area explains it instead of surfacing the refusal as an error.
export const WATCH_ONLY_SERVICES = "Arca watches this machine; it doesn't run services here yet";

export function formatWatts(value) {
  const number = finite(value);
  return number === null || number < 0 ? '' : `${formatNumber(number, 1)} W`;
}

export function formatCelsius(value) {
  const number = finite(value);
  return number === null ? '' : `${formatNumber(number, 1)} °C`;
}

export function formatMhz(value) {
  const number = finite(value);
  return number === null || number < 0 ? '' : `${Math.round(number)} MHz`;
}

// nvidia-smi reports clocks_event_reasons.active as a bit mask. Bit 0x4 is SW Power Cap; no other
// bit may show that sentence. The mask is a string in the kernel sample, so parse it as a big integer.
export function gpuPowerCapped(value) {
  if (value === null || value === undefined || value === '') return false;
  let mask;
  try {
    if (typeof value === 'number') {
      if (!Number.isFinite(value) || value < 0) return false;
      mask = BigInt(Math.trunc(value));
    } else {
      const raw = String(value).trim().toLowerCase();
      if (!/^(?:0x[0-9a-f]+|[0-9]+)$/.test(raw)) return false;
      mask = BigInt(raw);
    }
  } catch {
    return false;
  }
  return (mask & 4n) === 4n;
}

function gpuProductName(product) {
  let value = text(product).trim();
  // The PCI database often names a chip first and the market name in brackets: keep the market name.
  const bracket = value.match(/\[([^\]]+)\]/);
  if (bracket) value = bracket[1];
  value = value
    .replace(/\b(?:NVIDIA|AMD|ATI|Intel|Corporation)\b/gi, ' ')
    .replace(/\b(?:GeForce|Radeon|Graphics)\b/gi, ' ')
    .replace(/\b(?:RTX|GTX|RX|Arc)\b/gi, ' ')
    .replace(/\b\d+\s*GB\b/gi, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  // A product that lists several models ("880M / 890M") keeps the first as the short card name.
  value = value.split(/\s*\/\s*/)[0].trim();
  return value;
}

function gpuCardReason(card, gpu) {
  return text(card?.reason).trim() || text(gpu?.reason).trim();
}

function containerLabel(value) {
  const id = text(value).trim();
  if (!id) return '';
  // The kernel names a host Docker container by its full 64-hex cgroup id. Twelve digits are enough
  // to tell containers apart in the canvas; the full id stays on the element's title attribute.
  return /^[0-9a-f]{64}$/i.test(id) ? id.slice(0, 12) : id;
}

function gpuOwnerRows(processes) {
  const groups = new Map();
  for (const process of list(processes)) {
    if (!process || typeof process !== 'object') continue;
    const owner = process.owner && typeof process.owner === 'object' ? process.owner : {};
    const project = text(owner.project).trim();
    const name = text(owner.name).trim();
    const container = text(owner.container).trim();
    const kind = text(owner.kind).trim().toLowerCase();
    let row;
    if (project || name) {
      const where = [project ? projectLabel(project) : '', name].filter(Boolean).join(' / ');
      row = {
        kind: 'instance', project, name, container,
        label: where || 'an instance',
        detail: container ? `Docker ${containerLabel(container)}` : '',
      };
    } else if (container || kind === 'docker' || kind === 'container') {
      row = {
        kind: 'docker', project: '', name: '', container,
        label: container ? `Docker container ${containerLabel(container)}` : 'a Docker container',
        detail: '',
      };
    } else {
      row = { kind: 'host', project: '', name: '', container: '', label: 'on the host', detail: '' };
    }
    const key = `${row.kind}|${project}|${name}|${container}`;
    let group = groups.get(key);
    if (!group) {
      group = { ...row, key, vramBytes: null, processes: 0 };
      groups.set(key, group);
    }
    group.processes += 1;
    const bytes = finite(process.vramBytes);
    if (bytes !== null && bytes >= 0) group.vramBytes = (group.vramBytes ?? 0) + bytes;
  }
  return [...groups.values()]
    .sort((a, b) => {
      if (a.vramBytes === null && b.vramBytes !== null) return 1;
      if (a.vramBytes !== null && b.vramBytes === null) return -1;
      if (a.vramBytes !== b.vramBytes) return (b.vramBytes ?? 0) - (a.vramBytes ?? 0);
      return byName(a.label, b.label);
    })
    .map((owner) => ({ ...owner, vramText: owner.vramBytes === null ? '' : `${formatGb(owner.vramBytes)} GB` }));
}

// The kernel keys cards by PCI address, so two identical products stay two rows. Cards are sorted by
// PCI and every card gets its own slot number, whether or not products repeat.
export function gpuCards(gpu) {
  const cards = list(gpu?.cards)
    .filter((card) => card && typeof card === 'object' && text(card.pci).trim())
    .slice()
    .sort((a, b) => text(a.pci).trim().localeCompare(text(b.pci).trim(), 'en'));
  return cards.map((card, index) => {
    const pci = text(card.pci).trim();
    const product = text(card.product).trim();
    const shortName = gpuProductName(product);
    const slot = index + 1;
    const label = `GPU ${slot}${shortName ? ` · ${shortName}` : ''}`;
    const telemetry = card.telemetry && typeof card.telemetry === 'object' ? card.telemetry : null;
    const utilizationPct = finite(telemetry?.utilizationPct);
    const usedBytes = finite(telemetry?.memoryUsedBytes);
    const totalBytes = finite(telemetry?.memoryTotalBytes) ?? finite(card.vramTotalBytes);
    const vramPercent = usedBytes !== null && totalBytes !== null && totalBytes > 0
      ? Math.max(0, Math.min(100, (usedBytes / totalBytes) * 100)) : null;
    let vramText = '';
    if (usedBytes !== null && totalBytes !== null) vramText = `${formatGb(usedBytes)} / ${formatGb(totalBytes)} GB`;
    else if (usedBytes !== null) vramText = `${formatGb(usedBytes)} GB used`;
    else if (totalBytes !== null) vramText = `${formatGb(totalBytes)} GB`;
    const hasReading = Boolean(telemetry)
      && (utilizationPct !== null || usedBytes !== null || totalBytes !== null);
    const reason = gpuCardReason(card, gpu);
    return {
      pci,
      slot,
      slotLabel: `GPU ${slot}`,
      shortName,
      label,
      product,
      vendor: text(card.vendor).trim(),
      uuid: text(card.uuid).trim(),
      driver: text(card.driver).trim(),
      driverVersion: text(card.driverVersion).trim(),
      use: text(card.use).trim(),
      numaNode: finite(card.numaNode),
      telemetry,
      hasReading,
      utilizationPct: utilizationPct === null ? null : Math.max(0, Math.min(100, utilizationPct)),
      utilizationText: utilizationPct === null ? '' : `${Math.round(Math.max(0, Math.min(100, utilizationPct)))}%`,
      usedBytes,
      totalBytes,
      vramText,
      vramPercent,
      reason,
      // With no usable telemetry the plain kernel reason is the whole reading, never a zero.
      statusText: hasReading ? '' : (reason || 'No reading yet'),
      powerDrawW: finite(telemetry?.powerDrawW),
      powerLimitW: finite(telemetry?.powerLimitW),
      temperatureC: finite(telemetry?.temperatureC),
      smClockMhz: finite(telemetry?.smClockMhz),
      eventReasons: telemetry?.eventReasons ?? null,
      powerCapped: gpuPowerCapped(telemetry?.eventReasons),
      processReason: text(card.processReason).trim(),
      owners: gpuOwnerRows(card.processes),
      title: product ? `${product} · ${pci}` : pci,
    };
  });
}

export function isHostOnline(host = {}) {
  return Boolean(host.machine) && host.machine.incusReachable !== false;
}

// The kernel's coresKnown flag is authoritative. Older snapshots have no flag, so only a positive count is
// real there; zero has always been the placeholder for an unread host total. The machine card and the fleet
// total must apply this one rule, or a mixed fleet counts a failed read as capacity.
export function cpuCoresKnown(cpu) {
  const cores = finite(cpu?.cores);
  return cores !== null && cores > 0
    && (typeof cpu?.coresKnown === 'boolean' ? cpu.coresKnown : true);
}

// The same rule as a count: zero when the card must say "cores n/a" and the fleet total must skip this host.
export function cpuCoreCount(cpu) {
  return cpuCoresKnown(cpu) ? finite(cpu.cores) : 0;
}

export function hostCpu(host = {}) {
  const cpu = host.metrics?.latest?.cpu;
  const cores = finite(cpu?.cores);
  const coresKnown = cpuCoresKnown(cpu);
  const usedPct = finite(cpu?.usedPct);
  const reason = typeof cpu?.reason === 'string' ? cpu.reason.trim() : '';
  if (cores === null && usedPct === null && !reason) return null;
  // The load percentage cannot be trusted against an unknown capacity, so it is reported as unknown and no bar
  // is drawn. A reason only explains an absent load; it never invalidates a real core count.
  const percent = coresKnown && usedPct !== null ? clampPercent(usedPct) : null;
  return {
    cores,
    coresKnown,
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
  // Trust the shared cores rule; an older snapshot without the flag is handled there too.
  // A reason describes an absent load, not an absent capacity, so it never turns a known count into "cores n/a".
  const cores = cpuCoreCount(cpu);
  if (!cores) return 'cores n/a';
  // A null load is unknown, never zero: keep the real capacity and invent no percentage.
  const usedPct = finite(cpu.usedPct);
  if (usedPct === null) return `n/a / ${count(cores, 'core')}`;
  const usedCores = finite(cpu.usedCores);
  if (usedCores !== null) return `${formatNumber(usedCores)} / ${count(cores, 'core')}`;
  return `${formatNumber(usedPct)}% used`;
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
  const cores = rows.reduce((sum, host) => sum + cpuCoreCount(hostCpu(host)), 0);
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

// A snapshot error is the joined list of failed reads ("service_list: ... node_metrics: ..."). The machine
// card needs to know which read failed, so one expected watch-only service refusal does not hide a real
// partial read elsewhere on the same machine.
function readErrorTools(error) {
  const value = text(error).trim();
  if (!value) return [];
  const pattern = /(?:^|\s)(arca_status|watch_report|postgres_list|node_metrics|alerts_list|plan_list|service_list|journal_list):\s/g;
  const tools = [];
  let match;
  while ((match = pattern.exec(value)) !== null) tools.push(match[1]);
  return [...new Set(tools)];
}

function buildMachine(host) {
  const cpu = hostCpu(host);
  const memory = hostMemory(host);
  const online = isHostOnline(host);
  const mode = text(host.machine?.mode).trim();
  const gpu = host.metrics?.latest?.gpu;
  const errorTools = readErrorTools(host.error);
  return {
    name: text(host.name, text(host.nodeName, 'Machine')),
    label: text(host.name, text(host.nodeName, 'Machine')),
    address: text(host.address).trim(),
    online,
    status: online ? 'Online' : 'Offline',
    tone: online ? 'ok' : 'bad',
    mode,
    // The kernel says watch-only when it has no project it may write in. It is a mode of the machine,
    // not an error, and the services area keys off this value rather than any service refusal text.
    watchOnly: mode.toLowerCase() === 'watch-only',
    cpu,
    cpuLabel: cpuLabel(cpu),
    // The reason is the CPU n/a tooltip: it only applies while no load is shown.
    cpuReason: cpu?.usedPct === null ? (cpu?.reason ?? '') : '',
    memory,
    memoryLabel: memoryLabel(memory),
    memoryReason: memory?.reason ?? '',
    gpus: gpuCards(gpu),
    gpuReason: text(gpu?.reason).trim(),
    gpuSkipped: text(gpu?.skipped).trim(),
    services: list(host.services)
      .map((service) => ({
        name: text(service?.name).trim(),
        state: text(service?.state).trim(),
        lanPort: finite(service?.lanPort),
        versionDiffers: versionDiffers(service),
        // A program the node only records (external_add) says its kind and that Arca does not manage it. Every
        // other row carries neither, so it is the row it always was.
        kind: service?.recorded === true ? text(service.kind).trim() : '',
        recorded: service?.recorded === true,
      }))
      .filter((service) => service.name),
    // Older snapshots have no flag: treat their empty list as known-empty, as before.
    servicesKnown: host.servicesKnown !== false,
    errorTools,
    servicesOnlyError: errorTools.length > 0 && errorTools.every((tool) => tool === 'service_list'),
    instanceCount: host.instancesKnown === false ? null : list(host.instances).length,
    instancesKnown: host.instancesKnown !== false,
    error: host.error ?? null,
    metricsAt: host.metrics?.latest?.at ?? null,
  };
}

// service_list marks a service whose installed template names another version than the one it runs with
// update.state "differs", and no other service. Two digests carry no order, so this never means "newer".
function versionDiffers(service) {
  return service?.update?.state === 'differs';
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
        versionDiffers: versionDiffers(service),
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
