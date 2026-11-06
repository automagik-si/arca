export const COLUMNS = {
  machine: { w: 230, h: 210 },
  instance: { w: 260, h: 210 },
  database: { w: 210, h: 136 },
  alert: { w: 240, h: 160 },
  phone: { w: 230, h: 210 },
  desktop: { w: 240, h: 210 },
};

// Default viewport minus the Steps panel and both stage paddings.
const CONTENT_WIDTH = 1440 - 396 - 2 * 16;
const COLUMN_GAP = 40;
const COLUMN_HEIGHT = 690;

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
  return Array.isArray(alerts) ? alerts.filter((row) => row.state === 'firing')
    : list(alerts?.firing).map((row) => ({ ...row, state: 'firing' }));
}

export function layoutCanvas(snapshot, contentWidth = CONTENT_WIDTH) {
  const groups = {
    machine: snapshot.machine ? [snapshot.machine] : [],
    instance: list(snapshot.instances),
    database: list(snapshot.databases),
    alert: alertRows(snapshot.alerts),
    phone: list(snapshot.phones),
    desktop: list(snapshot.desktops),
  };
  const nodes = [];
  let x = 0;
  let y = 0;
  let bottom = 0;
  const key = (row) => `${row.project ?? ''}/${row.name ?? row.node ?? row.rule ?? ''}`;
  const columns = Object.entries(groups).map(([kind, rows]) => {
    const sorted = [...rows].sort((a, b) => key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0);
    const capacity = Math.max(1, Math.floor((COLUMN_HEIGHT + 30) / (COLUMNS[kind].h + 30)));
    return { kind, sorted, capacity };
  });
  // Give every populated kind a first column before placing overflow columns.
  // Otherwise a large instance list pushes all phones below many screens.
  for (let offset = 0; columns.some(({ sorted, capacity }) => offset * capacity < sorted.length); offset++) {
    for (const { kind, sorted, capacity } of columns) {
      const start = offset * capacity;
      const rows = sorted.slice(start, start + capacity);
      if (!rows.length) continue;
      const column = COLUMNS[kind];
      const w = Math.min(column.w, contentWidth);
      // Keep column sizes and gaps; start a row below the tallest prior stack.
      if (x > 0 && x + w > contentWidth) {
        x = 0;
        y = bottom + COLUMN_GAP;
      }
      for (const [index, item] of spread(rows.map((data) => ({ data, h: column.h }))).entries()) {
        const node = { kind, id: `${kind}:${key(item.data)}:${start + index}`, x, w, ...item, y: y + item.y };
        nodes.push(node);
        bottom = Math.max(bottom, node.y + node.h);
      }
      x += w + COLUMN_GAP;
    }
  }
  return { width: contentWidth, height: Math.max(620, bottom + 76), nodes };
}
