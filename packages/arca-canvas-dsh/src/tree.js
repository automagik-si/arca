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

function buildHostTree(snapshot = {}) {
  const machine = snapshot.machine ?? null;
  const nodeId = text(machine?.node, 'host');
  const hostId = text(snapshot.name, nodeId);
  const keyHost = encodeURIComponent(hostId);
  const hostSegment = snapshot.name === undefined ? '' : `${encodeURIComponent(snapshot.name)}/`;
  const groupSuffix = snapshot.name === undefined ? '' : `/${encodeURIComponent(snapshot.name)}`;
  const hostName = snapshot.name === undefined ? text(snapshot.nodeName, 'Machine').trim() || 'Machine' : text(snapshot.name);
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
      device.reason = matches.length ? 'name matches more than one instance' : 'no instance with this name';
      unplaced.push(device);
      continue;
    }
    const key = instanceKey({ node: keyHost }, matches[0].project, matches[0].name);
    device.matched = { project: text(matches[0].project, 'default'), name: matches[0].name };
    if (!placed.has(key)) placed.set(key, []);
    placed.get(key).push(device);
  }
  const databases = list(snapshot.databases).map((row) => ({ ...row, kind: 'database' }));
  const databasesByInstance = new Map();
  const unplacedDatabases = [];
  for (const database of databases) {
    const explicit = text(database.instance ?? database.droid ?? database.container);
    let matches = explicit ? instances.filter((row) => text(row.name) === explicit) : [];
    if (!matches.length) matches = instances.filter((row) => text(row.name) === text(database.name) || text(row.name) === text(database.entry));
    if (matches.length === 1) {
      const key = instanceKey({ node: keyHost }, matches[0].project, matches[0].name);
      if (!databasesByInstance.has(key)) databasesByInstance.set(key, []);
      databasesByInstance.get(key).push(database);
    } else unplacedDatabases.push(database);
  }
  const projectNodes = projects.map((project) => {
    const rows = instances.filter((row) => text(row.project, 'default') === project).sort((a, b) => byName(text(a.name), text(b.name)));
    const children = rows.map((instance) => {
      const key = instanceKey({ node: keyHost }, project, instance.name);
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
        route: `#project/${hostSegment}${encodeURIComponent(project)}/instance/${encodeURIComponent(text(instance.name))}`,
      };
    });
    const summary = rollup(children, 'Empty Incus project');
    const projectDevices = [...new Map(children.flatMap((child) => child.devices).map((device) => [`${device.kind}:${device.name}`, device])).values()];
    return {
      kind: 'project', key: `${keyHost}|project:${project}`, project, label: projectLabel(project), rawName: project,
      rawStatus: summary.tone, status: summary.text, tone: summary.tone, children,
      devices: projectDevices,
      route: `#project/${hostSegment}${encodeURIComponent(project)}`,
    };
  });
  const projectSummary = rollup(projectNodes.map((project) => ({ rawStatus: project.tone, tone: project.tone })), 'No Incus projects');
  const host = {
    kind: 'host', key: `${keyHost}|host`, label: hostName, rawName: nodeId, tone: projectSummary.tone,
    status: snapshot.instancesKnown === false ? 'Watch report unavailable'
      : `${count(projectNodes.length, 'project')} · ${count(instances.length, 'instance')}`,
    route: `#host/${encodeURIComponent(hostId)}`,
    children: projectNodes,
  };
  if (snapshot.instancesKnown === false) host.tone = 'bad';
  const devicesKnown = !['off', 'unavailable', 'unreachable'].includes(snapshot.devices);
  const notPlaced = {
    kind: 'not-placed', key: `${keyHost}|not-placed`, label: 'Not placed', rawName: 'Not placed',
    status: !devicesKnown ? 'Device data unavailable'
      : unplaced.length ? `${count(unplaced.length, 'device')} Arca couldn't match to one instance`
        : 'No unplaced devices',
    tone: !devicesKnown ? 'bad' : unplaced.length ? 'warn' : 'neutral', devices: unplaced, route: `#not-placed${groupSuffix}`,
  };
  const databaseNode = {
    kind: 'databases', key: `${keyHost}|databases`, label: 'Databases', rawName: 'Databases',
    status: unplacedDatabases.length ? `${count(unplacedDatabases.length, 'database')} without one instance name` : 'No unplaced databases',
    tone: unplacedDatabases.length ? 'warn' : 'neutral', rows: unplacedDatabases, route: `#databases${groupSuffix}`,
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
  return { host, projects: projectNodes, notPlaced, databases: databaseNode, nodes, hostId, snapshot };
}

export function buildTree(snapshot = {}) {
  if (!Array.isArray(snapshot.hosts)) return buildHostTree(snapshot);
  const hosts = snapshot.hosts.map(buildHostTree);
  return { ...hosts[0], hosts };
}

export function parseRoute(fragment, tree) {
  const raw = text(fragment).replace(/^#/, '');
  const parts = raw.split('/');
  const decode = (value) => {
    try { return decodeURIComponent(value); } catch { return null; }
  };
  const hosts = tree.hosts ?? [tree];
  const primary = hosts[0];
  const found = (model, node, project) => ({ kind: node.kind, key: node.key, node, model, ...(project ? { project } : {}) });
  if (!raw && primary) return found(primary, primary.host);
  if (parts[0] === 'host' && parts.length === 2) {
    const name = decode(parts[1]);
    const model = hosts.find((host) => host.hostId === name)
      ?? (primary?.host.rawName === name ? primary : null);
    if (model) return found(model, model.host);
  }
  if (parts[0] === 'project') {
    const qualified = parts.length === 3 || (parts.length === 5 && parts[3] === 'instance');
    const legacy = parts.length === 2 || (parts.length === 4 && parts[2] === 'instance');
    const model = qualified ? hosts.find((host) => host.hostId === decode(parts[1])) : primary;
    const offset = qualified ? 2 : 1;
    const node = (qualified || legacy) && model?.projects.find((candidate) => candidate.project === decode(parts[offset]));
    if (node && parts.length === offset + 1) return found(model, node);
    if (node) {
      const instance = node.children.find((candidate) => candidate.name === decode(parts[offset + 2]));
      if (instance) return found(model, instance, node);
    }
  }
  if (['not-placed', 'databases'].includes(parts[0]) && (parts.length === 1 || parts.length === 2)) {
    const model = parts.length === 1 ? primary : hosts.find((host) => host.hostId === decode(parts[1]));
    if (model) return found(model, parts[0] === 'not-placed' ? model.notPlaced : model.databases);
  }
  return { kind: 'not-found', key: `${primary?.hostId}|not-found`, node: null, model: primary };
}

export function routeFor(node) {
  return node?.route ?? '#host';
}

export function ownerLabel(owner) {
  const value = text(owner?.class ?? owner, 'unknown');
  return ['kernel', 'retained'].includes(value) ? 'Managed by Arca' : 'External';
}
