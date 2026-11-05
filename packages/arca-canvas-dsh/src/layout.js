export const COLUMNS = {
  machine: { x: 0, w: 230, h: 210 },
  instance: { x: 270, w: 260, h: 210 },
  database: { x: 570, w: 210, h: 136 },
  alert: { x: 820, w: 240, h: 160 },
};

// Spread cards over at least 480 px, preserving a minimum 30 px gap.
export function spread(items) {
  const sum = items.reduce((height, item) => height + item.h, 0);
  const height = Math.max(480, sum + 30 * (items.length - 1));
  const gap = items.length > 1 ? (height - sum) / (items.length - 1) : 0;
  let y = 0;
  return items.map((item) => {
    const placed = { ...item, y: Math.round(y) };
    y += item.h + gap;
    return placed;
  });
}

const list = (value) => Array.isArray(value) ? value : [];
export function alertRows(alerts) {
  return Array.isArray(alerts) ? alerts : [
    ...list(alerts?.firing).map((row) => ({ ...row, state: 'firing' })),
    ...list(alerts?.unknown).map((row) => ({ ...row, state: 'unknown' })),
    ...list(alerts?.resolvedRecent).map((row) => ({ ...row, state: 'resolved' })),
  ];
}

export function layoutCanvas(snapshot) {
  const groups = {
    machine: snapshot.machine ? [snapshot.machine] : [],
    instance: list(snapshot.instances),
    database: list(snapshot.databases),
    alert: alertRows(snapshot.alerts),
  };
  const nodes = [];
  for (const [kind, rows] of Object.entries(groups)) {
    const column = COLUMNS[kind];
    const key = (row) => `${row.project ?? ''}/${row.name ?? row.node ?? row.rule ?? ''}`;
    const sorted = [...rows].sort((a, b) => key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0);
    for (const [index, item] of spread(sorted.map((data) => ({ data, h: column.h }))).entries()) {
      nodes.push({ kind, id: `${kind}:${key(item.data)}:${index}`, x: column.x, w: column.w, ...item });
    }
  }
  return { width: 1060, height: Math.max(620, ...nodes.map((node) => node.y + node.h + 76)), nodes };
}
