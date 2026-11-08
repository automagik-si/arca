const list = (value) => Array.isArray(value) ? value : [];
const text = (value, fallback = '') => value == null ? fallback : String(value);
const count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
const byName = (a, b) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });

export function humanize(value) {
  const raw = text(value, 'Unnamed');
  return raw.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

export function projectLabel(value) {
  const id = text(value, 'Unnamed project');
  if (id === 'default') return 'Default';
  if (id === 'bench') return 'Bench';
  if (id === 'arcak-svc') return 'Arca services';
  if (id.startsWith('arcak-adopt-')) return `Adopted ${humanize(id.slice('arcak-adopt-'.length))}`;
  return humanize(id);
}

export function deviceLabel(kind) {
  return kind === 'desktop' ? 'Desktop' : 'Phone';
}

export function kindLabel(kind) {
  const value = text(kind).toLowerCase();
  if (value === 'virtual-machine' || value === 'vm') return 'VM';
  if (value === 'container') return 'Container';
  return humanize(value || 'Instance');
}

export function statusTone(status) {
  const value = text(status).trim().toLowerCase();
  if (['running', 'ready', 'active', 'started', 'ok', 'up', 'reachable'].includes(value)) return 'ok';
  if (['stopped', 'exited', 'off', 'disabled'].includes(value)) return 'neutral';
  if (['error', 'failed', 'unreachable', 'unavailable', 'down', 'missing'].includes(value)) return 'bad';
  return value ? 'warn' : 'unknown';
}

export function statusText(status, fallback = 'Unknown') {
  return status ? text(status) : fallback;
}

export function alertRows(alerts) {
  if (Array.isArray(alerts)) return alerts.filter((row) => row.state === 'firing');
  return list(alerts?.firing).map((row) => ({ ...row, state: 'firing' }));
}

function instanceKey(host, project, name) {
  return `${host.node}|${text(project, '')}|${text(name, '')}`;
}

function shortLine(instance) {
  const image = instance.image?.alias ?? instance.image?.fingerprint ?? instance.image;
  const address = instance.address ?? instance.ipv4 ?? instance.ip;
  if (image) return `Image ${text(image)}`;
  if (address) return `IP ${text(address)}`;
  return null;
}

function rollup(children, emptyText) {
  if (!children.length) return { tone: 'neutral', text: emptyText };
  const tones = children.map((child) => child.tone);
  const bad = tones.filter((tone) => tone === 'bad').length;
  const warn = tones.filter((tone) => tone === 'warn').length;
  const running = children.filter((child) => child.rawStatus?.toLowerCase() === 'running').length;
  const stopped = children.filter((child) => child.rawStatus?.toLowerCase() === 'stopped').length;
  const tone = bad ? 'bad' : warn ? 'warn' : tones.every((tone) => tone === 'neutral') ? 'neutral' : 'ok';
  const parts = [`${running} running`];
  if (stopped) parts.push(`${stopped} stopped`);
  if (bad) parts.push(`${bad} unavailable`);
  else if (warn) parts.push(`${warn} unknown`);
  return { tone, text: parts.join(' · ') };
}

export function buildTree(snapshot = {}) {
  const machine = snapshot.machine ?? null;
  const hostId = text(machine?.node, 'host');
  const hostName = text(snapshot.nodeName, 'Machine').trim() || 'Machine';
  const instances = list(snapshot.instances).filter((row) => row && row.name);
  const projectIds = new Set(list(snapshot.projects).map((value) => text(value)).filter(Boolean));
  for (const instance of instances) projectIds.add(text(instance.project, 'default'));
  const projects = [...projectIds].sort(byName);
  const instancesByName = new Map();
  for (const instance of instances) {
    const name = text(instance.name);
    if (!instancesByName.has(name)) instancesByName.set(name, []);
    instancesByName.get(name).push(instance);
  }
  const devices = [
    ...list(snapshot.phones).map((row) => ({ ...row, kind: 'phone' })),
    ...list(snapshot.desktops).map((row) => ({ ...row, kind: 'desktop' })),
  ];
  const placed = new Map();
  const unplaced = [];
  for (const device of devices) {
    const matches = instancesByName.get(text(device.name)) ?? [];
    if (matches.length !== 1) {
      unplaced.push({ ...device, reason: matches.length ? 'name matches more than one instance' : 'no instance with this name' });
      continue;
    }
    const key = instanceKey(machine ?? {}, matches[0].project, matches[0].name);
    const matched = { ...device, matched: { project: matches[0].project, name: matches[0].name } };
    if (!placed.has(key)) placed.set(key, []);
    placed.get(key).push(matched);
  }
  const databases = list(snapshot.databases).map((row) => ({ ...row, kind: 'database' }));
  const databasesByInstance = new Map();
  const unplacedDatabases = [];
  for (const database of databases) {
    const explicit = text(database.instance ?? database.droid ?? database.container);
    let matches = explicit ? instances.filter((row) => text(row.name) === explicit) : [];
    if (!matches.length) matches = instances.filter((row) => text(row.name) === text(database.name) || text(row.name) === text(database.entry));
    if (matches.length === 1) {
      const key = instanceKey(machine ?? {}, matches[0].project, matches[0].name);
      if (!databasesByInstance.has(key)) databasesByInstance.set(key, []);
      databasesByInstance.get(key).push(database);
    } else unplacedDatabases.push(database);
  }
  const projectNodes = projects.map((project) => {
    const rows = instances.filter((row) => text(row.project, 'default') === project).sort((a, b) => byName(text(a.name), text(b.name)));
    const children = rows.map((instance) => {
      const key = instanceKey(machine ?? {}, project, instance.name);
      const rawStatus = text(instance.status);
      const tone = statusTone(rawStatus);
      const matched = placed.get(key) ?? [];
      return {
        kind: 'instance', key, project, name: text(instance.name), label: text(instance.name),
        rawName: text(instance.name), rawStatus, status: statusText(rawStatus, 'Status unknown'), tone,
        type: kindLabel(instance.kind), shortLine: shortLine(instance),
        stopped: rawStatus.toLowerCase() === 'stopped',
        owner: instance.owner ?? null, privileged: Boolean(instance.privileged), identity: Boolean(instance.identity),
        devices: matched, databases: databasesByInstance.get(key) ?? [],
        route: `#project/${encodeURIComponent(project)}/instance/${encodeURIComponent(text(instance.name))}`,
      };
    });
    const summary = rollup(children, 'Empty Incus project');
    const projectDevices = [...new Map(children.flatMap((child) => child.devices).map((device) => [`${device.kind}:${device.name}`, device])).values()];
    return {
      kind: 'project', key: `${hostId}|project:${project}`, project, label: projectLabel(project), rawName: project,
      rawStatus: summary.tone, status: summary.text, tone: summary.tone, children,
      devices: projectDevices,
      route: `#project/${encodeURIComponent(project)}`,
    };
  });
  const projectSummary = rollup(projectNodes.map((project) => ({ rawStatus: project.tone, tone: project.tone })), 'No Incus projects');
  const host = {
    kind: 'host', key: `${hostId}|host`, label: hostName, rawName: hostId, tone: projectSummary.tone,
    status: snapshot.instancesKnown === false ? 'Watch report unavailable'
      : `${count(projectNodes.length, 'project')} · ${count(instances.length, 'instance')}`,
    route: `#host/${encodeURIComponent(hostId)}`,
    children: projectNodes,
  };
  if (snapshot.instancesKnown === false) host.tone = 'bad';
  const devicesKnown = !['unavailable', 'unreachable'].includes(snapshot.devices);
  const notPlaced = {
    kind: 'not-placed', key: `${hostId}|not-placed`, label: 'Not placed', rawName: 'Not placed',
    status: !devicesKnown ? 'Device data unavailable'
      : unplaced.length ? `${count(unplaced.length, 'device')} Arca couldn't match to one instance`
        : 'No unplaced devices',
    tone: !devicesKnown ? 'bad' : unplaced.length ? 'warn' : 'neutral', devices: unplaced, route: '#not-placed',
  };
  const databaseNode = {
    kind: 'databases', key: `${hostId}|databases`, label: 'Databases', rawName: 'Databases',
    status: unplacedDatabases.length ? `${count(unplacedDatabases.length, 'database')} without one instance name` : 'No unplaced databases',
    tone: unplacedDatabases.length ? 'warn' : 'neutral', rows: unplacedDatabases, route: '#databases',
  };
  host.devices = devices;
  host.databases = databaseNode;
  host.notPlaced = notPlaced;
  const nodes = new Map();
  const add = (node) => {
    nodes.set(node.key, node);
    for (const child of node.children ?? []) add(child);
  };
  add(host);
  return { host, projects: projectNodes, notPlaced, databases: databaseNode, nodes, hostId };
}

export function parseRoute(fragment, tree) {
  const raw = text(fragment).replace(/^#/, '');
  const parts = raw.split('/').filter(Boolean);
  const decode = (value) => {
    try { return decodeURIComponent(value); } catch { return null; }
  };
  if (!raw) return { kind: 'host', key: tree.host.key, node: tree.host };
  if (parts[0] === 'host' && parts.length === 2) {
    if (decode(parts[1]) === tree.host.rawName) return { kind: 'host', key: tree.host.key, node: tree.host };
  }
  if (parts[0] === 'project' && parts.length >= 2) {
    const project = decode(parts[1]);
    const node = tree.projects.find((candidate) => candidate.project === project);
    if (node && parts.length === 2) return { kind: 'project', key: node.key, node };
    if (node && parts.length === 4 && parts[2] === 'instance') {
      const name = decode(parts[3]);
      const instance = node.children.find((candidate) => candidate.name === name);
      if (instance) return { kind: 'instance', key: instance.key, node: instance, project: node };
    }
  }
  if (parts[0] === 'not-placed' && parts.length === 1) return { kind: 'not-placed', key: tree.notPlaced.key, node: tree.notPlaced };
  if (parts[0] === 'databases' && parts.length === 1) return { kind: 'databases', key: tree.databases.key, node: tree.databases };
  return { kind: 'not-found', key: `${tree.hostId}|not-found`, node: null };
}

export function routeFor(node) {
  return node?.route ?? '#host';
}

export function ownerLabel(owner) {
  const value = text(owner?.class ?? owner, 'unknown');
  if (['kernel', 'retained'].includes(value)) return 'Managed by Arca';
  if (value === 'droidd') return 'Droidd';
  return humanize(value);
}
