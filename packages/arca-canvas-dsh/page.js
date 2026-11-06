(() => {
  // src/layout.js
  var COLUMNS = {
    machine: { w: 230, h: 210 },
    instance: { w: 260, h: 210 },
    database: { w: 210, h: 136 },
    alert: { w: 240, h: 160 },
    phone: { w: 230, h: 210 },
    desktop: { w: 240, h: 210 }
  };
  var CONTENT_WIDTH = 1440 - 396 - 2 * 16;
  var COLUMN_GAP = 40;
  var COLUMN_HEIGHT = 690;
  function spread(items) {
    const sum = items.reduce((height2, item) => height2 + item.h, 0);
    const height = Math.max(480, sum + 30 * (items.length - 1));
    const gap = items.length > 1 ? (height - sum) / (items.length - 1) : 0;
    let y = 0;
    return items.map((item) => {
      const placed = { ...item, y: Math.round(y) };
      y += item.h + gap;
      return placed;
    });
  }
  var list = (value) => Array.isArray(value) ? value : [];
  function alertRows(alerts) {
    return Array.isArray(alerts) ? alerts.filter((row) => row.state === "firing") : list(alerts?.firing).map((row) => ({ ...row, state: "firing" }));
  }
  function layoutCanvas(snapshot, contentWidth = CONTENT_WIDTH) {
    const groups = {
      machine: snapshot.machine ? [snapshot.machine] : [],
      instance: list(snapshot.instances),
      database: list(snapshot.databases),
      alert: alertRows(snapshot.alerts),
      phone: list(snapshot.phones),
      desktop: list(snapshot.desktops)
    };
    const nodes = [];
    let x = 0;
    let y = 0;
    let bottom = 0;
    const key = (row) => `${row.project ?? ""}/${row.name ?? row.node ?? row.rule ?? ""}`;
    const columns = Object.entries(groups).map(([kind, rows]) => {
      const sorted = [...rows].sort((a, b) => key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0);
      const capacity = Math.max(1, Math.floor((COLUMN_HEIGHT + 30) / (COLUMNS[kind].h + 30)));
      return { kind, sorted, capacity };
    });
    for (let offset = 0; columns.some(({ sorted, capacity }) => offset * capacity < sorted.length); offset++) {
      for (const { kind, sorted, capacity } of columns) {
        const start = offset * capacity;
        const rows = sorted.slice(start, start + capacity);
        if (!rows.length) continue;
        const column = COLUMNS[kind];
        const w = Math.min(column.w, contentWidth);
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

  // src/approval.js
  function approvalUrl(value) {
    if (typeof value !== "string" || !/^https?:\/\//i.test(value)) return "";
    try {
      const url = new URL(value);
      return ["http:", "https:"].includes(url.protocol) && !url.username && !url.password ? url.href : "";
    } catch {
      return "";
    }
  }

  // src/render.js
  function element(doc, tag, className, text2) {
    const node = doc.createElement(tag);
    node.className = className;
    if (text2 !== void 0) node.textContent = String(text2);
    return node;
  }
  var text = (value, fallback = "Not available") => value == null ? fallback : String(value);
  var number = (value, suffix = "") => Number.isFinite(value) ? `${Math.round(value * 10) / 10}${suffix}` : "Not available";
  function uptime(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0) return "Not available";
    const [unit, scale] = seconds < 120 ? ["s", 1] : seconds < 3600 ? ["m", 60] : seconds < 86400 ? ["h", 3600] : ["d", 86400];
    return `${Math.round(seconds / scale)} ${unit}`;
  }
  function ownerLabel(owner) {
    return ["kernel", "retained"].includes(owner?.class ?? owner) ? "Managed by Arca" : "External";
  }
  function cardContent(node, snapshot) {
    const row = node.data;
    switch (node.kind) {
      case "machine": {
        const metrics = snapshot.metrics?.latest;
        return {
          title: snapshot.nodeName?.trim() || "Machine",
          kind: "Machine",
          icon: "\u25C8",
          lines: [`CPU ${number(metrics?.cpu?.usedPct, "%")}`, `Load ${number(metrics?.load?.one)}`, `Arca up ${uptime(row.uptimeS)}`],
          state: row.incusReachable ? "Incus reachable" : "Incus unavailable",
          meta: text(row.mode)
        };
      }
      case "instance":
        return {
          title: text(row.name, "Instance"),
          kind: text(row.kind, "Instance"),
          icon: "\u25C7",
          lines: [`Project ${text(row.project)}`, ownerLabel(row.owner), row.privileged ? "Privileged" : "Unprivileged"],
          state: text(row.status, "Unknown"),
          meta: row.identity ? "Identity" : "Instance"
        };
      case "database":
        return {
          title: text(row.name, "Database"),
          kind: "PostgreSQL",
          icon: "\u25A4",
          lines: [row.address ? `${text(row.address)}:${text(row.port)}` : "Awaiting an address"],
          state: text(row.status, "Unknown"),
          meta: row.port ? `:${row.port}` : "Database"
        };
      case "phone":
        return {
          title: text(row.name, "Phone"),
          kind: "Phone",
          icon: "\u25AF",
          lines: [
            `Network ${text(row.network)}`,
            `Battery ${number(row.battery, "%")}`,
            row.size ? `Screen ${text(row.size[0])} \xD7 ${text(row.size[1])}` : "Screen not available"
          ],
          state: text(row.state, "Unknown"),
          meta: "Read only"
        };
      case "desktop":
        return {
          title: text(row.name, "Desktop"),
          kind: "Desktop",
          icon: "\u25A3",
          lines: [`Memory ${number(row.memoryBytes == null ? null : row.memoryBytes / 1024 ** 3, " GiB")}`],
          state: text(row.status, "Unknown"),
          meta: "Read only"
        };
      case "alert":
        return {
          title: text(row.rule ?? row.name, "Alert"),
          kind: "Alert",
          icon: "!",
          lines: [text(row.message ?? row.reason, "No further details")],
          state: text(row.state, "Unknown"),
          meta: text(row.target, "Machine")
        };
    }
  }
  function renderCanvas(root, snapshot) {
    const doc = root.ownerDocument;
    const shell = element(doc, "div", "arca-canvas");
    const bar = element(doc, "header", "arca-topbar");
    bar.dataset.region = "topbar";
    bar.append(element(doc, "strong", "arca-title", "\u25C8  Arca"), element(doc, "span", "arca-crumb", "Canvas"));
    bar.append(element(doc, "span", "arca-mode", text(snapshot.machine?.mode, "Read only")));
    const plans = Array.isArray(snapshot.plans) ? snapshot.plans.length : 0;
    const url = approvalUrl(snapshot.approvalUrl);
    if (snapshot.devices === "unavailable") bar.append(element(doc, "span", "arca-devices", "Devices unavailable"));
    if (plans) {
      const badge = element(doc, url ? "a" : "span", "arca-plans", `${plans} waiting plan${plans === 1 ? "" : "s"}`);
      if (url) badge.href = url;
      bar.append(badge);
    }
    const main = element(doc, "div", "arca-main");
    const stage = element(doc, "section", "arca-stage");
    stage.setAttribute("aria-label", "Infrastructure canvas");
    stage.dataset.region = "canvas";
    const status = element(doc, "div", "arca-status", snapshot.error ?? `Snapshot ${text(snapshot.at, "pending")}`);
    status.setAttribute("role", "status");
    const world = element(doc, "div", "arca-world");
    const summary = element(doc, "div", "arca-summary");
    const counts = [
      ["machine", snapshot.machine ? 1 : 0],
      ["instance", snapshot.instances?.length ?? 0],
      ["database", snapshot.databases?.length ?? 0],
      ["phone", snapshot.phones?.length ?? 0],
      ["desktop", snapshot.desktops?.length ?? 0]
    ];
    summary.append(element(doc, "span", "", counts.map(([kind, count]) => `${count} ${kind}${count === 1 ? "" : "s"}`).join(" \xB7 ")));
    const unknown = Array.isArray(snapshot.alerts?.unknown) ? snapshot.alerts.unknown.length : 0;
    if (unknown) summary.append(element(doc, "span", "arca-no-data", `${unknown} check${unknown === 1 ? " has" : "s have"} no data`));
    stage.append(status, summary, world);
    const panel = element(doc, "aside", "arca-panel");
    const steps = element(doc, "div", "arca-steps-placeholder");
    steps.dataset.region = "steps";
    steps.append(element(doc, "h2", "", "Steps"), element(doc, "p", "", "Steps are hidden until the journal is available."));
    panel.append(steps);
    main.append(stage, panel);
    shell.append(bar, main);
    root.replaceChildren(shell);
    const reflow = () => {
      const style = doc.defaultView?.getComputedStyle(stage);
      const width = style && stage.clientWidth ? Math.max(1, stage.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)) : void 0;
      const layout = layoutCanvas(snapshot, width);
      world.replaceChildren();
      world.style.width = `${layout.width}px`;
      world.style.height = `${layout.height}px`;
      for (const node of layout.nodes) {
        const content = cardContent(node, snapshot);
        const card = element(doc, "article", "arca-card");
        card.dataset.kind = node.kind;
        Object.assign(card.style, { left: `${node.x}px`, top: `${node.y}px`, width: `${node.w}px`, height: `${node.h}px` });
        const header = element(doc, "header", "arca-card-header");
        const title = element(doc, "div", "arca-card-title");
        title.append(element(doc, "strong", "", content.title), element(doc, "span", "", content.kind));
        header.append(element(doc, "span", "arca-icon", content.icon), title);
        const live = element(doc, "div", "arca-live");
        for (const line of content.lines) live.append(element(doc, "div", "", line));
        const footer = element(doc, "footer", "arca-state");
        footer.append(element(doc, "span", "", content.state), element(doc, "span", "arca-meta", content.meta));
        card.append(header, live, footer);
        world.append(card);
      }
      if (!layout.nodes.length) world.append(element(doc, "p", "arca-empty", "Waiting for Arca data."));
    };
    reflow();
    return { stage, reflow };
  }
  function mountCanvas(root, { fetchSnapshot = () => fetch("/arca-canvas/snapshot", { credentials: "same-origin" }), pollMs = 5e3 } = {}) {
    let stopped = false;
    let timer;
    let frame;
    let view;
    let measuredWidth;
    const win = root.ownerDocument.defaultView;
    const observer = win?.ResizeObserver ? new win.ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width;
      if (stopped || width === measuredWidth) return;
      measuredWidth = width;
      if (frame !== void 0) return;
      frame = win.requestAnimationFrame(() => {
        frame = void 0;
        if (!stopped) view.reflow();
      });
    }) : null;
    const render = () => {
      observer?.disconnect();
      view = renderCanvas(root, current);
      measuredWidth = void 0;
      observer?.observe(view.stage);
    };
    let current = { error: "Waiting for the first Arca snapshot." };
    const refresh = async () => {
      try {
        const response = await fetchSnapshot();
        if (!response.ok) throw new Error("Snapshot unavailable");
        current = await response.json();
      } catch {
        current = { ...current, error: "Arca snapshot is unavailable. Retrying\u2026" };
      }
      if (stopped) return;
      render();
      timer = setTimeout(refresh, pollMs);
    };
    render();
    void refresh();
    return () => {
      stopped = true;
      clearTimeout(timer);
      observer?.disconnect();
      if (frame !== void 0) win.cancelAnimationFrame(frame);
    };
  }

  // src/page.js
  mountCanvas(document.getElementById("arca-root"));
})();
