import { approvalUrl } from './approval.js';
import {
  buildTree, deviceLabel, ownerLabel, parseRoute, projectLabel, statusTone,
} from './tree.js';

const text = (value, fallback = '') => value == null ? fallback : String(value);
const list = (value) => Array.isArray(value) ? value : [];
const count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
const RAIL_KEY = 'arca-canvas-rail-collapsed';
const SECTIONS_KEY = 'arca-canvas-sections';

function read(key, fallback) {
  try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
}
function write(key, value) {
  try { localStorage.setItem(key, value); } catch { /* local storage can be unavailable */ }
}

function el(doc, tag, className, value) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  if (value !== undefined) node.textContent = String(value);
  return node;
}

function dot(doc, tone) {
  const node = el(doc, 'span', `arca-dot ${tone || 'unknown'}`);
  node.setAttribute('aria-hidden', 'true');
  return node;
}

function stateLine(doc, value, tone) {
  const row = el(doc, 'span', 'arca-row-state');
  row.append(dot(doc, tone), el(doc, 'span', '', value));
  return row;
}

function ageLabel(at) {
  const elapsed = Date.now() - Date.parse(at);
  if (!Number.isFinite(elapsed) || elapsed < 0) return null;
  const seconds = Math.floor(elapsed / 1000);
  if (seconds < 60) return `${seconds}s old`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m old`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h old`;
  return `${Math.floor(seconds / 86400)}d old`;
}

function field(doc, label, value, cls = '') {
  const row = el(doc, 'div', `arca-field ${cls}`.trim());
  row.append(el(doc, 'span', 'arca-field-label', label), el(doc, 'span', 'arca-field-value', value));
  return row;
}

function emptyNotice(doc, title, body) {
  const box = el(doc, 'div', 'arca-empty-state');
  box.append(el(doc, 'h2', '', title), el(doc, 'p', '', body));
  return box;
}

function frameHeader(doc, label, meta, tone, raw) {
  const head = el(doc, 'div', 'arca-frame-head');
  const title = el(doc, 'div', 'arca-frame-title');
  title.append(el(doc, 'h2', '', label));
  if (raw) title.append(el(doc, 'span', 'arca-frame-raw', raw));
  const status = el(doc, 'div', 'arca-frame-state');
  status.append(dot(doc, tone), el(doc, 'span', '', meta));
  head.append(title, status);
  return head;
}

export function createCanvas(root) {
  const doc = root.ownerDocument;
  const shell = el(doc, 'div', 'arca-canvas');
  const topbar = el(doc, 'header', 'arca-topbar');
  const brand = el(doc, 'div', 'arca-brand');
  brand.append(el(doc, 'strong', 'arca-title', '◈ Arca'), el(doc, 'span', 'arca-crumb', 'Canvas'));
  const readOnly = el(doc, 'span', 'arca-readonly', 'Read only');
  const remoteView = (root.dataset?.arcaBase ?? '').includes('/approve/canvas');
  const context = el(doc, 'span', 'arca-context', remoteView ? 'Remote view' : '');
  const deviceBadge = el(doc, 'span', 'arca-devices');
  let plans = el(doc, 'span', 'arca-plans');
  topbar.append(brand, readOnly, ...(remoteView ? [context] : []), deviceBadge, plans);
  const main = el(doc, 'div', 'arca-main');
  const rail = el(doc, 'nav', 'arca-rail');
  rail.id = 'arca-menu';
  rail.setAttribute('aria-label', 'Canvas navigation');
  const railHead = el(doc, 'div', 'arca-rail-head');
  const railToggle = el(doc, 'button', 'arca-rail-toggle', '‹');
  railToggle.type = 'button';
  railToggle.setAttribute('aria-label', 'Collapse navigation');
  railToggle.setAttribute('aria-controls', 'arca-menu');
  const railTitle = el(doc, 'span', 'arca-rail-title', 'Navigate');
  railHead.append(railToggle, railTitle);
  const sections = el(doc, 'div', 'arca-sections');
  rail.append(railHead, sections);
  const stage = el(doc, 'section', 'arca-stage');
  stage.setAttribute('aria-label', 'Scoped canvas');
  const stageHead = el(doc, 'div', 'arca-stage-head');
  const stageKicker = el(doc, 'p', 'arca-stage-kicker');
  const stageTitle = el(doc, 'h1', 'arca-stage-title');
  const stageMeta = el(doc, 'p', 'arca-stage-meta');
  stageHead.append(stageKicker, stageTitle, stageMeta);
  const stageBody = el(doc, 'div', 'arca-stage-body');
  stage.append(stageHead, stageBody);
  const detail = el(doc, 'aside', 'arca-detail');
  detail.setAttribute('aria-label', 'Selected item detail');
  const detailKicker = el(doc, 'p', 'arca-detail-kicker');
  const detailTitle = el(doc, 'h2', 'arca-detail-title');
  const detailBody = el(doc, 'div', 'arca-detail-body');
  detail.append(detailKicker, detailTitle, detailBody);
  main.append(rail, stage, detail);
  shell.append(topbar, main);
  root.replaceChildren(shell);

  const collapsed = read(RAIL_KEY, '0') === '1';
  shell.classList.toggle('tree-collapsed', collapsed);
  railToggle.textContent = collapsed ? '›' : '‹';
  railToggle.setAttribute('aria-expanded', String(!collapsed));
  railToggle.addEventListener('click', () => {
    const next = !shell.classList.contains('tree-collapsed');
    shell.classList.toggle('tree-collapsed', next);
    railToggle.textContent = next ? '›' : '‹';
    railToggle.setAttribute('aria-expanded', String(!next));
    write(RAIL_KEY, next ? '1' : '0');
  });

  let sectionState = {};
  try { sectionState = JSON.parse(read(SECTIONS_KEY, '{}')) || {}; } catch { sectionState = {}; }
  const section = (id, title, countValue) => {
    const box = el(doc, 'section', 'arca-section');
    box.dataset.section = id;
    const head = el(doc, 'button', 'arca-section-head');
    head.type = 'button';
    head.setAttribute('aria-expanded', String(sectionState[id] !== false));
    head.append(el(doc, 'span', 'arca-section-title', title), el(doc, 'span', 'arca-section-count', countValue), el(doc, 'span', 'arca-section-chevron', sectionState[id] === false ? '›' : '⌄'));
    const body = el(doc, 'div', 'arca-section-body');
    body.setAttribute('role', 'tree');
    body.setAttribute('aria-label', title);
    body.hidden = sectionState[id] === false;
    head.addEventListener('click', () => {
      const open = body.hidden;
      body.hidden = !open;
      head.setAttribute('aria-expanded', String(open));
      head.lastChild.textContent = open ? '⌄' : '›';
      sectionState[id] = open;
      write(SECTIONS_KEY, JSON.stringify(sectionState));
    });
    box.append(head, body);
    sections.append(box);
    return body;
  };
  const hostsBody = section('hosts', 'Hosts', '0');
  const projectsBody = section('projects', 'Projects', '0');

  const state = {
    snapshot: null, model: null, route: null, rows: new Map(), nodes: new Map(), tiles: new Map(),
    stageRouteKey: null, focusKey: null, rendered: false,
  };

  function setHash(hash) {
    if (location.hash !== hash) location.hash = hash;
  }

  function railRow(node, kind, body) {
    if (state.rows.has(node.key)) return state.rows.get(node.key);
    const row = el(doc, 'button', 'arca-rail-row');
    row.type = 'button';
    row.dataset.key = node.key;
    row.dataset.kind = kind;
    row.setAttribute('role', 'treeitem');
    const label = el(doc, 'span', 'arca-rail-label', node.label);
    const stateNode = el(doc, 'span', 'arca-rail-state');
    row.append(label, stateNode);
    const entry = { row, label, stateNode };
    state.rows.set(node.key, entry);
    row.addEventListener('click', () => {
      state.focusKey = node.key;
      setHash(node.route);
      row.focus();
    });
    row.addEventListener('focus', () => { state.focusKey = node.key; });
    row.addEventListener('keydown', (event) => {
      const rows = [...body.querySelectorAll('.arca-rail-row')];
      const index = rows.indexOf(row);
      if (event.key === 'ArrowDown') { event.preventDefault(); rows[Math.min(rows.length - 1, index + 1)]?.focus(); }
      else if (event.key === 'ArrowUp') { event.preventDefault(); rows[Math.max(0, index - 1)]?.focus(); }
      else if (event.key === 'Home') { event.preventDefault(); rows[0]?.focus(); }
      else if (event.key === 'End') { event.preventDefault(); rows[rows.length - 1]?.focus(); }
      else if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); setHash(node.route); }
    });
    body.append(row);
    return entry;
  }

  function updateRail() {
    const model = state.model;
    const hosts = state.tree.hosts ?? [state.tree];
    for (const [index, { host: node }] of hosts.entries()) {
      const host = railRow(node, 'host', hostsBody);
      host.label.textContent = node.label;
      host.stateNode.replaceChildren(stateLine(doc, node.status, node.tone));
      host.row.setAttribute('aria-selected', String(state.route?.key === node.key));
      host.row.tabIndex = state.focusKey === node.key ? 0 : -1;
      if (hostsBody.children[index] !== host.row) hostsBody.insertBefore(host.row, hostsBody.children[index] ?? null);
    }
    hostsBody.closest('.arca-section').querySelector('.arca-section-count').textContent = String(hosts.length);
    const projects = model.projects;
    const selectedProject = state.route?.kind === 'instance'
      ? state.route.project.key : state.route?.key;
    for (const project of projects) {
      const entry = railRow(project, 'project', projectsBody);
      entry.label.textContent = project.label;
      entry.stateNode.replaceChildren(stateLine(doc, project.status, project.tone));
      entry.row.setAttribute('aria-selected', String(selectedProject === project.key));
      entry.row.tabIndex = state.focusKey === project.key ? 0 : -1;
    }
    const hostGroups = [];
    if (model.notPlaced.devices.length) hostGroups.push({ ...model.notPlaced, label: 'Not placed' });
    if (model.databases.rows.length) hostGroups.push({ ...model.databases, label: 'Databases not placed' });
    for (const group of hostGroups) {
      const entry = railRow(group, group.kind, hostsBody);
      entry.label.textContent = group.label;
      entry.stateNode.replaceChildren(stateLine(doc, group.status, group.tone));
      entry.row.setAttribute('aria-selected', String(state.route?.key === group.key));
      entry.row.tabIndex = state.focusKey === group.key ? 0 : -1;
    }
    const wanted = new Set([...hosts.map(({ host }) => host.key), ...hostGroups.map((group) => group.key), ...projects.map((project) => project.key)]);
    for (const [key, entry] of state.rows) {
      if (!wanted.has(key)) { entry.row.remove(); state.rows.delete(key); }
    }
    projectsBody.closest('.arca-section').querySelector('.arca-section-count').textContent = String(projects.length);
    if (!state.rows.has(state.focusKey)) state.focusKey = state.rows.has(selectedProject) ? selectedProject : model.host.key;
    const focus = state.rows.get(state.focusKey);
    if (focus) for (const [key, entry] of state.rows) entry.row.tabIndex = key === state.focusKey ? 0 : -1;
  }

  function nodeButton(entry) {
    const button = el(doc, 'button', `arca-canvas-node arca-node-${entry.kind}`);
    button.type = 'button';
    button.dataset.key = entry.key;
    if (entry.href) button.addEventListener('click', () => setHash(entry.href));
    const head = el(doc, 'div', 'arca-canvas-node-head');
    const title = el(doc, 'div', 'arca-canvas-node-title');
    const name = el(doc, 'strong');
    const type = el(doc, 'span');
    title.append(name, type);
    head.append(title);
    const stateNode = el(doc, 'span', 'arca-canvas-node-state');
    head.append(stateNode);
    const short = el(doc, 'p', 'arca-canvas-node-short');
    const extra = el(doc, 'div', 'arca-canvas-node-extra');
    button.append(head, short, extra);
    return { button, name, type, stateNode, short, extra };
  }

  function updateNode(entry, nodeEntry) {
    let item = state.nodes.get(entry.key);
    if (!item) {
      item = nodeButton(nodeEntry);
      state.nodes.set(entry.key, item);
    }
    item.name.textContent = nodeEntry.name;
    item.type.textContent = nodeEntry.type;
    item.stateNode.replaceChildren(dot(doc, nodeEntry.tone), el(doc, 'span', '', nodeEntry.status));
    item.short.textContent = nodeEntry.short ?? '';
    item.short.hidden = !nodeEntry.short;
    item.extra.replaceChildren();
    for (const database of nodeEntry.databases ?? []) {
      const badge = el(doc, 'div', 'arca-instance-db');
      badge.append(el(doc, 'span', 'arca-instance-db-label', 'Database'), el(doc, 'span', 'arca-instance-db-name', database.name ?? database.entry ?? 'unnamed'));
      item.extra.append(badge);
    }
    for (const device of nodeEntry.devices ?? []) {
      item.extra.append(el(doc, 'div', 'arca-instance-device', `${deviceLabel(device.kind)} · ${device.name} · matched by name`));
    }
    item.button.classList.toggle('is-stopped', Boolean(nodeEntry.stopped));
    item.button.classList.toggle('is-selected', state.route?.key === entry.key);
    item.button.dataset.kind = nodeEntry.kind;
    return item;
  }

  function syncStage(entries) {
    const seen = new Set();
    const ordered = [];
    for (const entry of entries) {
      seen.add(entry.key);
      const item = updateNode(entry, entry);
      ordered.push(item.button);
    }
    const current = [...stageBody.children];
    if (current.length !== ordered.length || ordered.some((node, index) => current[index] !== node)) {
      stageBody.replaceChildren(...ordered);
    }
    for (const [key, item] of state.nodes) {
      if (!seen.has(key)) { item.button.remove(); state.nodes.delete(key); }
    }
  }

  function stageEmpty(titleValue, bodyValue) {
    state.nodes.clear();
    stageBody.replaceChildren(emptyNotice(doc, titleValue, bodyValue));
  }

  function projectCanvas(project) {
    const host = state.model.host;
    const hostFrame = el(doc, 'section', 'arca-frame arca-host-frame');
    hostFrame.append(frameHeader(doc, host.label, host.status, host.tone, host.rawName));
    const projectFrame = el(doc, 'section', 'arca-frame arca-project-frame');
    projectFrame.append(frameHeader(doc, project.label, project.status, project.tone, project.rawName));
    const nodes = el(doc, 'div', 'arca-project-nodes');
    projectFrame.append(nodes);
    const devices = el(doc, 'section', 'arca-devices-frame');
    devices.append(el(doc, 'h3', '', 'Devices'));
    if (!project.devices.length) devices.append(el(doc, 'p', 'arca-frame-empty', ['off', 'unavailable', 'unreachable'].includes(state.snapshot.devices) ? 'Device data unavailable.' : 'No phones or desktops matched this project by name.'));
    else {
      for (const device of project.devices) {
        const row = el(doc, 'div', 'arca-device-row');
        row.append(dot(doc, statusTone(device.state)), el(doc, 'span', '', `${deviceLabel(device.kind)} · ${device.name}`), el(doc, 'span', 'arca-device-match', 'matched by name'));
        devices.append(row);
      }
    }
    projectFrame.append(devices);
    hostFrame.append(projectFrame);
    return { hostFrame, projectFrame, nodes };
  }

  function renderStage(route) {
    const snapshot = state.snapshot;
    const model = state.model;
    const title = (titleValue, kicker, meta) => {
      stageKicker.textContent = kicker;
      stageTitle.textContent = titleValue;
      stageMeta.textContent = meta;
    };
    if (route.kind === 'not-found') {
      title('View not found', 'Canvas', 'This address does not name a known host, project or instance.');
      stageEmpty('Not found', 'Choose a host or project from the left menu.');
      return;
    }
    if (snapshot.instancesKnown === false && ['host', 'project', 'instance'].includes(route.kind)) {
      title(model.host.label, 'Host', 'Watch report unavailable');
      state.nodes.clear();
      const frame = el(doc, 'section', 'arca-frame arca-host-frame');
      frame.append(frameHeader(doc, model.host.label, model.host.status, model.host.tone, model.host.rawName),
        emptyNotice(doc, 'Instance data unavailable', 'Arca did not return the watch report. This is not an empty host.'));
      stageBody.replaceChildren(frame);
      return;
    }
    if (route.kind === 'host') {
      title(model.host.label, 'Host', 'Containment by Incus project');
      const hostFrame = el(doc, 'section', 'arca-frame arca-host-frame');
      hostFrame.append(frameHeader(doc, model.host.label, model.host.status, model.host.tone, model.host.rawName));
      const projects = el(doc, 'div', 'arca-host-projects');
      const seenTiles = new Set();
      for (const project of model.projects) {
        seenTiles.add(project.key);
        let tile = state.tiles.get(project.key);
        if (!tile) {
          tile = el(doc, 'button', 'arca-project-tile');
          tile.type = 'button';
          tile.dataset.key = project.key;
          tile.addEventListener('click', () => setHash(project.route));
          tile.append(el(doc, 'strong'), el(doc, 'span', 'arca-project-tile-raw'), el(doc, 'span', 'arca-project-tile-state'), dot(doc, project.tone));
          state.tiles.set(project.key, tile);
        }
        tile.firstChild.textContent = project.label;
        tile.children[1].textContent = project.rawName;
        tile.children[2].textContent = project.status;
        projects.append(tile);
      }
      for (const [key, tile] of state.tiles) {
        if (!seenTiles.has(key)) { tile.remove(); state.tiles.delete(key); }
      }
      if (model.notPlaced.devices.length || model.databases.rows.length) {
        const links = el(doc, 'div', 'arca-host-links');
        if (model.notPlaced.devices.length) {
          const button = el(doc, 'button', 'arca-linkish', `Devices not placed · ${count(model.notPlaced.devices.length, 'device')}`);
          button.type = 'button';
          button.addEventListener('click', () => setHash(model.notPlaced.route));
          links.append(button);
        }
        if (model.databases.rows.length) {
          const button = el(doc, 'button', 'arca-linkish', `Databases not placed · ${count(model.databases.rows.length, 'database')}`);
          button.type = 'button';
          button.addEventListener('click', () => setHash(model.databases.route));
          links.append(button);
        }
        hostFrame.append(links);
      }
      hostFrame.append(projects);
      const devicesFrame = el(doc, 'section', 'arca-devices-frame arca-host-devices');
      devicesFrame.append(el(doc, 'h3', '', 'Devices'));
      if (!model.host.devices.length) devicesFrame.append(el(doc, 'p', 'arca-frame-empty',
        ['off', 'unavailable', 'unreachable'].includes(snapshot.devices)
          ? 'Device data unavailable.' : 'No phones or desktops returned.'));
      for (const device of model.host.devices) {
        const row = el(doc, 'div', 'arca-device-row');
        const place = device.matched ? `${device.matched.project}/${device.matched.name} · matched by name` : (device.reason ?? 'not placed');
        row.append(dot(doc, statusTone(device.state)), el(doc, 'span', '', `${deviceLabel(device.kind)} · ${device.name}`), el(doc, 'span', 'arca-device-match', `${device.state ?? 'Unknown'} · ${place}`));
        devicesFrame.append(row);
      }
      hostFrame.append(devicesFrame);
      const firing = list(snapshot.alerts?.firing).length;
      const unknown = list(snapshot.alerts?.unknown).length;
      if (firing || unknown) {
        const alerts = el(doc, 'div', 'arca-host-alerts');
        if (firing) alerts.append(el(doc, 'span', 'arca-alert-firing', `${firing} firing alert${firing === 1 ? '' : 's'}`));
        if (unknown) alerts.append(el(doc, 'span', 'arca-no-data', `${unknown} check${unknown === 1 ? ' has' : 's have'} no data`));
        hostFrame.append(alerts);
      }
      state.nodes.clear();
      stageBody.replaceChildren(hostFrame);
      return;
    }
    if (route.kind === 'project' || route.kind === 'instance') {
      const project = route.kind === 'project' ? route.node : model.projects.find((item) => item.project === route.node.project);
      if (!project) { title('Project not found', 'Canvas', 'This project no longer exists.'); stageEmpty('Project not found', 'Choose a project from the left menu.'); return; }
      title(route.kind === 'instance' ? route.node.name : project.label,
        route.kind === 'instance' ? 'Instance' : 'Project canvas',
        route.kind === 'instance' ? `${project.label} · ${route.node.status}` : `${count(project.children.length, 'instance')} · ${project.rawName}`);
      const { hostFrame, projectFrame, nodes } = projectCanvas(project);
      const nodeEntries = project.children.map((instance) => ({
        key: instance.key, kind: 'instance', name: instance.name, type: instance.type,
        status: instance.status, tone: instance.tone, short: instance.shortLine,
        stopped: instance.stopped, href: instance.route, databases: instance.databases, devices: [],
      }));
      // The project node grid is updated with the same keyed elements as the stage.
      state.stageRouteKey = `${route.kind}:${project.key}`;
      const seen = new Set();
      const ordered = [];
      for (const entry of nodeEntries) {
        seen.add(entry.key);
        const item = updateNode(entry, entry);
        ordered.push(item.button);
      }
      for (const [key, item] of state.nodes) {
        if (!seen.has(key)) { item.button.remove(); state.nodes.delete(key); }
      }
      nodes.replaceChildren(...ordered);
      stageBody.replaceChildren(hostFrame);
      return;
    }
    if (route.kind === 'not-placed') {
      title('Not placed', 'Host', model.notPlaced.status);
      if (!model.notPlaced.devices.length) {
        stageEmpty(['off', 'unavailable', 'unreachable'].includes(snapshot.devices) ? 'Device data unavailable' : 'No unplaced devices',
          ['off', 'unavailable', 'unreachable'].includes(snapshot.devices)
            ? 'The device reader did not answer. This does not mean the host has no devices.' : 'Every phone and desktop has one unique instance name.');
        return;
      }
      const listNode = el(doc, 'div', 'arca-device-list');
      for (const device of model.notPlaced.devices) {
        const row = el(doc, 'div', 'arca-device-row');
        row.append(dot(doc, statusTone(device.state)), el(doc, 'span', '', `${deviceLabel(device.kind)} · ${device.name}`), el(doc, 'span', 'arca-device-match', device.reason));
        listNode.append(row);
      }
      stageBody.replaceChildren(listNode);
      return;
    }
    if (route.kind === 'databases') {
      title('Databases', 'Host', model.databases.status);
      if (!model.databases.rows.length) { stageEmpty('No unplaced databases', 'No database row names one instance exactly.'); return; }
      const listNode = el(doc, 'div', 'arca-device-list');
      for (const database of model.databases.rows) {
        const row = el(doc, 'div', 'arca-device-row');
        row.append(dot(doc, statusTone(database.status)), el(doc, 'span', '', database.name ?? database.entry ?? 'unnamed'), el(doc, 'span', 'arca-device-match', database.status ?? 'Unknown'));
        listNode.append(row);
      }
      stageBody.replaceChildren(listNode);
    }
  }

  function renderDetail(route) {
    const snapshot = state.snapshot;
    const model = state.model;
    const scroll = detailBody.scrollTop;
    detailBody.replaceChildren();
    detailTitle.textContent = route.kind === 'not-found' ? 'Not found' : (route.node?.label ?? route.node?.name ?? 'Canvas');
    detailKicker.textContent = route.kind === 'host' ? 'Host' : route.kind === 'project' ? 'Project' : route.kind === 'instance' ? 'Instance' : 'Host';
    const add = (label, value, cls) => detailBody.append(field(doc, label, value, cls));
    if (route.kind === 'not-found') {
      detailBody.append(el(doc, 'p', 'arca-detail-copy', 'Choose a host or project from the left menu.'));
      return;
    }
    if (route.kind === 'host') {
      add('Host name', model.host.label);
      add('Node id', model.host.rawName, 'mono');
      add('Projects', count(model.projects.length, 'project'));
      add('Instances', snapshot.instancesKnown === false ? 'Watch report unavailable' : count(list(snapshot.instances).length, 'instance'));
      add('Incus', snapshot.machine?.incusReachable === false ? 'Unavailable' : 'Reachable');
      add('Devices', text(snapshot.devices, 'off'));
      add('Alerts', `${list(snapshot.alerts?.firing).length} firing · ${count(list(snapshot.alerts?.unknown).length, 'check')} with no data`);
    } else if (route.kind === 'project') {
      add('Friendly name', route.node.label);
      add('Raw Incus project', route.node.rawName, 'mono');
      add('Instances', count(route.node.children.length, 'instance'));
      add('Status', route.node.status);
      add('Devices', count(route.node.devices.length, 'device'));
    } else if (route.kind === 'instance') {
      add('Name', route.node.name, 'mono');
      add('Project', route.node.project, 'mono');
      add('Status', route.node.status);
      add('Type', route.node.type);
      add('Owner', ownerLabel(route.node.owner));
      add('Privileged', route.node.privileged ? 'Yes' : 'No');
      if (route.node.shortLine) add('Image or IP', route.node.shortLine);
      for (const database of route.node.databases) add('Database', database.name ?? database.entry ?? 'unnamed', 'mono');
      for (const device of route.node.devices) add('Matched by name', `${deviceLabel(device.kind)} ${device.name}`, 'mono');
    }
    detailBody.scrollTop = scroll;
  }

  function update(snapshot) {
    state.envelope = snapshot;
    state.tree = buildTree(snapshot);
    const primary = state.tree.hosts?.[0] ?? state.tree;
    if ((!location.hash || location.hash === '#host/host') && (primary.snapshot.machine || primary.snapshot.instancesKnown)) {
      history.replaceState(null, '', primary.host.route);
    }
    state.route = parseRoute(location.hash, state.tree);
    if (!state.focusKey) state.focusKey = state.route?.key ?? primary.host.key;
    renderRoute();
  }

  function renderRoute() {
    state.model = state.route.model;
    state.snapshot = state.model.snapshot;
    const snapshot = state.snapshot;
    const plansCount = list(snapshot.plans).length;
    const url = approvalUrl(state.envelope.approvalUrl);
    const plansTag = plansCount && url ? 'a' : 'span';
    if (plans.localName !== plansTag) {
      const next = el(doc, plansTag, 'arca-plans');
      plans.replaceWith(next);
      plans = next;
    }
    plans.textContent = plansCount ? `${plansCount} waiting plan${plansCount === 1 ? '' : 's'}` : '';
    plans.hidden = plansCount === 0;
    plans.className = plansCount && url ? 'arca-plans is-link' : 'arca-plans';
    if (plansCount && url) plans.href = url;
    const deviceState = snapshot.devices;
    const staleAge = deviceState === 'stale' ? ageLabel(snapshot.devicesAt) : null;
    const deviceLabelText = deviceState === 'unavailable' ? 'Devices unavailable'
      : deviceState === 'unreachable' ? 'Devices unreachable'
        : deviceState === 'stale' ? `Devices stale${staleAge ? ` · ${staleAge}` : ''}` : '';
    deviceBadge.textContent = deviceLabelText;
    deviceBadge.hidden = !deviceLabelText;
    deviceBadge.dataset.state = deviceState;
    updateRail();
    renderStage(state.route);
    renderDetail(state.route);
  }

  function onHash() {
    if (!state.tree) return;
    state.route = parseRoute(location.hash, state.tree);
    state.model = state.route.model;
    if (state.route.node) {
      state.focusKey = state.route.key;
      if (state.route.kind === 'instance') {
        state.focusKey = state.route.project.key;
      }
    }
    state.nodes.clear();
    renderRoute();
  }
  window.addEventListener('hashchange', onHash);
  return { update, stop: () => window.removeEventListener('hashchange', onHash) };
}

export function renderCanvas(root, snapshot) {
  const view = createCanvas(root);
  view.update(snapshot);
  return view;
}

export function mountCanvas(root, { fetchSnapshot, pollMs = 5000 } = {}) {
  const base = (root.dataset?.arcaBase ?? '/arca-canvas').replace(/\/+$/, '') || '/arca-canvas';
  const load = fetchSnapshot ?? (() => fetch(`${base}/snapshot`, { credentials: 'same-origin' }));
  const view = createCanvas(root);
  let stopped = false;
  let timer;
  let current = {
    at: new Date().toISOString(), error: null, approvalUrl: '', nodeName: 'Machine',
    machine: null, instances: [], instancesKnown: false, projects: [], databases: [], metrics: null,
    alerts: [], plans: [], devices: 'off', devicesAt: null, phones: [], desktops: [],
  };
  view.update(current);
  const refresh = async () => {
    try {
      const response = await load();
      if (!response.ok) throw new Error('Snapshot unavailable');
      current = await response.json();
    } catch {
      current = { ...current, error: 'Arca snapshot is unavailable. Retrying…' };
    }
    if (stopped) return;
    view.update(current);
    timer = setTimeout(refresh, pollMs);
  };
  void refresh();
  return () => {
    stopped = true;
    clearTimeout(timer);
    view.stop();
  };
}
