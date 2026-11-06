import { layoutCanvas } from './layout.js';
import { approvalUrl } from './approval.js';

function element(doc, tag, className, text) {
  const node = doc.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = String(text);
  return node;
}
const text = (value, fallback = 'Not available') => value == null ? fallback : String(value);
const number = (value, suffix = '') => Number.isFinite(value) ? `${Math.round(value * 10) / 10}${suffix}` : 'Not available';

function cardContent(node, snapshot) {
  const row = node.data;
  switch (node.kind) {
    case 'machine': {
      const metrics = snapshot.metrics?.latest;
      return {
        title: text(row.node, 'Machine'), kind: 'Machine', icon: '◈',
        lines: [`CPU ${number(metrics?.cpu?.usedPct, '%')}`, `Load ${number(metrics?.load?.one)}`, `Uptime ${number(row.uptimeS, ' s')}`],
        state: row.incusReachable ? 'Incus reachable' : 'Incus unavailable', meta: text(row.mode),
      };
    }
    case 'instance': return {
      title: text(row.name, 'Instance'), kind: text(row.kind, 'Instance'), icon: '◇',
      lines: [`Project ${text(row.project)}`, `Owner ${text(row.owner?.class ?? row.owner, 'Not available')}`, row.privileged ? 'Privileged' : 'Unprivileged'],
      state: text(row.status, 'Unknown'), meta: row.identity ? 'Identity' : 'Instance',
    };
    case 'database': return {
      title: text(row.name, 'Database'), kind: 'PostgreSQL', icon: '▤',
      lines: [row.address ? `${text(row.address)}:${text(row.port)}` : 'Awaiting an address'],
      state: text(row.status, 'Unknown'), meta: row.port ? `:${row.port}` : 'Database',
    };
    case 'alert': return {
      title: text(row.rule ?? row.name, 'Alert'), kind: 'Alert', icon: '!',
      lines: [text(row.message ?? row.reason, 'No further details')],
      state: text(row.state, 'Unknown'), meta: text(row.target, 'Machine'),
    };
  }
}

export function renderCanvas(root, snapshot) {
  const doc = root.ownerDocument;
  const shell = element(doc, 'div', 'arca-canvas');
  const bar = element(doc, 'header', 'arca-topbar');
  bar.dataset.region = 'topbar';
  bar.append(element(doc, 'strong', 'arca-title', '◈  Arca'), element(doc, 'span', 'arca-crumb', 'Canvas'));
  bar.append(element(doc, 'span', 'arca-mode', text(snapshot.machine?.mode, 'Read only')));
  const plans = Array.isArray(snapshot.plans) ? snapshot.plans.length : 0;
  const url = approvalUrl(snapshot.approvalUrl);
  const badge = element(doc, url && plans ? 'a' : 'span', 'arca-plans', `${plans} waiting plan${plans === 1 ? '' : 's'}`);
  if (url && plans) badge.href = url;
  bar.append(badge);
  const main = element(doc, 'div', 'arca-main');
  const stage = element(doc, 'section', 'arca-stage');
  stage.setAttribute('aria-label', 'Infrastructure canvas');
  stage.dataset.region = 'canvas';
  const status = element(doc, 'div', 'arca-status', snapshot.error ?? `Snapshot ${text(snapshot.at, 'pending')}`);
  status.setAttribute('role', 'status');
  const world = element(doc, 'div', 'arca-world');
  stage.append(status, world);
  const panel = element(doc, 'aside', 'arca-panel');
  const steps = element(doc, 'div', 'arca-steps-placeholder');
  steps.dataset.region = 'steps';
  steps.append(element(doc, 'h2', '', 'Steps'), element(doc, 'p', '', 'Steps are hidden until the journal is available.'));
  panel.append(steps);
  main.append(stage, panel);
  shell.append(bar, main);
  root.replaceChildren(shell);
  // Before an explicit width, this block fills the stage's content box.
  const layout = layoutCanvas(snapshot, world.clientWidth || undefined);
  world.style.width = `${layout.width}px`;
  world.style.height = `${layout.height}px`;
  for (const node of layout.nodes) {
    const content = cardContent(node, snapshot);
    const card = element(doc, 'article', 'arca-card');
    card.dataset.kind = node.kind;
    Object.assign(card.style, { left: `${node.x}px`, top: `${node.y}px`, width: `${node.w}px`, height: `${node.h}px` });
    const header = element(doc, 'header', 'arca-card-header');
    const title = element(doc, 'div', 'arca-card-title');
    title.append(element(doc, 'strong', '', content.title), element(doc, 'span', '', content.kind));
    header.append(element(doc, 'span', 'arca-icon', content.icon), title);
    const live = element(doc, 'div', 'arca-live');
    for (const line of content.lines) live.append(element(doc, 'div', '', line));
    const footer = element(doc, 'footer', 'arca-state');
    footer.append(element(doc, 'span', '', content.state), element(doc, 'span', 'arca-meta', content.meta));
    card.append(header, live, footer);
    world.append(card);
  }
  if (!layout.nodes.length) world.append(element(doc, 'p', 'arca-empty', 'Waiting for Arca data.'));
}

export function mountCanvas(root, { fetchSnapshot = () => fetch('/arca-canvas/snapshot', { credentials: 'same-origin' }), pollMs = 5000 } = {}) {
  let stopped = false;
  let timer;
  let current = { error: 'Waiting for the first Arca snapshot.' };
  const refresh = async () => {
    try {
      const response = await fetchSnapshot();
      if (!response.ok) throw new Error('Snapshot unavailable');
      current = await response.json();
    } catch {
      current = { ...current, error: 'Arca snapshot is unavailable. Retrying…' };
    }
    if (stopped) return;
    renderCanvas(root, current);
    timer = setTimeout(refresh, pollMs);
  };
  renderCanvas(root, current);
  void refresh();
  return () => { stopped = true; clearTimeout(timer); };
}
