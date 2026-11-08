import { approvalUrl } from './approval.js';
import {
  buildDashboard, parseProjectHash, projectHash, relativeAge,
} from './model.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const text = (value, fallback = '') => value == null ? fallback : String(value);
const list = (value) => Array.isArray(value) ? value : [];
const count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;

function el(doc, tag, className, value) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  if (value !== undefined) node.textContent = String(value);
  return node;
}

function dot(doc, tone) {
  const node = el(doc, 'i', `arca-dot ${tone || 'unknown'}`);
  node.setAttribute('aria-hidden', 'true');
  return node;
}

function statusLine(doc, tone, label) {
  const row = el(doc, 'span', 'arca-status');
  row.append(dot(doc, tone), el(doc, 'span', '', label));
  return row;
}

function svgEl(doc, tag, attrs = {}, children = []) {
  const node = doc.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, String(value));
  node.append(...children);
  return node;
}

function icon(doc, name) {
  const svg = (children) => svgEl(doc, 'svg', {
    width: 15, height: 15, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor',
    'stroke-width': 1.4, 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true',
  }, children);
  const path = (d) => svgEl(doc, 'path', { d });
  switch (name) {
    case 'project': return svg([
      svgEl(doc, 'rect', { x: 2, y: 2, width: 12, height: 5, rx: 1.5 }),
      svgEl(doc, 'rect', { x: 2, y: 9, width: 12, height: 5, rx: 1.5 }),
      path('M4.6 4.5h.01M4.6 11.5h.01'),
    ]);
    case 'domain': return svg([
      svgEl(doc, 'circle', { cx: 8, cy: 8, r: 6 }),
      path('M2 8h12M8 2c2 2 2 10 0 12M8 2c-2 2-2 10 0 12'),
    ]);
    case 'container': return svg([
      svgEl(doc, 'path', { d: 'M8 1.8 14 4.8v6.4L8 14.2 2 11.2V4.8Z' }),
      path('M2 4.8 8 7.8l6-3M8 7.8v6.4'),
    ]);
    case 'vm': return svg([
      svgEl(doc, 'rect', { x: 1.5, y: 2.5, width: 13, height: 8.5, rx: 1.5 }),
      path('M6 14h4M8 11v3'),
    ]);
    case 'database': return svg([
      svgEl(doc, 'ellipse', { cx: 8, cy: 3.8, rx: 5, ry: 2 }),
      path('M3 3.8v8.4c0 1.1 2.2 2 5 2s5-.9 5-2V3.8M3 8c0 1.1 2.2 2 5 2s5-.9 5-2'),
    ]);
    case 'machine': return svg([
      svgEl(doc, 'rect', { x: 2, y: 2, width: 12, height: 5, rx: 1.5 }),
      svgEl(doc, 'rect', { x: 2, y: 9, width: 12, height: 5, rx: 1.5 }),
      path('M4.6 4.5h.01M4.6 11.5h.01'),
    ]);
    case 'storage': return svg([
      svgEl(doc, 'ellipse', { cx: 8, cy: 4, rx: 5.5, ry: 2.2 }),
      path('M2.5 4v8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2V4M2.5 8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2'),
    ]);
    case 'phone': return svg([
      svgEl(doc, 'rect', { x: 4.5, y: 1.5, width: 7, height: 13, rx: 1.8 }),
      path('M7 12.2h2'),
    ]);
    case 'clock': return svg([
      svgEl(doc, 'circle', { cx: 8, cy: 8, r: 6 }),
      path('M8 4.5V8l2.3 1.4'),
    ]);
    case 'check': return svg([path('m3.5 8.5 3 3 6-7')]);
    case 'plus': return svg([path('M8 2.5v11M2.5 8h11')]);
    case 'shield': return svg([
      path('M8 1.8 13.2 4v4c0 3-2.3 5.2-5.2 6.2C5.1 13.2 2.8 11 2.8 8V4Z'),
      path('m5.8 8 1.6 1.6 2.8-3'),
    ]);
    case 'send': return svg([path('M3 8h10M9 4l4 4-4 4')]);
    case 'spark': return svg([path('M8 1.5C8.6 5.4 10.6 7.4 14.5 8 10.6 8.6 8.6 10.6 8 14.5 7.4 10.6 5.4 8.6 1.5 8 5.4 7.4 7.4 5.4 8 1.5Z')]);
    default: return svg([svgEl(doc, 'circle', { cx: 8, cy: 8, r: 5 })]);
  }
}

function iconBox(doc, name, className = '') {
  const box = el(doc, 'span', `arca-icon ${className}`.trim());
  box.append(icon(doc, name));
  return box;
}

function titleBlock(doc, title, subtitle) {
  const box = el(doc, 'span', 'arca-nt');
  box.append(el(doc, 'strong', 'arca-nn', title), el(doc, 'span', 'arca-nk', subtitle));
  return box;
}

function cap(doc, value) {
  return el(doc, 'span', 'arca-cap', value);
}

function brandMark(doc) {
  const svg = svgEl(doc, 'svg', { viewBox: '0 0 100 100', width: 26, height: 26, 'aria-hidden': 'true', class: 'arca-mark' });
  svg.append(
    svgEl(doc, 'clipPath', { id: 'arca-mark-clip' }, [svgEl(doc, 'circle', { cx: 48, cy: 50, r: 40 })]),
    svgEl(doc, 'circle', { cx: 48, cy: 50, r: 40, fill: '#D97757' }),
    svgEl(doc, 'g', {
      'clip-path': 'url(#arca-mark-clip)', stroke: '#B0543A', 'stroke-width': 6.5,
      fill: 'none', 'stroke-linecap': 'round',
    }, [
      svgEl(doc, 'path', { d: 'M26 8 Q12 50 26 92' }),
      svgEl(doc, 'path', { d: 'M44 6 Q34 50 44 94' }),
      svgEl(doc, 'path', { d: 'M62 8 Q56 50 62 92' }),
    ]),
    svgEl(doc, 'path', { d: 'M70 66 C80 60 94 66 96 78 C97 84 92 88 86 87 L70 84 C65 79 65 71 70 66 Z', fill: '#F0B48E' }),
    svgEl(doc, 'circle', { cx: 81.5, cy: 73, r: 3.8, fill: '#2A1A15' }),
    svgEl(doc, 'circle', { cx: 95, cy: 80, r: 2.3, fill: '#2A1A15' }),
  );
  return svg;
}

function meter(doc, label, value, percent, emptyText) {
  const row = el(doc, 'div', 'arca-meter');
  row.append(el(doc, 'span', 'arca-meter-label', label));
  const bar = svgEl(doc, 'svg', {
    class: 'arca-meter-bar', viewBox: '0 0 100 6', preserveAspectRatio: 'none', 'aria-hidden': 'true',
  }, [
    svgEl(doc, 'rect', { class: 'arca-meter-track', width: 100, height: 6, rx: 3 }),
    svgEl(doc, 'rect', { class: 'arca-meter-fill', width: percent == null ? 0 : Math.max(0, Math.min(100, percent)), height: 6, rx: 3 }),
  ]);
  const valueNode = el(doc, 'span', 'arca-meter-value mono', value || emptyText || '');
  row.append(bar, valueNode);
  return row;
}

function machineCard(doc, machine) {
  const card = el(doc, 'article', 'arca-machine');
  card.dataset.host = machine.name;
  card.dataset.online = String(machine.online);
  const head = el(doc, 'div', 'arca-machine-head');
  head.append(el(doc, 'strong', 'arca-machine-name', machine.name), statusLine(doc, machine.tone, machine.status));
  card.append(head);
  const meters = el(doc, 'div', 'arca-meters');
  meters.append(
    meter(doc, 'CPU', machine.cpuLabel, machine.cpu?.usedPct, 'No metrics'),
    meter(doc, 'Mem', machine.memoryLabel, machine.memory?.usedPct, 'No metrics'),
  );
  card.append(meters);
  const foot = el(doc, 'div', 'arca-machine-foot');
  foot.append(el(doc, 'span', 'arca-machine-instances',
    machine.instanceCount === null ? 'Instances unknown' : count(machine.instanceCount, 'instance')));
  if (machine.error && !machine.metricsAt) foot.append(el(doc, 'span', 'arca-machine-warn', 'Partial data'));
  card.append(foot);
  return card;
}

function placeholderCard(doc, name, body) {
  const card = el(doc, 'article', 'arca-machine arca-placeholder');
  card.setAttribute('aria-label', `${name} placeholder`);
  const head = el(doc, 'div', 'arca-machine-head');
  head.append(el(doc, 'strong', 'arca-machine-name', name), el(doc, 'span', 'arca-soon', 'Soon'));
  card.append(head, iconBox(doc, name === 'Storage' ? 'storage' : 'phone', 'arca-placeholder-icon'), el(doc, 'p', 'arca-placeholder-body', body));
  return card;
}

function emptyState(doc, title, body) {
  const box = el(doc, 'div', 'arca-empty-state');
  box.append(el(doc, 'b', '', title), el(doc, 'span', '', body));
  return box;
}

function cardShell(doc, kind, x, y, width, height) {
  const node = el(doc, 'article', `arca-node arca-node-${kind}`);
  node.dataset.kind = kind;
  node.style.left = `${x}px`;
  node.style.top = `${y}px`;
  node.style.width = `${width}px`;
  node.style.height = `${height}px`;
  return node;
}

function fact(doc, label, value) {
  const box = el(doc, 'div', 'arca-stat');
  box.append(el(doc, 'b', '', value), el(doc, 'span', '', label));
  return box;
}

function hostTag(doc, host) {
  const tag = el(doc, 'span', 'arca-host-tag');
  tag.append(icon(doc, 'machine'), el(doc, 'span', '', `runs on ${host}`));
  return tag;
}

function projectCard(doc, project, x, y, width, height) {
  const node = cardShell(doc, 'project', x, y, width, height);
  node.dataset.project = project.name;
  const head = el(doc, 'div', 'arca-nh');
  head.append(iconBox(doc, 'project'), titleBlock(doc,
    project.name,
    project.kind === 'owner' ? 'Project'
      : project.kind === 'service' ? `Service · ${project.host}`
        : `Incus project ${project.rawName} · ${project.host}`));
  node.append(head);
  const facts = el(doc, 'div', 'arca-facts');
  facts.append(
    fact(doc, count(project.cards.length, 'thing'), project.cards.length),
    fact(doc, count(project.hosts.length, 'host'), project.hosts.length),
  );
  node.append(facts);
  if (project.missing) node.append(el(doc, 'p', 'arca-card-note', `${count(project.missing, 'configured member')} not found on a machine`));
  if (project.unknown) node.append(el(doc, 'p', 'arca-card-note', `${count(project.unknown, 'configured member')} unknown (host data unavailable)`));
  const foot = el(doc, 'div', 'arca-nf');
  foot.append(dot(doc, project.tone), el(doc, 'span', '', project.status), el(doc, 'span', 'arca-nf-right', 'self-hosted'));
  node.append(foot);
  return node;
}

function domainCard(doc, project, x, y, width, height) {
  const node = cardShell(doc, 'domain', x, y, width, height);
  const head = el(doc, 'div', 'arca-nh');
  head.append(iconBox(doc, 'domain'), titleBlock(doc, 'Domain', 'Project setting'));
  node.append(head);
  node.append(el(doc, 'div', 'arca-domain-name mono', project.domain));
  node.append(el(doc, 'p', 'arca-card-note', 'Set on this project; Arca does not manage DNS in this canvas.'));
  const foot = el(doc, 'div', 'arca-nf');
  foot.append(dot(doc, 'ok'), el(doc, 'span', '', 'Configured'), el(doc, 'span', 'arca-nf-right', 'config'));
  node.append(foot);
  return node;
}

function instanceCard(doc, card, x, y, width, height) {
  const node = cardShell(doc, 'instance', x, y, width, height);
  node.dataset.host = card.host;
  node.dataset.status = card.status;
  const head = el(doc, 'div', 'arca-nh');
  head.append(iconBox(doc, card.type === 'VM' ? 'vm' : 'container'), titleBlock(doc, card.name, card.type));
  node.append(head);
  const tags = el(doc, 'div', 'arca-card-tags');
  tags.append(hostTag(doc, card.host));
  if (card.url) {
    const link = el(doc, 'a', 'arca-card-link', 'Open');
    link.href = card.url;
    link.target = '_blank';
    link.rel = 'noopener';
    tags.append(link);
  }
  node.append(tags);
  const foot = el(doc, 'div', 'arca-nf');
  foot.append(dot(doc, card.tone), el(doc, 'span', '', card.status), el(doc, 'span', 'arca-nf-right', card.owner));
  node.append(foot);
  return node;
}

function databaseCard(doc, card, x, y, width, height) {
  const node = cardShell(doc, 'database', x, y, width, height);
  node.dataset.host = card.host;
  const head = el(doc, 'div', 'arca-nh');
  head.append(iconBox(doc, 'database'), titleBlock(doc, card.name, card.type));
  node.append(head);
  const tags = el(doc, 'div', 'arca-card-tags');
  tags.append(hostTag(doc, card.host));
  if (card.address) tags.append(el(doc, 'span', 'arca-prov', `${card.address}${card.port === null ? '' : `:${card.port}`}`));
  if (card.links.length) tags.append(el(doc, 'span', 'arca-prov', count(card.links.length, 'link')));
  if (card.url) {
    const link = el(doc, 'a', 'arca-card-link', 'Open');
    link.href = card.url;
    link.target = '_blank';
    link.rel = 'noopener';
    tags.append(link);
  }
  node.append(tags);
  const foot = el(doc, 'div', 'arca-nf');
  foot.append(dot(doc, card.tone), el(doc, 'span', '', card.status), el(doc, 'span', 'arca-nf-right', `via ${card.instance}`));
  node.append(foot);
  return node;
}

const DOMAIN_W = 230;
const HUB_W = 300;
const CARD_W = 350;
const CARD_H = 118;
const GAP = 12;
const TOP = 30;
const WORLD_W = 1000;

function wire(doc, from, to, edgeKind) {
  const group = svgEl(doc, 'g', { class: 'arca-wire' });
  group.dataset.edge = edgeKind;
  const d = `M ${from.x} ${from.y} C ${from.x + 42} ${from.y} ${to.x - 42} ${to.y} ${to.x} ${to.y}`;
  group.append(
    svgEl(doc, 'path', { class: 'arca-wire-base', d }),
    svgEl(doc, 'path', { class: 'arca-wire-flow', d }),
  );
  return group;
}

function buildWorld(doc, project) {
  const hasDomain = Boolean(project.domain);
  const hubX = hasDomain ? 270 : 0;
  const cardsX = 630;
  const rows = project.cards.length;
  const height = Math.max(460, TOP + rows * (CARD_H + GAP) - GAP + TOP);
  const world = el(doc, 'div', 'arca-world');
  world.style.width = `${WORLD_W}px`;
  world.style.height = `${height}px`;

  const wires = svgEl(doc, 'svg', {
    class: 'arca-wires', viewBox: `0 0 ${WORLD_W} ${height}`, width: WORLD_W, height,
    'aria-hidden': 'true',
  });
  const hubY = TOP;
  if (hasDomain) {
    wires.append(wire(doc, { x: DOMAIN_W, y: hubY + 88 }, { x: hubX, y: hubY + 88 }, 'domain-project'));
  }
  const cardY = (index) => TOP + index * (CARD_H + GAP);
  for (let index = 0; index < rows; index++) {
    wires.append(wire(doc, { x: hubX + HUB_W, y: hubY + 88 }, { x: cardsX, y: cardY(index) + CARD_H / 2 }, 'project-card'));
  }
  world.append(wires);

  if (hasDomain) {
    world.append(domainCard(doc, project, 0, hubY, DOMAIN_W, 176));
  }
  world.append(projectCard(doc, project, hubX, hubY, HUB_W, 176));
  project.cards.forEach((card, index) => {
    const y = cardY(index);
    world.append(card.kind === 'database'
      ? databaseCard(doc, card, cardsX, y, CARD_W, CARD_H)
      : instanceCard(doc, card, cardsX, y, CARD_W, CARD_H));
  });
  if (!project.cards.length) {
    const empty = cardShell(doc, 'empty', cardsX, TOP, CARD_W, 118);
    empty.append(project.unknown
      ? emptyState(doc, 'Host data unavailable', 'Configured members on a machine whose instance list could not be read stay hidden.')
      : emptyState(doc, 'No live things in this project', 'Configured members that are not on a machine stay hidden.'));
    world.append(empty);
  }
  return world;
}

function toolState(tone) {
  if (tone === 'bad') return 'bad';
  if (tone === 'warn') return 'wait';
  return 'done';
}

function toolCard(doc, record, now) {
  const card = el(doc, 'article', 'arca-tool');
  card.dataset.state = toolState(record.tone);
  const head = el(doc, 'div', 'arca-tool-head');
  const ic = el(doc, 'span', 'arca-tool-icon');
  ic.append(record.tone === 'bad' ? icon(doc, 'shield') : icon(doc, 'check'));
  head.append(ic, el(doc, 'span', 'arca-tool-name mono', record.entry));
  const age = relativeAge(record.at, now);
  head.append(el(doc, 'span', 'arca-tool-meta', age ? `${record.host} · ${age}` : record.host));
  card.append(head);
  card.append(el(doc, 'p', 'arca-tool-text', `${record.target ? `${record.target} - ` : ''}${record.state}`));
  if (record.plugin) {
    const links = el(doc, 'div', 'arca-tool-links');
    links.append(el(doc, 'span', 'arca-prov', record.plugin));
    card.append(links);
  }
  return card;
}

function stepCard(doc, record, now) {
  const card = el(doc, 'div', 'arca-step');
  card.dataset.state = toolState(record.tone);
  const ic = el(doc, 'span', 'arca-step-icon');
  ic.append(icon(doc, record.tone === 'bad' ? 'shield' : 'check'));
  const body = el(doc, 'div', 'arca-step-body');
  body.append(el(doc, 'p', 'arca-step-text', `${record.entry}${record.target ? ` · ${record.target}` : ''}`));
  const age = relativeAge(record.at, now);
  body.append(el(doc, 'span', 'arca-step-meta', `${record.host} · ${record.state}${age ? ` · ${age}` : ''}`));
  card.append(ic, body);
  return card;
}

function approvalCard(doc, plans, url) {
  const card = el(doc, 'article', 'arca-card arca-card-warn arca-approval');
  const head = el(doc, 'div', 'arca-card-head');
  head.append(el(doc, 'span', 'arca-badge wait', 'Waiting for approval'));
  head.append(el(doc, 'span', 'arca-card-meta', count(plans, 'plan')));
  card.append(head);
  card.append(el(doc, 'p', 'arca-card-text', `${count(plans, 'Arca plan')} ${plans === 1 ? 'needs' : 'need'} your approval before it can run.`));
  if (url) {
    const link = el(doc, 'a', 'arca-btn arca-btn-ink', 'Review approvals');
    link.href = url;
    card.append(link);
  } else {
    card.append(el(doc, 'span', 'arca-card-note', 'Approval page is not configured.'));
  }
  return card;
}

export function createCanvas(root) {
  const doc = root.ownerDocument;
  const shell = el(doc, 'div', 'arca-canvas');
  const topbar = el(doc, 'header', 'arca-topbar');
  const brand = el(doc, 'div', 'arca-brand');
  brand.append(brandMark(doc), el(doc, 'strong', 'arca-brand-name', 'Arca'), el(doc, 'span', 'arca-slash', '/'));
  const picker = el(doc, 'select', 'arca-project-select');
  picker.setAttribute('aria-label', 'Project');
  const pickerWrap = el(doc, 'label', 'arca-picker');
  pickerWrap.append(picker);
  const chevron = el(doc, 'span', 'arca-picker-chevron', '⌄');
  chevron.setAttribute('aria-hidden', 'true');
  pickerWrap.append(chevron);
  brand.append(pickerWrap);

  const modes = el(doc, 'div', 'arca-modes');
  modes.setAttribute('role', 'group');
  modes.setAttribute('aria-label', 'Where it runs');
  const arcaMode = el(doc, 'button', 'arca-mode');
  arcaMode.type = 'button';
  arcaMode.setAttribute('aria-pressed', 'true');
  arcaMode.append(el(doc, 'i', 'arca-mode-swatch arca-mode-self'), el(doc, 'span', '', 'Arca'), el(doc, 'small', '', 'self-hosted'));
  const cloudMode = el(doc, 'button', 'arca-mode');
  cloudMode.type = 'button';
  cloudMode.disabled = true;
  cloudMode.setAttribute('aria-pressed', 'false');
  cloudMode.title = 'Arca Cloud is coming soon';
  cloudMode.append(el(doc, 'i', 'arca-mode-swatch arca-mode-cloud'), el(doc, 'span', '', 'Arca Cloud'), el(doc, 'small', 'arca-soon', 'coming soon'));
  modes.append(arcaMode, cloudMode);

  const spacer = el(doc, 'span', 'arca-spacer');
  const summary = el(doc, 'span', 'arca-summary');
  const clock = el(doc, 'span', 'arca-clock-chip mono');
  const readOnly = el(doc, 'span', 'arca-readonly', 'Read only');
  topbar.append(brand, modes, spacer, summary, clock, readOnly);

  const main = el(doc, 'div', 'arca-main');
  const left = el(doc, 'div', 'arca-left');
  const machines = el(doc, 'section', 'arca-machines');
  machines.setAttribute('aria-label', 'Machines');
  const machinesHead = el(doc, 'div', 'arca-machines-head');
  const machinesMeta = el(doc, 'span', 'arca-machines-meta');
  machinesHead.append(cap(doc, 'Machines'), machinesMeta);
  const machinesRow = el(doc, 'div', 'arca-machines-row');
  machines.append(machinesHead, machinesRow);

  const stage = el(doc, 'section', 'arca-stage');
  stage.setAttribute('aria-label', 'Project canvas');
  const canvasHead = el(doc, 'div', 'arca-canvas-head');
  const canvasTitle = el(doc, 'h1', 'arca-canvas-title');
  const canvasChips = el(doc, 'div', 'arca-canvas-chips');
  canvasHead.append(canvasTitle, canvasChips);
  const scroller = el(doc, 'div', 'arca-scroller');
  const worldHost = el(doc, 'div', 'arca-world-host');
  scroller.append(worldHost);
  stage.append(canvasHead, scroller);
  left.append(machines, stage);

  const panel = el(doc, 'aside', 'arca-panel');
  panel.setAttribute('aria-label', 'Doctor');
  const panelHead = el(doc, 'div', 'arca-panel-head');
  const orb = el(doc, 'span', 'arca-orb');
  const panelTitles = el(doc, 'div', 'arca-panel-titles');
  const panelName = el(doc, 'strong', 'arca-panel-name', 'Doctor');
  const panelSub = el(doc, 'span', 'arca-panel-sub');
  panelTitles.append(panelName, panelSub);
  const tabs = el(doc, 'div', 'arca-tabs');
  tabs.setAttribute('role', 'tablist');
  tabs.setAttribute('aria-label', 'Doctor view');
  const chatTab = el(doc, 'button', 'arca-tab', 'Chat');
  chatTab.type = 'button';
  chatTab.id = 'arca-tab-chat';
  chatTab.setAttribute('role', 'tab');
  chatTab.setAttribute('aria-selected', 'true');
  chatTab.setAttribute('aria-controls', 'arca-chat-panel');
  const stepsTab = el(doc, 'button', 'arca-tab', 'Steps');
  stepsTab.type = 'button';
  stepsTab.id = 'arca-tab-steps';
  stepsTab.setAttribute('role', 'tab');
  stepsTab.setAttribute('aria-selected', 'false');
  stepsTab.setAttribute('aria-controls', 'arca-steps-panel');
  tabs.append(chatTab, stepsTab);
  panelHead.append(orb, panelTitles, tabs);

  const chatScroll = el(doc, 'div', 'arca-panel-scroll arca-chat-view');
  chatScroll.id = 'arca-chat-panel';
  chatScroll.setAttribute('role', 'tabpanel');
  chatScroll.setAttribute('aria-labelledby', 'arca-tab-chat');
  const thread = el(doc, 'div', 'arca-thread');
  chatScroll.append(thread);
  const stepsScroll = el(doc, 'div', 'arca-panel-scroll arca-steps-view');
  stepsScroll.id = 'arca-steps-panel';
  stepsScroll.setAttribute('role', 'tabpanel');
  stepsScroll.setAttribute('aria-labelledby', 'arca-tab-steps');
  stepsScroll.hidden = true;
  const steps = el(doc, 'div', 'arca-steps');
  stepsScroll.append(steps);

  const composer = el(doc, 'div', 'arca-composer');
  const composerBox = el(doc, 'div', 'arca-composer-box');
  const composerRow = el(doc, 'div', 'arca-composer-row');
  const input = el(doc, 'input', 'arca-composer-input');
  input.type = 'text';
  input.readOnly = true;
  input.id = 'arca-chat-input';
  input.placeholder = 'Chat is coming in phase 2';
  input.setAttribute('aria-label', 'Message the Doctor');
  composerRow.append(input);
  const composerBar = el(doc, 'div', 'arca-composer-bar');
  const plus = el(doc, 'button', 'arca-icon-button');
  plus.type = 'button';
  plus.setAttribute('aria-label', 'Add');
  plus.setAttribute('aria-disabled', 'true');
  plus.append(icon(doc, 'plus'));
  const perm = el(doc, 'span', 'arca-perm');
  perm.append(icon(doc, 'shield'), el(doc, 'span', '', 'Approval required'));
  const barSpacer = el(doc, 'span', 'arca-spacer');
  const model = el(doc, 'span', 'arca-model', 'Doctor');
  const send = el(doc, 'button', 'arca-send');
  send.type = 'button';
  send.setAttribute('aria-label', 'Send');
  send.setAttribute('aria-disabled', 'true');
  send.append(icon(doc, 'send'));
  composerBar.append(plus, perm, barSpacer, model, send);
  composerBox.append(composerRow, composerBar);
  const composerNote = el(doc, 'p', 'arca-composer-note', 'The composer is inert in this delivery.');
  composer.append(composerBox, composerNote);

  panel.append(panelHead, chatScroll, stepsScroll, composer);
  main.append(left, panel);
  shell.append(topbar, main);
  root.replaceChildren(shell);

  const state = {
    envelope: {}, dashboard: null, project: null, tab: 'chat',
    signatures: { machines: '', canvas: '', panel: '', picker: '' },
  };
  let clockTimer;
  const tick = () => {
    try {
      clock.textContent = new Intl.DateTimeFormat(undefined, {
        hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
      }).format(new Date());
    } catch { clock.textContent = ''; }
  };
  tick();
  clockTimer = setInterval(tick, 1000);

  const setTab = (name) => {
    state.tab = name;
    chatTab.setAttribute('aria-selected', String(name === 'chat'));
    stepsTab.setAttribute('aria-selected', String(name === 'steps'));
    chatScroll.hidden = name !== 'chat';
    stepsScroll.hidden = name !== 'steps';
  };
  chatTab.addEventListener('click', () => setTab('chat'));
  stepsTab.addEventListener('click', () => setTab('steps'));

  const updatePicker = () => {
    const projects = state.dashboard?.projects ?? [];
    const signature = projects.map((project) => `${project.name}\u0000${project.kind}`).join('\u0001');
    if (signature !== state.signatures.picker) {
      state.signatures.picker = signature;
      const fragment = doc.createDocumentFragment();
      for (const [kind, label] of [['owner', 'Your projects'], ['service', 'Services'], ['incus', 'Machine projects']]) {
        const rows = projects.filter((project) => project.kind === kind);
        if (!rows.length) continue;
        const group = doc.createElement('optgroup');
        group.label = label;
        for (const project of rows) {
          const option = doc.createElement('option');
          option.value = project.name;
          option.textContent = project.name;
          group.append(option);
        }
        fragment.append(group);
      }
      picker.replaceChildren(fragment);
    }
    picker.value = state.project?.name ?? '';
  };

  const updateSummary = () => {
    summary.textContent = state.dashboard?.summary ?? '';
    const online = (state.dashboard?.hosts ?? []).filter((machine) => machine.online).length;
    machinesMeta.textContent = `${online} online`;
  };

  const updateMachines = () => {
    const hosts = state.dashboard?.hosts ?? [];
    const signature = JSON.stringify(hosts.map(({ name, online, cpuLabel, memoryLabel, instanceCount, error, cpu, memory }) =>
      ({ name, online, cpuLabel, memoryLabel, instanceCount, error, cpuPct: cpu?.usedPct, memoryPct: memory?.usedPct })));
    if (signature === state.signatures.machines) return;
    state.signatures.machines = signature;
    const nodes = hosts.map((machine) => machineCard(doc, machine));
    nodes.push(
      placeholderCard(doc, 'Storage', 'Room for a snapshot target on each machine.'),
      placeholderCard(doc, 'Phones', 'Room for paired phones and desktops.'),
    );
    machinesRow.replaceChildren(...nodes);
  };

  const updateCanvas = () => {
    const project = state.project;
    if (!project) {
      canvasTitle.textContent = 'No projects';
      canvasChips.replaceChildren(el(doc, 'span', 'arca-chip', 'Read only'));
      const empty = el(doc, 'div', 'arca-world-empty');
      empty.append(emptyState(doc, 'No projects found', 'No owner project or Incus project has live instances.'));
      worldHost.replaceChildren(empty);
      state.signatures.canvas = 'empty';
      return;
    }
    canvasTitle.textContent = project.name;
    const chips = [
      `${count(project.cards.length, 'thing')}`,
      `${count(project.hosts.length, 'host')}`,
    ];
    if (project.domain) chips.push(project.domain);
    if (project.kind === 'service') chips.push(`${project.host} · service`);
    if (project.kind === 'incus') chips.push(`${project.host} · ${project.rawName}`);
    canvasChips.replaceChildren(...chips.map((value) => el(doc, 'span', 'arca-chip', value)));
    const signature = JSON.stringify({
      name: project.name, domain: project.domain, kind: project.kind, host: project.host,
      status: project.status, cards: project.cards, missing: project.missing, unknown: project.unknown,
    });
    if (signature === state.signatures.canvas) return;
    state.signatures.canvas = signature;
    const scrollTop = scroller.scrollTop;
    const scrollLeft = scroller.scrollLeft;
    worldHost.replaceChildren(buildWorld(doc, project));
    scroller.scrollTop = scrollTop;
    scroller.scrollLeft = scrollLeft;
  };

  const updatePanel = () => {
    const project = state.project;
    panelSub.textContent = 'Recent activity · all machines';
    const dashboard = state.dashboard;
    const journal = list(dashboard?.journal);
    const plans = dashboard?.plans ?? 0;
    const url = approvalUrl(state.envelope.approvalUrl);
    const pending = (state.envelope.hosts ?? []).some((host) => host.journalPending);
    const signature = JSON.stringify({ journal, plans, url, project: project?.name, pending });
    if (signature === state.signatures.panel) return;
    state.signatures.panel = signature;
    const now = Date.now();
    const chatScrollTop = chatScroll.scrollTop;
    const stepsScrollTop = stepsScroll.scrollTop;
    thread.replaceChildren();
    steps.replaceChildren();
    thread.append(el(doc, 'div', 'arca-thread-head',
      `Recent Arca activity · ${count(journal.length, 'event')}`));
    if (!journal.length) {
      const known = (state.envelope.hosts ?? []).some((host) => host.journalKnown);
      thread.append(pending
        ? emptyState(doc, 'Reading Arca activity…', 'The journal is catching up; this fills in on the next refresh.')
        : emptyState(doc,
          known ? 'No recent Arca activity' : 'Activity journal unavailable',
          known ? 'Arca has not applied a change recently.' : 'This host did not return a journal.'));
    } else {
      if (pending) thread.append(el(doc, 'p', 'arca-thread-note', 'Reading older activity…'));
      for (const record of journal) thread.append(toolCard(doc, record, now));
    }
    if (plans > 0) thread.append(approvalCard(doc, plans, url));
    if (!journal.length) {
      steps.append(el(doc, 'p', 'arca-steps-empty', pending ? 'Reading older activity…' : 'No recent Arca steps.'));
    }
    for (const record of journal) steps.append(stepCard(doc, record, now));
    if (plans > 0) steps.append(approvalCard(doc, plans, url));
    chatScroll.scrollTop = chatScrollTop;
    stepsScroll.scrollTop = stepsScrollTop;
  };

  const selectProject = (project, { updateHash = true } = {}) => {
    state.project = project;
    if (updateHash) {
      const hash = projectHash(project);
      if (location.hash !== hash) location.hash = hash;
    }
    updatePicker();
    updateCanvas();
    updatePanel();
  };

  picker.addEventListener('change', () => {
    const project = selectDashboardProject(picker.value);
    if (project) selectProject(project, { updateHash: true });
  });

  const selectDashboardProject = (name) => (state.dashboard?.projects ?? []).find((project) => project.name === name) ?? null;

  const onHash = () => {
    if (!state.dashboard) return;
    const name = parseProjectHash(location.hash);
    const project = name ? selectDashboardProject(name) : null;
    if (project) selectProject(project, { updateHash: false });
    else {
      const fallback = state.dashboard.defaultProject ?? state.dashboard.projects[0];
      if (fallback) {
        state.project = fallback;
        history.replaceState(null, '', projectHash(fallback));
        updatePicker();
        updateCanvas();
        updatePanel();
      }
    }
  };
  window.addEventListener('hashchange', onHash);

  const update = (envelope = {}) => {
    state.envelope = envelope;
    state.dashboard = buildDashboard(envelope);
    const previous = state.project ? selectDashboardProject(state.project.name) : null;
    const hashedName = parseProjectHash(location.hash);
    const hashed = hashedName ? selectDashboardProject(hashedName) : null;
    const project = hashed ?? previous ?? state.dashboard.defaultProject ?? state.dashboard.projects[0] ?? null;
    state.project = project;
    if (project) {
      const hash = projectHash(project);
      if (location.hash !== hash) history.replaceState(null, '', hash);
    }
    updateSummary();
    updateMachines();
    updatePicker();
    updateCanvas();
    updatePanel();
  };

  return {
    update,
    stop() {
      clearInterval(clockTimer);
      window.removeEventListener('hashchange', onHash);
    },
  };
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
    at: new Date().toISOString(), error: null, approvalUrl: '', projects: [], defaultProject: '',
    hosts: [],
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
