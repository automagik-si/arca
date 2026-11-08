(() => {
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

  // src/model.js
  var list = (value) => Array.isArray(value) ? value : [];
  var text = (value, fallback = "") => value == null ? fallback : String(value);
  var count = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
  var byName = (a, b) => a.localeCompare(b, void 0, { numeric: true, sensitivity: "base" });
  function humanize(value) {
    const raw = text(value, "Unnamed");
    return raw.replace(/[_-]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
  }
  function projectLabel(value) {
    const id = text(value, "Unnamed project");
    if (id === "default") return "Default";
    if (id === "bench") return "Bench";
    if (id === "arcak-svc") return "Arca services";
    if (id.startsWith("arcak-adopt-")) return `Adopted ${humanize(id.slice("arcak-adopt-".length))}`;
    return humanize(id);
  }
  function instanceKindLabel(kind) {
    const value = text(kind).toLowerCase();
    if (value === "virtual-machine" || value === "vm") return "VM";
    if (value === "container") return "Container";
    return humanize(value || "Instance");
  }
  function statusTone(status) {
    const value = text(status).trim().toLowerCase();
    if (["running", "ready", "active", "started", "ok", "up", "reachable", "applied", "committed"].includes(value)) return "ok";
    if (["stopped", "exited", "off", "disabled", "retained"].includes(value)) return "neutral";
    if (["error", "failed", "unreachable", "unavailable", "down", "missing", "rejected"].includes(value)) return "bad";
    return value ? "warn" : "unknown";
  }
  function ownerLabel(owner) {
    const value = text(owner?.class ?? owner, "unknown");
    return ["kernel", "retained"].includes(value) ? "Managed by Arca" : "External";
  }
  function finite(value) {
    const number = Number(value);
    return Number.isFinite(number) ? number : null;
  }
  function clampPercent(value) {
    return Math.max(0, Math.min(100, value));
  }
  function formatNumber(value, digits = 1) {
    if (!Number.isFinite(value)) return "";
    return `${Math.round(value * 10 ** digits) / 10 ** digits}`;
  }
  function formatGb(bytes) {
    const value = Number(bytes);
    if (!Number.isFinite(value) || value < 0) return "";
    const gb = value / 1e9;
    if (gb >= 10) return String(Math.round(gb));
    return String(Math.round(gb * 10) / 10);
  }
  function isHostOnline(host = {}) {
    return Boolean(host.machine) && host.machine.incusReachable !== false;
  }
  function hostCpu(host = {}) {
    const cpu = host.metrics?.latest?.cpu;
    const cores = finite(cpu?.cores);
    const usedPct = finite(cpu?.usedPct);
    if (cores === null && usedPct === null) return null;
    return {
      cores,
      usedPct: usedPct === null ? null : clampPercent(usedPct),
      usedCores: cores !== null && usedPct !== null ? clampPercent(usedPct) / 100 * cores : null
    };
  }
  function hostMemory(host = {}) {
    const memory = host.metrics?.latest?.memory;
    const totalBytes = finite(memory?.totalBytes);
    const availableBytes = finite(memory?.availableBytes);
    if (totalBytes === null && availableBytes === null) return null;
    const usedBytes = totalBytes !== null && availableBytes !== null ? Math.max(0, totalBytes - availableBytes) : null;
    return {
      totalBytes,
      availableBytes,
      usedBytes,
      usedPct: usedBytes !== null && totalBytes > 0 ? clampPercent(usedBytes / totalBytes * 100) : null
    };
  }
  function cpuLabel(cpu) {
    if (!cpu) return "";
    if (cpu.usedCores !== null && cpu.cores !== null) return `${formatNumber(cpu.usedCores)} / ${count(cpu.cores, "core")}`;
    if (cpu.cores !== null) return count(cpu.cores, "core");
    return `${formatNumber(cpu.usedPct)}% used`;
  }
  function memoryLabel(memory) {
    if (!memory) return "";
    if (memory.usedBytes !== null && memory.totalBytes !== null) return `${formatGb(memory.usedBytes)} / ${formatGb(memory.totalBytes)} GB`;
    if (memory.totalBytes !== null) return `${formatGb(memory.totalBytes)} GB total`;
    return "";
  }
  function fleetSummary(hosts = []) {
    const rows = list(hosts);
    const parts = [count(rows.length, "machine")];
    const cores = rows.reduce((sum, host) => sum + (hostCpu(host)?.cores ?? 0), 0);
    if (cores > 0) parts.push(count(cores, "core"));
    const bytes = rows.reduce((sum, host) => sum + (hostMemory(host)?.totalBytes ?? 0), 0);
    if (bytes > 0) parts.push(`${formatGb(bytes)} GB`);
    return parts.join(" \xB7 ");
  }
  function normalizeHosts(envelope) {
    if (Array.isArray(envelope.hosts)) return envelope.hosts;
    return [{ ...envelope, name: text(envelope.nodeName, "Machine") }];
  }
  function instanceKey(hostName, project, instance) {
    return `${text(hostName)}|${text(project, "default")}|${text(instance)}`;
  }
  function instanceUrl(instance) {
    const candidates = [
      instance?.url,
      instance?.webUrl,
      instance?.endpoint,
      ...list(instance?.urls).map((value) => typeof value === "string" ? value : value?.url)
    ];
    for (const candidate of candidates) {
      if (typeof candidate !== "string" || !candidate) continue;
      try {
        const parsed = new URL(candidate);
        if ((parsed.protocol === "http:" || parsed.protocol === "https:") && !parsed.username && !parsed.password) return candidate;
      } catch {
      }
    }
    return "";
  }
  function targetLabel(target) {
    if (typeof target === "string") return target;
    if (!target || typeof target !== "object") return "";
    const kind = text(target.kind);
    const name = text(target.name);
    if (kind && name) return `${kind} ${name}`;
    if (name) return name;
    if (kind && target.project) return `${kind} in ${target.project}`;
    return kind;
  }
  function rollup(cards) {
    if (!cards.length) return { tone: "neutral", text: "Nothing live yet" };
    const tones = cards.map((card) => card.tone);
    const bad = tones.filter((tone2) => tone2 === "bad").length;
    const warn = tones.filter((tone2) => tone2 === "warn").length;
    const running = cards.filter((card) => card.status.toLowerCase() === "running").length;
    const ready = cards.filter((card) => card.status.toLowerCase() === "ready").length;
    const stopped = cards.filter((card) => card.status.toLowerCase() === "stopped").length;
    const tone = bad ? "bad" : warn ? "warn" : tones.every((value) => value === "neutral") ? "neutral" : "ok";
    const parts = [];
    if (running) parts.push(`${running} running`);
    if (ready) parts.push(`${ready} ready`);
    if (stopped) parts.push(`${stopped} stopped`);
    if (bad) parts.push(`${bad} unavailable`);
    else if (warn) parts.push(`${warn} unknown`);
    return { tone, text: parts.join(" \xB7 ") || "Live" };
  }
  function buildMachine(host) {
    const cpu = hostCpu(host);
    const memory = hostMemory(host);
    const online = isHostOnline(host);
    return {
      name: text(host.name, text(host.nodeName, "Machine")),
      label: text(host.name, text(host.nodeName, "Machine")),
      online,
      status: online ? "Online" : "Offline",
      tone: online ? "ok" : "bad",
      cpu,
      cpuLabel: cpuLabel(cpu),
      memory,
      memoryLabel: memoryLabel(memory),
      instanceCount: host.instancesKnown === false ? null : list(host.instances).length,
      instancesKnown: host.instancesKnown !== false,
      error: host.error ?? null,
      metricsAt: host.metrics?.latest?.at ?? null
    };
  }
  function instanceCard(hostName, instance) {
    const status = text(instance.status, "Unknown");
    return {
      kind: "instance",
      key: instanceKey(hostName, instance.project, instance.name),
      host: hostName,
      incusProject: text(instance.project, "default"),
      name: text(instance.name, "Unnamed instance"),
      status,
      tone: statusTone(status),
      type: instanceKindLabel(instance.kind),
      owner: ownerLabel(instance.owner),
      privileged: Boolean(instance.privileged),
      url: instanceUrl(instance)
    };
  }
  function databaseCard(hostName, instance, database) {
    const status = text(database.status, "Unknown");
    return {
      kind: "database",
      key: `${instanceKey(hostName, instance.project, instance.name)}|db:${text(database.name ?? database.entry, "db")}`,
      host: hostName,
      incusProject: text(instance.project, "default"),
      instance: text(instance.name),
      name: text(database.name ?? database.entry, "Database"),
      status,
      tone: statusTone(status),
      type: "PostgreSQL",
      address: text(database.address ?? database.host),
      port: finite(database.port),
      links: list(database.links),
      url: instanceUrl(database)
    };
  }
  function databaseTargets(hosts) {
    const targets = /* @__PURE__ */ new Map();
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
  function buildDashboard(envelope = {}) {
    const hosts = normalizeHosts(envelope);
    const machines = hosts.map(buildMachine);
    const usedNames = /* @__PURE__ */ new Set();
    const claimed = /* @__PURE__ */ new Set();
    const projects = [];
    const databaseCards = databaseTargets(hosts);
    for (const config of list(envelope.projects)) {
      const name = text(config.name).trim();
      if (!name || usedNames.has(name)) continue;
      const cards = [];
      let missing = 0;
      for (const member of list(config.members)) {
        const host = hosts.find((candidate) => candidate.name === member.host);
        const instance = list(host?.instances).find((candidate) => text(candidate.project, "default") === text(member.incusProject) && text(candidate.name) === text(member.instance));
        if (!host || !instance) {
          missing++;
          continue;
        }
        const card = instanceCard(host.name, instance);
        cards.push(card);
        claimed.add(card.key);
      }
      for (const card of [...cards]) {
        const target = databaseCards.get(card.key);
        if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
      }
      usedNames.add(name);
      const roll = rollup(cards);
      projects.push({
        id: `owner:${name}`,
        kind: "owner",
        name,
        label: name,
        rawName: "",
        host: null,
        domain: text(config.domain).trim(),
        cards,
        hosts: [...new Set(cards.map((card) => card.host))],
        status: roll.text,
        tone: roll.tone,
        missing
      });
    }
    for (const host of hosts) {
      const projectIds = new Set(list(host.projects).map((value) => text(value)).filter(Boolean));
      for (const instance of list(host.instances)) projectIds.add(text(instance.project, "default"));
      for (const project of [...projectIds].sort(byName)) {
        const unlisted = list(host.instances).filter((instance) => text(instance.project, "default") === project && !claimed.has(instanceKey(host.name, instance.project, instance.name)));
        if (!unlisted.length) continue;
        const cards = unlisted.sort((a, b) => byName(text(a.name), text(b.name))).map((instance) => instanceCard(host.name, instance));
        for (const card of [...cards]) {
          const target = databaseCards.get(card.key);
          if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
        }
        const base = projectLabel(project);
        const name = uniqueName(base, usedNames, host.name);
        usedNames.add(name);
        const roll = rollup(cards);
        projects.push({
          id: `incus:${text(host.name)}/${project}`,
          kind: "incus",
          name,
          label: name,
          rawName: project,
          host: text(host.name),
          domain: "",
          cards,
          hosts: [text(host.name)],
          status: roll.text,
          tone: roll.tone,
          missing: 0
        });
      }
    }
    const configuredDefault = text(envelope.defaultProject).trim();
    const defaultProject = projects.find((project) => project.name === configuredDefault) ?? projects.find((project) => project.kind === "owner") ?? projects[0] ?? null;
    return {
      hosts: machines,
      projects,
      defaultProject,
      summary: fleetSummary(hosts),
      plans: hosts.reduce((sum, host) => sum + list(host.plans).length, 0),
      journal: journalFeed(hosts)
    };
  }
  function journalFeed(hosts = [], limit = 8) {
    const records = [];
    for (const host of list(hosts)) {
      for (const [index, record] of list(host.journal).entries()) {
        if (!record || typeof record !== "object") continue;
        records.push({
          key: `${text(host.name)}:${record.id ?? index}`,
          host: text(host.name, "Machine"),
          entry: text(record.entry, "activity"),
          state: text(record.state, "Unknown"),
          tone: statusTone(record.state),
          target: targetLabel(record.target),
          plugin: text(record.plugin),
          at: finite(record.at_ms)
        });
      }
    }
    records.sort((a, b) => (b.at ?? 0) - (a.at ?? 0));
    return records.slice(0, limit);
  }
  function projectHash(project) {
    return project ? `#project/${encodeURIComponent(project.name)}` : "";
  }
  function parseProjectHash(hash) {
    const match = /^#project\/(.+)$/.exec(text(hash));
    if (!match) return null;
    try {
      return decodeURIComponent(match[1]);
    } catch {
      return null;
    }
  }
  function relativeAge(at, now = Date.now()) {
    if (!Number.isFinite(at)) return "";
    const elapsed = Math.max(0, now - at);
    if (elapsed < 6e4) return `${Math.floor(elapsed / 1e3)}s`;
    if (elapsed < 36e5) return `${Math.floor(elapsed / 6e4)}m`;
    if (elapsed < 864e5) return `${Math.floor(elapsed / 36e5)}h`;
    return `${Math.floor(elapsed / 864e5)}d`;
  }

  // src/render.js
  var SVG_NS = "http://www.w3.org/2000/svg";
  var list2 = (value) => Array.isArray(value) ? value : [];
  var count2 = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
  function el(doc, tag, className, value) {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    if (value !== void 0) node.textContent = String(value);
    return node;
  }
  function dot(doc, tone) {
    const node = el(doc, "i", `arca-dot ${tone || "unknown"}`);
    node.setAttribute("aria-hidden", "true");
    return node;
  }
  function statusLine(doc, tone, label) {
    const row = el(doc, "span", "arca-status");
    row.append(dot(doc, tone), el(doc, "span", "", label));
    return row;
  }
  function svgEl(doc, tag, attrs = {}, children = []) {
    const node = doc.createElementNS(SVG_NS, tag);
    for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, String(value));
    node.append(...children);
    return node;
  }
  function icon(doc, name) {
    const svg = (children) => svgEl(doc, "svg", {
      width: 15,
      height: 15,
      viewBox: "0 0 16 16",
      fill: "none",
      stroke: "currentColor",
      "stroke-width": 1.4,
      "stroke-linecap": "round",
      "stroke-linejoin": "round",
      "aria-hidden": "true"
    }, children);
    const path = (d) => svgEl(doc, "path", { d });
    switch (name) {
      case "project":
        return svg([
          svgEl(doc, "rect", { x: 2, y: 2, width: 12, height: 5, rx: 1.5 }),
          svgEl(doc, "rect", { x: 2, y: 9, width: 12, height: 5, rx: 1.5 }),
          path("M4.6 4.5h.01M4.6 11.5h.01")
        ]);
      case "domain":
        return svg([
          svgEl(doc, "circle", { cx: 8, cy: 8, r: 6 }),
          path("M2 8h12M8 2c2 2 2 10 0 12M8 2c-2 2-2 10 0 12")
        ]);
      case "container":
        return svg([
          svgEl(doc, "path", { d: "M8 1.8 14 4.8v6.4L8 14.2 2 11.2V4.8Z" }),
          path("M2 4.8 8 7.8l6-3M8 7.8v6.4")
        ]);
      case "vm":
        return svg([
          svgEl(doc, "rect", { x: 1.5, y: 2.5, width: 13, height: 8.5, rx: 1.5 }),
          path("M6 14h4M8 11v3")
        ]);
      case "database":
        return svg([
          svgEl(doc, "ellipse", { cx: 8, cy: 3.8, rx: 5, ry: 2 }),
          path("M3 3.8v8.4c0 1.1 2.2 2 5 2s5-.9 5-2V3.8M3 8c0 1.1 2.2 2 5 2s5-.9 5-2")
        ]);
      case "machine":
        return svg([
          svgEl(doc, "rect", { x: 2, y: 2, width: 12, height: 5, rx: 1.5 }),
          svgEl(doc, "rect", { x: 2, y: 9, width: 12, height: 5, rx: 1.5 }),
          path("M4.6 4.5h.01M4.6 11.5h.01")
        ]);
      case "storage":
        return svg([
          svgEl(doc, "ellipse", { cx: 8, cy: 4, rx: 5.5, ry: 2.2 }),
          path("M2.5 4v8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2V4M2.5 8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2")
        ]);
      case "phone":
        return svg([
          svgEl(doc, "rect", { x: 4.5, y: 1.5, width: 7, height: 13, rx: 1.8 }),
          path("M7 12.2h2")
        ]);
      case "clock":
        return svg([
          svgEl(doc, "circle", { cx: 8, cy: 8, r: 6 }),
          path("M8 4.5V8l2.3 1.4")
        ]);
      case "check":
        return svg([path("m3.5 8.5 3 3 6-7")]);
      case "plus":
        return svg([path("M8 2.5v11M2.5 8h11")]);
      case "shield":
        return svg([
          path("M8 1.8 13.2 4v4c0 3-2.3 5.2-5.2 6.2C5.1 13.2 2.8 11 2.8 8V4Z"),
          path("m5.8 8 1.6 1.6 2.8-3")
        ]);
      case "send":
        return svg([path("M3 8h10M9 4l4 4-4 4")]);
      case "spark":
        return svg([path("M8 1.5C8.6 5.4 10.6 7.4 14.5 8 10.6 8.6 8.6 10.6 8 14.5 7.4 10.6 5.4 8.6 1.5 8 5.4 7.4 7.4 5.4 8 1.5Z")]);
      default:
        return svg([svgEl(doc, "circle", { cx: 8, cy: 8, r: 5 })]);
    }
  }
  function iconBox(doc, name, className = "") {
    const box = el(doc, "span", `arca-icon ${className}`.trim());
    box.append(icon(doc, name));
    return box;
  }
  function titleBlock(doc, title, subtitle) {
    const box = el(doc, "span", "arca-nt");
    box.append(el(doc, "strong", "arca-nn", title), el(doc, "span", "arca-nk", subtitle));
    return box;
  }
  function cap(doc, value) {
    return el(doc, "span", "arca-cap", value);
  }
  function brandMark(doc) {
    const svg = svgEl(doc, "svg", { viewBox: "0 0 100 100", width: 26, height: 26, "aria-hidden": "true", class: "arca-mark" });
    svg.append(
      svgEl(doc, "clipPath", { id: "arca-mark-clip" }, [svgEl(doc, "circle", { cx: 48, cy: 50, r: 40 })]),
      svgEl(doc, "circle", { cx: 48, cy: 50, r: 40, fill: "#D97757" }),
      svgEl(doc, "g", {
        "clip-path": "url(#arca-mark-clip)",
        stroke: "#B0543A",
        "stroke-width": 6.5,
        fill: "none",
        "stroke-linecap": "round"
      }, [
        svgEl(doc, "path", { d: "M26 8 Q12 50 26 92" }),
        svgEl(doc, "path", { d: "M44 6 Q34 50 44 94" }),
        svgEl(doc, "path", { d: "M62 8 Q56 50 62 92" })
      ]),
      svgEl(doc, "path", { d: "M70 66 C80 60 94 66 96 78 C97 84 92 88 86 87 L70 84 C65 79 65 71 70 66 Z", fill: "#F0B48E" }),
      svgEl(doc, "circle", { cx: 81.5, cy: 73, r: 3.8, fill: "#2A1A15" }),
      svgEl(doc, "circle", { cx: 95, cy: 80, r: 2.3, fill: "#2A1A15" })
    );
    return svg;
  }
  function meter(doc, label, value, percent, emptyText) {
    const row = el(doc, "div", "arca-meter");
    row.append(el(doc, "span", "arca-meter-label", label));
    const bar = svgEl(doc, "svg", {
      class: "arca-meter-bar",
      viewBox: "0 0 100 6",
      preserveAspectRatio: "none",
      "aria-hidden": "true"
    }, [
      svgEl(doc, "rect", { class: "arca-meter-track", width: 100, height: 6, rx: 3 }),
      svgEl(doc, "rect", { class: "arca-meter-fill", width: percent == null ? 0 : Math.max(0, Math.min(100, percent)), height: 6, rx: 3 })
    ]);
    const valueNode = el(doc, "span", "arca-meter-value mono", value || emptyText || "");
    row.append(bar, valueNode);
    return row;
  }
  function machineCard(doc, machine) {
    const card = el(doc, "article", "arca-machine");
    card.dataset.host = machine.name;
    card.dataset.online = String(machine.online);
    const head = el(doc, "div", "arca-machine-head");
    head.append(el(doc, "strong", "arca-machine-name", machine.name), statusLine(doc, machine.tone, machine.status));
    card.append(head);
    const meters = el(doc, "div", "arca-meters");
    meters.append(
      meter(doc, "CPU", machine.cpuLabel, machine.cpu?.usedPct, "No metrics"),
      meter(doc, "Mem", machine.memoryLabel, machine.memory?.usedPct, "No metrics")
    );
    card.append(meters);
    const foot = el(doc, "div", "arca-machine-foot");
    foot.append(el(
      doc,
      "span",
      "arca-machine-instances",
      machine.instanceCount === null ? "Instances unknown" : count2(machine.instanceCount, "instance")
    ));
    if (machine.error && !machine.metricsAt) foot.append(el(doc, "span", "arca-machine-warn", "Partial data"));
    card.append(foot);
    return card;
  }
  function placeholderCard(doc, name, body) {
    const card = el(doc, "article", "arca-machine arca-placeholder");
    card.setAttribute("aria-label", `${name} placeholder`);
    const head = el(doc, "div", "arca-machine-head");
    head.append(el(doc, "strong", "arca-machine-name", name), el(doc, "span", "arca-soon", "Soon"));
    card.append(head, iconBox(doc, name === "Storage" ? "storage" : "phone", "arca-placeholder-icon"), el(doc, "p", "arca-placeholder-body", body));
    return card;
  }
  function emptyState(doc, title, body) {
    const box = el(doc, "div", "arca-empty-state");
    box.append(el(doc, "b", "", title), el(doc, "span", "", body));
    return box;
  }
  function cardShell(doc, kind, x, y, width, height) {
    const node = el(doc, "article", `arca-node arca-node-${kind}`);
    node.dataset.kind = kind;
    node.style.left = `${x}px`;
    node.style.top = `${y}px`;
    node.style.width = `${width}px`;
    node.style.height = `${height}px`;
    return node;
  }
  function fact(doc, label, value) {
    const box = el(doc, "div", "arca-stat");
    box.append(el(doc, "b", "", value), el(doc, "span", "", label));
    return box;
  }
  function hostTag(doc, host) {
    const tag = el(doc, "span", "arca-host-tag");
    tag.append(icon(doc, "machine"), el(doc, "span", "", `runs on ${host}`));
    return tag;
  }
  function projectCard(doc, project, x, y, width, height) {
    const node = cardShell(doc, "project", x, y, width, height);
    node.dataset.project = project.name;
    const head = el(doc, "div", "arca-nh");
    head.append(iconBox(doc, "project"), titleBlock(
      doc,
      project.name,
      project.kind === "owner" ? "Project" : `Incus project ${project.rawName} \xB7 ${project.host}`
    ));
    node.append(head);
    const facts = el(doc, "div", "arca-facts");
    facts.append(
      fact(doc, count2(project.cards.length, "thing"), project.cards.length),
      fact(doc, count2(project.hosts.length, "host"), project.hosts.length)
    );
    node.append(facts);
    if (project.missing) node.append(el(doc, "p", "arca-card-note", `${count2(project.missing, "configured member")} not found on a machine`));
    const foot = el(doc, "div", "arca-nf");
    foot.append(dot(doc, project.tone), el(doc, "span", "", project.status), el(doc, "span", "arca-nf-right", "self-hosted"));
    node.append(foot);
    return node;
  }
  function domainCard(doc, project, x, y, width, height) {
    const node = cardShell(doc, "domain", x, y, width, height);
    const head = el(doc, "div", "arca-nh");
    head.append(iconBox(doc, "domain"), titleBlock(doc, "Domain", "Project setting"));
    node.append(head);
    node.append(el(doc, "div", "arca-domain-name mono", project.domain));
    node.append(el(doc, "p", "arca-card-note", "Set on this project; Arca does not manage DNS in this canvas."));
    const foot = el(doc, "div", "arca-nf");
    foot.append(dot(doc, "ok"), el(doc, "span", "", "Configured"), el(doc, "span", "arca-nf-right", "config"));
    node.append(foot);
    return node;
  }
  function instanceCard2(doc, card, x, y, width, height) {
    const node = cardShell(doc, "instance", x, y, width, height);
    node.dataset.host = card.host;
    node.dataset.status = card.status;
    const head = el(doc, "div", "arca-nh");
    head.append(iconBox(doc, card.type === "VM" ? "vm" : "container"), titleBlock(doc, card.name, card.type));
    node.append(head);
    const tags = el(doc, "div", "arca-card-tags");
    tags.append(hostTag(doc, card.host));
    if (card.url) {
      const link = el(doc, "a", "arca-card-link", "Open");
      link.href = card.url;
      link.target = "_blank";
      link.rel = "noopener";
      tags.append(link);
    }
    node.append(tags);
    const foot = el(doc, "div", "arca-nf");
    foot.append(dot(doc, card.tone), el(doc, "span", "", card.status), el(doc, "span", "arca-nf-right", card.owner));
    node.append(foot);
    return node;
  }
  function databaseCard2(doc, card, x, y, width, height) {
    const node = cardShell(doc, "database", x, y, width, height);
    node.dataset.host = card.host;
    const head = el(doc, "div", "arca-nh");
    head.append(iconBox(doc, "database"), titleBlock(doc, card.name, card.type));
    node.append(head);
    const tags = el(doc, "div", "arca-card-tags");
    tags.append(hostTag(doc, card.host));
    if (card.address) tags.append(el(doc, "span", "arca-prov", `${card.address}${card.port === null ? "" : `:${card.port}`}`));
    if (card.links.length) tags.append(el(doc, "span", "arca-prov", count2(card.links.length, "link")));
    if (card.url) {
      const link = el(doc, "a", "arca-card-link", "Open");
      link.href = card.url;
      link.target = "_blank";
      link.rel = "noopener";
      tags.append(link);
    }
    node.append(tags);
    const foot = el(doc, "div", "arca-nf");
    foot.append(dot(doc, card.tone), el(doc, "span", "", card.status), el(doc, "span", "arca-nf-right", `via ${card.instance}`));
    node.append(foot);
    return node;
  }
  var DOMAIN_W = 230;
  var HUB_W = 300;
  var CARD_W = 350;
  var CARD_H = 118;
  var GAP = 12;
  var TOP = 30;
  var WORLD_W = 1e3;
  function wire(doc, from, to, edgeKind) {
    const group = svgEl(doc, "g", { class: "arca-wire" });
    group.dataset.edge = edgeKind;
    const d = `M ${from.x} ${from.y} C ${from.x + 42} ${from.y} ${to.x - 42} ${to.y} ${to.x} ${to.y}`;
    group.append(
      svgEl(doc, "path", { class: "arca-wire-base", d }),
      svgEl(doc, "path", { class: "arca-wire-flow", d })
    );
    return group;
  }
  function buildWorld(doc, project) {
    const hasDomain = Boolean(project.domain);
    const hubX = hasDomain ? 270 : 0;
    const cardsX = 630;
    const rows = project.cards.length;
    const height = Math.max(460, TOP + rows * (CARD_H + GAP) - GAP + TOP);
    const world = el(doc, "div", "arca-world");
    world.style.width = `${WORLD_W}px`;
    world.style.height = `${height}px`;
    const wires = svgEl(doc, "svg", {
      class: "arca-wires",
      viewBox: `0 0 ${WORLD_W} ${height}`,
      width: WORLD_W,
      height,
      "aria-hidden": "true"
    });
    const hubY = TOP;
    if (hasDomain) {
      wires.append(wire(doc, { x: DOMAIN_W, y: hubY + 88 }, { x: hubX, y: hubY + 88 }, "domain-project"));
    }
    const cardY = (index) => TOP + index * (CARD_H + GAP);
    for (let index = 0; index < rows; index++) {
      wires.append(wire(doc, { x: hubX + HUB_W, y: hubY + 88 }, { x: cardsX, y: cardY(index) + CARD_H / 2 }, "project-card"));
    }
    world.append(wires);
    if (hasDomain) {
      world.append(domainCard(doc, project, 0, hubY, DOMAIN_W, 176));
    }
    world.append(projectCard(doc, project, hubX, hubY, HUB_W, 176));
    project.cards.forEach((card, index) => {
      const y = cardY(index);
      world.append(card.kind === "database" ? databaseCard2(doc, card, cardsX, y, CARD_W, CARD_H) : instanceCard2(doc, card, cardsX, y, CARD_W, CARD_H));
    });
    if (!project.cards.length) {
      const empty = cardShell(doc, "empty", cardsX, TOP, CARD_W, 118);
      empty.append(emptyState(doc, "No live things in this project", "Configured members that are not on a machine stay hidden."));
      world.append(empty);
    }
    return world;
  }
  function toolState(tone) {
    if (tone === "bad") return "bad";
    if (tone === "warn") return "wait";
    return "done";
  }
  function toolCard(doc, record, now) {
    const card = el(doc, "article", "arca-tool");
    card.dataset.state = toolState(record.tone);
    const head = el(doc, "div", "arca-tool-head");
    const ic = el(doc, "span", "arca-tool-icon");
    ic.append(record.tone === "bad" ? icon(doc, "shield") : icon(doc, "check"));
    head.append(ic, el(doc, "span", "arca-tool-name mono", record.entry));
    const age = relativeAge(record.at, now);
    head.append(el(doc, "span", "arca-tool-meta", age ? `${record.host} \xB7 ${age}` : record.host));
    card.append(head);
    card.append(el(doc, "p", "arca-tool-text", `${record.target ? `${record.target} - ` : ""}${record.state}`));
    if (record.plugin) {
      const links = el(doc, "div", "arca-tool-links");
      links.append(el(doc, "span", "arca-prov", record.plugin));
      card.append(links);
    }
    return card;
  }
  function stepCard(doc, record, now) {
    const card = el(doc, "div", "arca-step");
    card.dataset.state = toolState(record.tone);
    const ic = el(doc, "span", "arca-step-icon");
    ic.append(icon(doc, record.tone === "bad" ? "shield" : "check"));
    const body = el(doc, "div", "arca-step-body");
    body.append(el(doc, "p", "arca-step-text", `${record.entry}${record.target ? ` \xB7 ${record.target}` : ""}`));
    const age = relativeAge(record.at, now);
    body.append(el(doc, "span", "arca-step-meta", `${record.host} \xB7 ${record.state}${age ? ` \xB7 ${age}` : ""}`));
    card.append(ic, body);
    return card;
  }
  function approvalCard(doc, plans, url) {
    const card = el(doc, "article", "arca-card arca-card-warn arca-approval");
    const head = el(doc, "div", "arca-card-head");
    head.append(el(doc, "span", "arca-badge wait", "Waiting for approval"));
    head.append(el(doc, "span", "arca-card-meta", count2(plans, "plan")));
    card.append(head);
    card.append(el(doc, "p", "arca-card-text", `${count2(plans, "Arca plan")} ${plans === 1 ? "needs" : "need"} your approval before it can run.`));
    if (url) {
      const link = el(doc, "a", "arca-btn arca-btn-ink", "Review approvals");
      link.href = url;
      card.append(link);
    } else {
      card.append(el(doc, "span", "arca-card-note", "Approval page is not configured."));
    }
    return card;
  }
  function createCanvas(root2) {
    const doc = root2.ownerDocument;
    const shell = el(doc, "div", "arca-canvas");
    const topbar = el(doc, "header", "arca-topbar");
    const brand = el(doc, "div", "arca-brand");
    brand.append(brandMark(doc), el(doc, "strong", "arca-brand-name", "Arca"), el(doc, "span", "arca-slash", "/"));
    const picker = el(doc, "select", "arca-project-select");
    picker.setAttribute("aria-label", "Project");
    const pickerWrap = el(doc, "label", "arca-picker");
    pickerWrap.append(picker);
    const chevron = el(doc, "span", "arca-picker-chevron", "\u2304");
    chevron.setAttribute("aria-hidden", "true");
    pickerWrap.append(chevron);
    brand.append(pickerWrap);
    const modes = el(doc, "div", "arca-modes");
    modes.setAttribute("role", "group");
    modes.setAttribute("aria-label", "Where it runs");
    const arcaMode = el(doc, "button", "arca-mode");
    arcaMode.type = "button";
    arcaMode.setAttribute("aria-pressed", "true");
    arcaMode.append(el(doc, "i", "arca-mode-swatch arca-mode-self"), el(doc, "span", "", "Arca"), el(doc, "small", "", "self-hosted"));
    const cloudMode = el(doc, "button", "arca-mode");
    cloudMode.type = "button";
    cloudMode.disabled = true;
    cloudMode.setAttribute("aria-pressed", "false");
    cloudMode.title = "Arca Cloud is coming soon";
    cloudMode.append(el(doc, "i", "arca-mode-swatch arca-mode-cloud"), el(doc, "span", "", "Arca Cloud"), el(doc, "small", "arca-soon", "coming soon"));
    modes.append(arcaMode, cloudMode);
    const spacer = el(doc, "span", "arca-spacer");
    const summary = el(doc, "span", "arca-summary");
    const clock = el(doc, "span", "arca-clock-chip mono");
    const readOnly = el(doc, "span", "arca-readonly", "Read only");
    topbar.append(brand, modes, spacer, summary, clock, readOnly);
    const main = el(doc, "div", "arca-main");
    const left = el(doc, "div", "arca-left");
    const machines = el(doc, "section", "arca-machines");
    machines.setAttribute("aria-label", "Machines");
    const machinesHead = el(doc, "div", "arca-machines-head");
    const machinesMeta = el(doc, "span", "arca-machines-meta");
    machinesHead.append(cap(doc, "Machines"), machinesMeta);
    const machinesRow = el(doc, "div", "arca-machines-row");
    machines.append(machinesHead, machinesRow);
    const stage = el(doc, "section", "arca-stage");
    stage.setAttribute("aria-label", "Project canvas");
    const canvasHead = el(doc, "div", "arca-canvas-head");
    const canvasTitle = el(doc, "h1", "arca-canvas-title");
    const canvasChips = el(doc, "div", "arca-canvas-chips");
    canvasHead.append(canvasTitle, canvasChips);
    const scroller = el(doc, "div", "arca-scroller");
    const worldHost = el(doc, "div", "arca-world-host");
    scroller.append(worldHost);
    stage.append(canvasHead, scroller);
    left.append(machines, stage);
    const panel = el(doc, "aside", "arca-panel");
    panel.setAttribute("aria-label", "Doctor");
    const panelHead = el(doc, "div", "arca-panel-head");
    const orb = el(doc, "span", "arca-orb");
    const panelTitles = el(doc, "div", "arca-panel-titles");
    const panelName = el(doc, "strong", "arca-panel-name", "Doctor");
    const panelSub = el(doc, "span", "arca-panel-sub");
    panelTitles.append(panelName, panelSub);
    const tabs = el(doc, "div", "arca-tabs");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "Doctor view");
    const chatTab = el(doc, "button", "arca-tab", "Chat");
    chatTab.type = "button";
    chatTab.id = "arca-tab-chat";
    chatTab.setAttribute("role", "tab");
    chatTab.setAttribute("aria-selected", "true");
    chatTab.setAttribute("aria-controls", "arca-chat-panel");
    const stepsTab = el(doc, "button", "arca-tab", "Steps");
    stepsTab.type = "button";
    stepsTab.id = "arca-tab-steps";
    stepsTab.setAttribute("role", "tab");
    stepsTab.setAttribute("aria-selected", "false");
    stepsTab.setAttribute("aria-controls", "arca-steps-panel");
    tabs.append(chatTab, stepsTab);
    panelHead.append(orb, panelTitles, tabs);
    const chatScroll = el(doc, "div", "arca-panel-scroll arca-chat-view");
    chatScroll.id = "arca-chat-panel";
    chatScroll.setAttribute("role", "tabpanel");
    chatScroll.setAttribute("aria-labelledby", "arca-tab-chat");
    const thread = el(doc, "div", "arca-thread");
    chatScroll.append(thread);
    const stepsScroll = el(doc, "div", "arca-panel-scroll arca-steps-view");
    stepsScroll.id = "arca-steps-panel";
    stepsScroll.setAttribute("role", "tabpanel");
    stepsScroll.setAttribute("aria-labelledby", "arca-tab-steps");
    stepsScroll.hidden = true;
    const steps = el(doc, "div", "arca-steps");
    stepsScroll.append(steps);
    const composer = el(doc, "div", "arca-composer");
    const composerBox = el(doc, "div", "arca-composer-box");
    const composerRow = el(doc, "div", "arca-composer-row");
    const input = el(doc, "input", "arca-composer-input");
    input.type = "text";
    input.readOnly = true;
    input.id = "arca-chat-input";
    input.placeholder = "Chat is coming in phase 2";
    input.setAttribute("aria-label", "Message the Doctor");
    composerRow.append(input);
    const composerBar = el(doc, "div", "arca-composer-bar");
    const plus = el(doc, "button", "arca-icon-button");
    plus.type = "button";
    plus.setAttribute("aria-label", "Add");
    plus.setAttribute("aria-disabled", "true");
    plus.append(icon(doc, "plus"));
    const perm = el(doc, "span", "arca-perm");
    perm.append(icon(doc, "shield"), el(doc, "span", "", "Approval required"));
    const barSpacer = el(doc, "span", "arca-spacer");
    const model = el(doc, "span", "arca-model", "Doctor");
    const send = el(doc, "button", "arca-send");
    send.type = "button";
    send.setAttribute("aria-label", "Send");
    send.setAttribute("aria-disabled", "true");
    send.append(icon(doc, "send"));
    composerBar.append(plus, perm, barSpacer, model, send);
    composerBox.append(composerRow, composerBar);
    const composerNote = el(doc, "p", "arca-composer-note", "The composer is inert in this delivery.");
    composer.append(composerBox, composerNote);
    panel.append(panelHead, chatScroll, stepsScroll, composer);
    main.append(left, panel);
    shell.append(topbar, main);
    root2.replaceChildren(shell);
    const state = {
      envelope: {},
      dashboard: null,
      project: null,
      tab: "chat",
      signatures: { machines: "", canvas: "", panel: "", picker: "" }
    };
    let clockTimer;
    const tick = () => {
      try {
        clock.textContent = new Intl.DateTimeFormat(void 0, {
          hour: "2-digit",
          minute: "2-digit",
          hourCycle: "h23"
        }).format(/* @__PURE__ */ new Date());
      } catch {
        clock.textContent = "";
      }
    };
    tick();
    clockTimer = setInterval(tick, 1e3);
    const setTab = (name) => {
      state.tab = name;
      chatTab.setAttribute("aria-selected", String(name === "chat"));
      stepsTab.setAttribute("aria-selected", String(name === "steps"));
      chatScroll.hidden = name !== "chat";
      stepsScroll.hidden = name !== "steps";
    };
    chatTab.addEventListener("click", () => setTab("chat"));
    stepsTab.addEventListener("click", () => setTab("steps"));
    const updatePicker = () => {
      const projects = state.dashboard?.projects ?? [];
      const signature = projects.map((project) => `${project.name}\0${project.kind}`).join("");
      if (signature !== state.signatures.picker) {
        state.signatures.picker = signature;
        const fragment = doc.createDocumentFragment();
        for (const [kind, label] of [["owner", "Your projects"], ["incus", "Machine projects"]]) {
          const rows = projects.filter((project) => project.kind === kind);
          if (!rows.length) continue;
          const group = doc.createElement("optgroup");
          group.label = label;
          for (const project of rows) {
            const option = doc.createElement("option");
            option.value = project.name;
            option.textContent = project.name;
            group.append(option);
          }
          fragment.append(group);
        }
        picker.replaceChildren(fragment);
      }
      picker.value = state.project?.name ?? "";
    };
    const updateSummary = () => {
      summary.textContent = state.dashboard?.summary ?? "";
      const online = (state.dashboard?.hosts ?? []).filter((machine) => machine.online).length;
      machinesMeta.textContent = `${online} online`;
    };
    const updateMachines = () => {
      const hosts = state.dashboard?.hosts ?? [];
      const signature = JSON.stringify(hosts.map(({ name, online, cpuLabel: cpuLabel2, memoryLabel: memoryLabel2, instanceCount, error, cpu, memory }) => ({ name, online, cpuLabel: cpuLabel2, memoryLabel: memoryLabel2, instanceCount, error, cpuPct: cpu?.usedPct, memoryPct: memory?.usedPct })));
      if (signature === state.signatures.machines) return;
      state.signatures.machines = signature;
      const nodes = hosts.map((machine) => machineCard(doc, machine));
      nodes.push(
        placeholderCard(doc, "Storage", "Room for a snapshot target on each machine."),
        placeholderCard(doc, "Phones", "Room for paired phones and desktops.")
      );
      machinesRow.replaceChildren(...nodes);
    };
    const updateCanvas = () => {
      const project = state.project;
      if (!project) {
        canvasTitle.textContent = "No projects";
        canvasChips.replaceChildren(el(doc, "span", "arca-chip", "Read only"));
        const empty = el(doc, "div", "arca-world-empty");
        empty.append(emptyState(doc, "No projects found", "No owner project or Incus project has live instances."));
        worldHost.replaceChildren(empty);
        state.signatures.canvas = "empty";
        return;
      }
      canvasTitle.textContent = project.name;
      const chips = [
        `${count2(project.cards.length, "thing")}`,
        `${count2(project.hosts.length, "host")}`
      ];
      if (project.domain) chips.push(project.domain);
      if (project.kind === "incus") chips.push(`${project.host} \xB7 ${project.rawName}`);
      canvasChips.replaceChildren(...chips.map((value) => el(doc, "span", "arca-chip", value)));
      const signature = JSON.stringify({
        name: project.name,
        domain: project.domain,
        kind: project.kind,
        host: project.host,
        status: project.status,
        cards: project.cards,
        missing: project.missing
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
      panelSub.textContent = project ? `Recent activity \xB7 ${project.name}` : "Recent activity";
      const dashboard = state.dashboard;
      const journal = list2(dashboard?.journal);
      const plans = dashboard?.plans ?? 0;
      const url = approvalUrl(state.envelope.approvalUrl);
      const signature = JSON.stringify({ journal, plans, url, project: project?.name });
      if (signature === state.signatures.panel) return;
      state.signatures.panel = signature;
      const now = Date.now();
      const chatScrollTop = chatScroll.scrollTop;
      const stepsScrollTop = stepsScroll.scrollTop;
      thread.replaceChildren();
      steps.replaceChildren();
      thread.append(el(
        doc,
        "div",
        "arca-thread-head",
        `Recent Arca activity \xB7 ${count2(journal.length, "event")}`
      ));
      if (!journal.length) {
        const known = (state.envelope.hosts ?? []).some((host) => host.journalKnown);
        thread.append(emptyState(
          doc,
          known ? "No recent Arca activity" : "Activity journal unavailable",
          known ? "Arca has not applied a change recently." : "This host did not return a journal."
        ));
      }
      for (const record of journal) thread.append(toolCard(doc, record, now));
      if (plans > 0) thread.append(approvalCard(doc, plans, url));
      if (!journal.length) {
        steps.append(el(doc, "p", "arca-steps-empty", "No recent Arca steps."));
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
    picker.addEventListener("change", () => {
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
          history.replaceState(null, "", projectHash(fallback));
          updatePicker();
          updateCanvas();
          updatePanel();
        }
      }
    };
    window.addEventListener("hashchange", onHash);
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
        if (location.hash !== hash) history.replaceState(null, "", hash);
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
        window.removeEventListener("hashchange", onHash);
      }
    };
  }
  function mountCanvas(root2, { fetchSnapshot, pollMs = 5e3 } = {}) {
    const base = (root2.dataset?.arcaBase ?? "/arca-canvas").replace(/\/+$/, "") || "/arca-canvas";
    const load = fetchSnapshot ?? (() => fetch(`${base}/snapshot`, { credentials: "same-origin" }));
    const view = createCanvas(root2);
    let stopped = false;
    let timer;
    let current = {
      at: (/* @__PURE__ */ new Date()).toISOString(),
      error: null,
      approvalUrl: "",
      projects: [],
      defaultProject: "",
      hosts: []
    };
    view.update(current);
    const refresh = async () => {
      try {
        const response = await load();
        if (!response.ok) throw new Error("Snapshot unavailable");
        current = await response.json();
      } catch {
        current = { ...current, error: "Arca snapshot is unavailable. Retrying\u2026" };
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

  // src/page.js
  var root = document.getElementById("arca-root");
  mountCanvas(root, { pollMs: Number(root.dataset.pollMs) || 5e3 });
})();
