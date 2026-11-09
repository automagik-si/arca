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
  var INTERNAL_PROJECT = "arcak-svc";
  function proposalId(value) {
    const id = value && typeof value === "object" ? text(value.id) : "";
    return /^act-[0-9a-f]{12}$/.test(id) ? id : "";
  }
  function humanize(value) {
    const raw = text(value, "Unnamed");
    return raw.replace(/[_-]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
  }
  function projectLabel(value) {
    const id = text(value, "Unnamed project");
    if (id === "default") return "Default";
    if (id === "bench") return "Bench";
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
    if (["running", "ready", "active", "started", "ok", "up", "reachable", "applied", "committed", "confirmed"].includes(value)) return "ok";
    if (["stopped", "exited", "off", "disabled", "retained", "reverted"].includes(value)) return "neutral";
    if (["error", "failed", "unreachable", "unavailable", "down", "missing", "rejected"].includes(value)) return "bad";
    return value ? "warn" : "unknown";
  }
  function ownerLabel(owner) {
    const value = text(owner?.class ?? owner, "unknown");
    return ["kernel", "retained"].includes(value) ? "Managed by Arca" : "External";
  }
  function finite(value) {
    if (value === null || value === void 0 || value === "") return null;
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
  function cpuCoresKnown(cpu) {
    const cores = finite(cpu?.cores);
    return cores !== null && cores > 0 && (typeof cpu?.coresKnown === "boolean" ? cpu.coresKnown : true);
  }
  function cpuCoreCount(cpu) {
    return cpuCoresKnown(cpu) ? finite(cpu.cores) : 0;
  }
  function hostCpu(host = {}) {
    const cpu = host.metrics?.latest?.cpu;
    const cores = finite(cpu?.cores);
    const coresKnown = cpuCoresKnown(cpu);
    const usedPct = finite(cpu?.usedPct);
    const reason = typeof cpu?.reason === "string" ? cpu.reason.trim() : "";
    if (cores === null && usedPct === null && !reason) return null;
    const percent = coresKnown && usedPct !== null ? clampPercent(usedPct) : null;
    return {
      cores,
      coresKnown,
      usedPct: percent,
      usedCores: percent !== null && cores !== null ? percent / 100 * cores : null,
      reason
    };
  }
  function hostMemory(host = {}) {
    const memory = host.metrics?.latest?.memory;
    const totalBytes = finite(memory?.totalBytes);
    const availableBytes = finite(memory?.availableBytes);
    const reportedUsed = finite(memory?.usedBytes);
    const reason = typeof memory?.reason === "string" ? memory.reason.trim() : "";
    if (totalBytes === null && availableBytes === null && reportedUsed === null && !reason) return null;
    const derivedUsed = totalBytes !== null && availableBytes !== null ? Math.max(0, totalBytes - availableBytes) : null;
    const usedBytes = reportedUsed ?? derivedUsed;
    return {
      totalBytes,
      availableBytes,
      usedBytes,
      usedPct: !reason && usedBytes !== null && totalBytes !== null && totalBytes > 0 ? clampPercent(usedBytes / totalBytes * 100) : null,
      reason
    };
  }
  function cpuLabel(cpu) {
    if (!cpu) return "";
    const cores = cpuCoreCount(cpu);
    if (!cores) return "cores n/a";
    const usedPct = finite(cpu.usedPct);
    if (usedPct === null) return `n/a / ${count(cores, "core")}`;
    const usedCores = finite(cpu.usedCores);
    if (usedCores !== null) return `${formatNumber(usedCores)} / ${count(cores, "core")}`;
    return `${formatNumber(usedPct)}% used`;
  }
  function memoryLabel(memory) {
    if (!memory) return "";
    if (memory.reason || memory.totalBytes === null) return "n/a";
    if (memory.usedBytes !== null) return `${formatGb(memory.usedBytes)} / ${formatGb(memory.totalBytes)} GB`;
    return `${formatGb(memory.totalBytes)} GB total`;
  }
  function fleetSummary(hosts = []) {
    const rows = list(hosts);
    const parts = [count(rows.length, "machine")];
    const cores = rows.reduce((sum, host) => sum + cpuCoreCount(hostCpu(host)), 0);
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
  function aclLabel(value) {
    return text(value).replace(/\barcak-svc-svc-([a-z0-9-]+)\b/gi, (_, name) => `${humanize(name)} firewall`).replace(/arcak-svc/gi, "Arca service");
  }
  function targetLabel(target) {
    if (typeof target === "string") return aclLabel(target);
    if (!target || typeof target !== "object") return "";
    const kind = text(target.kind);
    const name = aclLabel(target.name);
    if (kind && name) return `${kind} ${name}`;
    if (name) return name;
    if (kind && target.project) {
      const project = text(target.project);
      return `${kind} in ${project === INTERNAL_PROJECT ? "Arca service" : projectLabel(project)}`;
    }
    return kind;
  }
  function rollup(cards, unknown = 0) {
    if (!cards.length) return unknown > 0 ? { tone: "warn", text: "Host data unavailable" } : { tone: "neutral", text: "Nothing live yet" };
    const tones = cards.map((card) => card.tone);
    const bad = tones.filter((tone2) => tone2 === "bad").length;
    const warn = tones.filter((tone2) => tone2 === "warn").length;
    const running = cards.filter((card) => card.status.toLowerCase() === "running").length;
    const ready = cards.filter((card) => card.status.toLowerCase() === "ready").length;
    const stopped = cards.filter((card) => card.status.toLowerCase() === "stopped").length;
    const tone = bad ? "bad" : warn || unknown ? "warn" : tones.every((value) => value === "neutral") ? "neutral" : "ok";
    const parts = [];
    if (running) parts.push(`${running} running`);
    if (ready) parts.push(`${ready} ready`);
    if (stopped) parts.push(`${stopped} stopped`);
    if (bad) parts.push(`${bad} unavailable`);
    else if (warn) parts.push(`${warn} unknown`);
    if (unknown) parts.push(`${unknown} unknown`);
    return { tone, text: parts.join(" \xB7 ") || "Live" };
  }
  function buildMachine(host) {
    const cpu = hostCpu(host);
    const memory = hostMemory(host);
    const online = isHostOnline(host);
    return {
      name: text(host.name, text(host.nodeName, "Machine")),
      label: text(host.name, text(host.nodeName, "Machine")),
      address: text(host.address).trim(),
      online,
      status: online ? "Online" : "Offline",
      tone: online ? "ok" : "bad",
      cpu,
      cpuLabel: cpuLabel(cpu),
      // The reason is the CPU n/a tooltip: it only applies while no load is shown.
      cpuReason: cpu?.usedPct === null ? cpu?.reason ?? "" : "",
      memory,
      memoryLabel: memoryLabel(memory),
      memoryReason: memory?.reason ?? "",
      instanceCount: host.instancesKnown === false ? null : list(host.instances).length,
      instancesKnown: host.instancesKnown !== false,
      error: host.error ?? null,
      metricsAt: host.metrics?.latest?.at ?? null
    };
  }
  function serviceUrl(address, lanPort, state) {
    const host = text(address).trim();
    const port = finite(lanPort);
    if (text(state).toLowerCase() !== "running") return "";
    if (!host || port === null || port < 1 || port > 65535) return "";
    return `http://${host.includes(":") ? `[${host}]` : host}:${Math.trunc(port)}`;
  }
  function serviceTargets(hosts) {
    const targets = /* @__PURE__ */ new Map();
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
          url: serviceUrl(host.address, service.lanPort, service.state)
        });
      }
    }
    return targets;
  }
  function instanceCard(hostName, instance, services) {
    const status = text(instance.status, "Unknown");
    const service = services?.get(instanceKey(hostName, instance.project, instance.name)) ?? null;
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
      service,
      url: instanceUrl(instance) || service?.url || ""
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
        const instance = list(host?.instances).find((candidate) => text(candidate.project, "default") === text(member.incusProject) && text(candidate.name) === text(member.instance));
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
        missing,
        unknown
      });
    }
    for (const host of hosts) {
      const unlisted = list(host.instances).filter((instance) => text(instance.project, "default") === INTERNAL_PROJECT && !claimed.has(instanceKey(host.name, instance.project, instance.name))).sort((a, b) => byName(humanize(text(a.name)), humanize(text(b.name))));
      for (const instance of unlisted) {
        const cards = [instanceCard(host.name, instance, serviceCards)];
        const target = databaseCards.get(cards[0].key);
        if (target) cards.push(databaseCard(target.host.name, target.instance, target.database));
        const name = uniqueName(humanize(text(instance.name)), usedNames, host.name);
        usedNames.add(name);
        const roll = rollup(cards);
        serviceProjects.push({
          id: `service:${text(host.name)}/${text(instance.name)}`,
          kind: "service",
          name,
          label: name,
          rawName: "",
          host: text(host.name),
          domain: "",
          cards,
          hosts: [text(host.name)],
          status: roll.text,
          tone: roll.tone,
          missing: 0,
          unknown: 0
        });
      }
    }
    for (const host of hosts) {
      const projectIds = new Set(list(host.projects).map((value) => text(value)).filter(Boolean));
      for (const instance of list(host.instances)) projectIds.add(text(instance.project, "default"));
      for (const project of [...projectIds].sort(byName)) {
        if (project === INTERNAL_PROJECT) continue;
        const unlisted = list(host.instances).filter((instance) => text(instance.project, "default") === project && !claimed.has(instanceKey(host.name, instance.project, instance.name)));
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
          missing: 0,
          unknown: 0
        });
      }
    }
    const projects = [...ownerProjects, ...serviceProjects, ...fallbackProjects];
    const configuredDefault = text(envelope.defaultProject).trim();
    const defaultProject = projects.find((project) => project.name === configuredDefault) ?? projects.find((project) => project.kind === "owner") ?? projects.find((project) => project.kind === "service") ?? projects.find((project) => project.kind === "incus") ?? null;
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
  var APPROVAL_NAMESPACE = "arca-kernel-approve";
  var APPROVAL_DOMAIN = "arca-kernel-approval/v1";
  var PROPOSE_ACTIONS = /* @__PURE__ */ new Set([
    "service_create",
    "service_remove",
    "service_publish",
    "service_unpublish",
    "service_connect",
    "service_disconnect",
    "postgres_create"
  ]);
  var REFUSALS = Object.freeze({
    Expired: "This proposal ran out of time. Nothing changed.",
    Replayed: "This approval was already used. Reload the page to see what is still waiting.",
    NoSuchPlan: "This proposal is no longer waiting in Arca.",
    Untrusted: "Arca does not trust this passkey. Use the passkey you set up for Arca.",
    BadSignature: "Arca could not check the passkey's answer. Try again.",
    NoUserPresence: "Your device did not confirm it was you. Try again.",
    NoUserVerification: "Your device did not confirm it was you. Try again.",
    WrongOrigin: "This page's address does not match Arca's settings.",
    WrongRp: "This page's address does not match Arca's settings.",
    WrongHost: "This approval was made for a different Arca.",
    owner_approval: "Arca did not accept this approval. Try again from the card."
  });
  var text2 = (value, fallback = "") => value == null ? fallback : String(value);
  var list2 = (value) => Array.isArray(value) ? value : [];
  var count2 = (value, singular, plural = `${singular}s`) => `${value} ${value === 1 ? singular : plural}`;
  function b64(bytes) {
    let text3 = "";
    for (const byte of bytes) text3 += String.fromCharCode(byte);
    return btoa(text3);
  }
  function b64url(bytes) {
    return b64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  function fromB64url(value) {
    const text3 = String(value).replace(/-/g, "+").replace(/_/g, "/");
    const padded = text3 + "=".repeat((4 - text3.length % 4) % 4);
    const raw = atob(padded);
    return Uint8Array.from(raw, (character) => character.charCodeAt(0));
  }
  function expiryText(expiresAt, now = Date.now()) {
    const left = Number(expiresAt) - now;
    if (!Number.isFinite(left) || left <= 0) return "Expired";
    const seconds = Math.ceil(left / 1e3);
    const minutes = Math.floor(seconds / 60);
    return `${minutes}:${String(seconds % 60).padStart(2, "0")}`;
  }
  function plainRefusal(refusal, status) {
    if (typeof refusal === "string" && REFUSALS[refusal]) return REFUSALS[refusal];
    if (status === 401 || status === 403) return "Arca did not accept this approval.";
    if (status === 404) return "This proposal is no longer waiting in Arca.";
    if (status === 429) return "Arca is busy. Try again shortly.";
    return "Arca could not complete this just now. Try again in a moment.";
  }
  function plainError(error) {
    if (!error) return "Something went wrong. Try again.";
    if (error.name === "TypeError") return "Can't reach Arca. Check your connection, then try again.";
    if (error.name === "SecurityError") return "Passkeys can't be used at this page's address. Open Arca from its usual address.";
    if (error.name === "InvalidStateError") return "This device already has a different passkey. Try again.";
    const message = String(error.message || error).trim();
    return message ? `${message.charAt(0).toUpperCase()}${message.slice(1)}` : "Something went wrong. Try again.";
  }
  function plainResult(action, result) {
    const value = result && typeof result === "object" ? result : {};
    if (value.removed === true) return "The service was removed.";
    if (value.unpublished === true) return "The service is no longer published.";
    if (typeof value.url === "string" && value.url) return `The change is live at ${value.url}.`;
    if (Array.isArray(value.env)) return `The link is set up; ${count2(value.env.length, "environment variable")} updated.`;
    if (action === "postgres_create" && typeof value.name === "string") {
      return `The database ${value.name} is ${value.status === "running" ? "ready" : "being created"}.`;
    }
    if (typeof value.name === "string") {
      const state = typeof value.state === "string" && value.state ? ` and is ${value.state}` : "";
      return `The service ${value.name} was created${state}.`;
    }
    return "Arca finished the change.";
  }
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
  function meter(doc, label, value, percent, emptyText, title = "") {
    const row = el(doc, "div", "arca-meter");
    row.dataset.empty = String(percent == null);
    if (title) row.title = title;
    row.append(el(doc, "span", "arca-meter-label", label));
    if (percent != null) {
      const bar = svgEl(doc, "svg", {
        class: "arca-meter-bar",
        viewBox: "0 0 100 6",
        preserveAspectRatio: "none",
        "aria-hidden": "true"
      }, [
        svgEl(doc, "rect", { class: "arca-meter-track", width: 100, height: 6, rx: 3 }),
        svgEl(doc, "rect", { class: "arca-meter-fill", width: Math.max(0, Math.min(100, percent)), height: 6, rx: 3 })
      ]);
      row.append(bar);
    }
    const valueNode = el(doc, "span", "arca-meter-value mono", value || emptyText || "");
    row.append(valueNode);
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
      meter(doc, "CPU", machine.cpuLabel, machine.cpu?.usedPct, "No metrics", machine.cpuReason),
      meter(doc, "Mem", machine.memoryLabel, machine.memory?.usedPct, "No metrics", machine.memoryReason)
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
      project.kind === "owner" ? "Project" : project.kind === "service" ? `Service \xB7 ${project.host}` : `Incus project ${project.rawName} \xB7 ${project.host}`
    ));
    node.append(head);
    const facts = el(doc, "div", "arca-facts");
    facts.append(
      fact(doc, count2(project.cards.length, "thing"), project.cards.length),
      fact(doc, count2(project.hosts.length, "host"), project.hosts.length)
    );
    node.append(facts);
    if (project.missing) node.append(el(doc, "p", "arca-card-note", `${count2(project.missing, "configured member")} not found on a machine`));
    if (project.unknown) node.append(el(doc, "p", "arca-card-note", `${count2(project.unknown, "configured member")} unknown (host data unavailable)`));
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
      empty.append(project.unknown ? emptyState(doc, "Host data unavailable", "Configured members on a machine whose instance list could not be read stay hidden.") : emptyState(doc, "No live things in this project", "Configured members that are not on a machine stay hidden."));
      world.append(empty);
    }
    return world;
  }
  function toolState(tone) {
    if (tone === "bad") return "bad";
    if (tone === "warn") return "wait";
    return "done";
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
    const card = el(doc, "article", "arca-card arca-card-warn arca-approval arca-approval-delete");
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
  function sseBlocks(buffer) {
    const parts = buffer.split(/\r?\n\r?\n/);
    return { blocks: parts.slice(0, -1), rest: parts.at(-1) ?? "" };
  }
  function parseSseBlock(block) {
    let event = "message";
    const data = [];
    for (const line of block.split(/\r?\n/)) {
      if (!line || line.startsWith(":")) continue;
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
    }
    if (!data.length) return null;
    try {
      return { event, data: JSON.parse(data.join("\n")) };
    } catch {
      return { event, data: null };
    }
  }
  function chatLabel(value) {
    return text2(value).replace(/\barcak-svc-svc-([a-z0-9-]+)\b/gi, (_, name) => `${humanize(name)} firewall`).replace(/arcak-svc/gi, "Arca service");
  }
  function chatStatusText(status) {
    if (status === 401) return "Sign in to Arca to chat with the Doctor.";
    if (status === 403) return "This page is not allowed to chat with the Doctor.";
    if (status === 413) return "That message is too long for the Doctor.";
    if (status === 415) return "The Doctor could not read that message.";
    if (status === 429) return "The Doctor is busy with another answer. Try again shortly.";
    if (status === 503) return "The Doctor is not available right now.";
    return "The Doctor could not answer.";
  }
  function toolSummary(card) {
    if (card.state === "running") return card.host ? `Reading ${card.host}\u2026` : "Reading Arca\u2026";
    if (card.state === "error") return card.host ? `${card.host} \xB7 failed` : "Failed";
    const summary = typeof card.summary === "string" ? card.summary.trim() : "";
    if (summary && !summary.startsWith("{") && !summary.startsWith("[")) return chatLabel(summary.slice(0, 120));
    return card.host ? `Answered from ${card.host}` : "Answered from Arca";
  }
  function chatToolCard(doc, card) {
    const node = el(doc, "article", "arca-tool");
    node.dataset.state = card.state === "running" ? "wait" : card.state === "error" ? "bad" : "done";
    node.dataset.tool = card.name;
    const head = el(doc, "div", "arca-tool-head");
    const ic = el(doc, "span", "arca-tool-icon");
    ic.append(icon(doc, card.state === "error" ? "shield" : "check"));
    head.append(ic, el(doc, "span", "arca-tool-name mono", card.name));
    head.append(el(doc, "span", "arca-tool-meta", card.host ?? ""));
    node.append(head, el(doc, "p", "arca-tool-text", toolSummary(card)));
    return node;
  }
  function chatNotice(doc, kind, title, body) {
    if (kind === "cutoff") return el(doc, "p", "arca-thread-note arca-cutoff", body);
    const card = el(doc, "article", `arca-card arca-card-warn arca-${kind}`);
    card.append(el(doc, "p", "arca-card-text", title), el(doc, "p", "arca-card-text", body));
    return card;
  }
  function createCanvas(root2, { base = "/arca-canvas" } = {}) {
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
    input.id = "arca-chat-input";
    input.placeholder = "Ask the Doctor about your machines";
    input.setAttribute("aria-label", "Message the Doctor");
    input.setAttribute("autocomplete", "off");
    input.setAttribute("maxlength", "16000");
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
    const model = el(doc, "span", "arca-model", "deepseek-flash");
    const send = el(doc, "button", "arca-send");
    send.type = "button";
    send.setAttribute("aria-label", "Send");
    send.setAttribute("aria-disabled", "true");
    send.append(icon(doc, "send"));
    composerBar.append(plus, perm, barSpacer, model, send);
    composerBox.append(composerRow, composerBar);
    const composerNote = el(doc, "p", "arca-composer-note", "Read only \xB7 approve the Doctor's proposals with your passkey.");
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
      plans: 0,
      approvalUrl: "",
      signatures: { machines: "", canvas: "", panel: "", picker: "" }
    };
    const chat = {
      messages: [],
      streaming: false,
      error: "",
      loaded: false,
      controller: null,
      sessionId: null
    };
    const approvals = {
      plans: /* @__PURE__ */ new Map(),
      deletes: [],
      cards: /* @__PURE__ */ new Map(),
      passkeysTrusted: 0,
      loaded: false,
      error: "",
      fetching: false,
      credentialIds: null
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
    const updateSubtitle = () => {
      panelSub.textContent = state.tab === "chat" ? "Read only \xB7 deepseek-flash" : "Recent activity \xB7 all machines";
    };
    updateSubtitle();
    const setTab = (name) => {
      state.tab = name;
      chatTab.setAttribute("aria-selected", String(name === "chat"));
      stepsTab.setAttribute("aria-selected", String(name === "steps"));
      chatScroll.hidden = name !== "chat";
      stepsScroll.hidden = name !== "steps";
      updateSubtitle();
      if (name === "steps" && approvalConfigured()) void refreshApprovals();
    };
    chatTab.addEventListener("click", () => setTab("chat"));
    stepsTab.addEventListener("click", () => setTab("steps"));
    const updatePicker = () => {
      const projects = state.dashboard?.projects ?? [];
      const signature = projects.map((project) => `${project.name}\0${project.kind}`).join("");
      if (signature !== state.signatures.picker) {
        state.signatures.picker = signature;
        const fragment = doc.createDocumentFragment();
        for (const [kind, label] of [["owner", "Your projects"], ["service", "Services"], ["incus", "Machine projects"]]) {
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
      const signature = JSON.stringify(hosts.map(({ name, online, cpuLabel: cpuLabel2, cpuReason, memoryLabel: memoryLabel2, memoryReason, instanceCount, error, cpu, memory }) => ({ name, online, cpuLabel: cpuLabel2, cpuReason, memoryLabel: memoryLabel2, memoryReason, instanceCount, error, cpuPct: cpu?.usedPct, memoryPct: memory?.usedPct })));
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
      if (project.kind === "service") chips.push(`${project.host} \xB7 service`);
      if (project.kind === "incus") chips.push(`${project.host} \xB7 ${project.rawName}`);
      canvasChips.replaceChildren(...chips.map((value) => el(doc, "span", "arca-chip", value)));
      const signature = JSON.stringify({
        name: project.name,
        domain: project.domain,
        kind: project.kind,
        host: project.host,
        status: project.status,
        cards: project.cards,
        missing: project.missing,
        unknown: project.unknown
      });
      if (signature === state.signatures.canvas) return;
      state.signatures.canvas = signature;
      const scrollTop = scroller.scrollTop;
      const scrollLeft = scroller.scrollLeft;
      worldHost.replaceChildren(buildWorld(doc, project));
      scroller.scrollTop = scrollTop;
      scroller.scrollLeft = scrollLeft;
    };
    const renderChat = () => {
      thread.dataset.streaming = String(chat.streaming);
      thread.dataset.session = chat.session || "Doctor";
      const nearBottom = chatScroll.scrollHeight - chatScroll.scrollTop - chatScroll.clientHeight < 48;
      const scrollTop = chatScroll.scrollTop;
      thread.replaceChildren();
      const banner = el(doc, "div", "arca-chat-banner");
      const shield = el(doc, "span", "arca-chat-banner-icon");
      shield.append(icon(doc, "shield"));
      banner.append(shield, el(doc, "span", "", "Read only \xB7 the Doctor proposes, you approve with your passkey"));
      thread.append(banner);
      if (chat.error) {
        thread.append(chatNotice(doc, "error", "Doctor unavailable", chat.error));
      }
      if (!chat.messages.length && !chat.error) {
        const empty = emptyState(doc, "Ask the Doctor", "An assistant for this Arca deployment. It reads live hosts and services; a change waits for your approval on a card.");
        empty.classList.add("arca-chat-empty");
        thread.append(empty);
      }
      for (const message of chat.messages) {
        const node = el(doc, "article", `arca-msg arca-msg-${message.role}`);
        if (message.role === "assistant" && !message.text && !message.tools.length && chat.streaming) {
          node.append(el(doc, "p", "arca-msg-text arca-typing", "Reading Arca\u2026"));
        } else if (message.text) {
          node.append(el(doc, "p", "arca-msg-text", chatLabel(message.text)));
        }
        if (message.tools.length) {
          const tools = el(doc, "div", "arca-msg-tools");
          for (const tool of message.tools) tools.append(chatToolCard(doc, tool));
          node.append(tools);
        }
        const proposed = /* @__PURE__ */ new Set();
        for (const tool of message.tools) {
          const id = proposalId(tool.proposal);
          if (!id || proposed.has(id)) continue;
          proposed.add(id);
          const card = proposalCard(doc, id);
          if (card) node.append(card);
        }
        for (const note of message.notes ?? []) node.append(chatNotice(doc, note.kind, note.title, note.body));
        thread.append(node);
      }
      if (nearBottom) chatScroll.scrollTop = chatScroll.scrollHeight;
      else chatScroll.scrollTop = scrollTop;
    };
    const updateSend = () => {
      const ready = input.value.trim().length > 0 && !chat.streaming;
      send.setAttribute("aria-disabled", String(!ready));
    };
    const activeAssistant = () => chat.messages.at(-1)?.role === "assistant" ? chat.messages.at(-1) : null;
    const approvalConfigured = () => Boolean(approvalUrl(state.envelope.approvalUrl));
    function approvalCardFor(id) {
      const key = proposalId({ id });
      if (!key) return null;
      let card = approvals.cards.get(key);
      if (!card) {
        if (!approvals.plans.has(key)) return null;
        card = { plan: null, state: "waiting", reason: "", note: "", result: "", busy: "" };
        approvals.cards.set(key, card);
      }
      if (!card.plan && approvals.plans.has(key)) card.plan = approvals.plans.get(key);
      return card;
    }
    function approvalDetails(value) {
      if (!Array.isArray(value) || !value.length) return null;
      const rows = [];
      for (const row of value) {
        if (!row || typeof row !== "object" || typeof row.label !== "string") return null;
        if (typeof row.value !== "string" && typeof row.value !== "number" && typeof row.value !== "boolean") return null;
        rows.push({ label: row.label, value: String(row.value) });
      }
      return rows;
    }
    function parseApprovalPlans(body) {
      const plans = /* @__PURE__ */ new Map();
      const deletes = [];
      for (const row of Array.isArray(body?.plans) ? body.plans : []) {
        if (!row || typeof row !== "object") continue;
        if (row.kind === "action") {
          if (!PROPOSE_ACTIONS.has(row.action)) continue;
          const id = proposalId(row);
          const summary2 = typeof row.summary === "string" ? row.summary.trim() : "";
          const digest = typeof row.argsDigest === "string" && /^[0-9a-f]{64}$/.test(row.argsDigest) ? row.argsDigest : "";
          const details = approvalDetails(row.details);
          const expiresAt = Number(row.expiresAt);
          if (!id || !summary2 || !digest || !details || !Number.isFinite(expiresAt) || expiresAt <= 0) continue;
          plans.set(id, {
            kind: "action",
            id,
            action: row.action,
            summary: summary2,
            argsDigest: digest,
            details,
            expiresAt,
            statement: typeof row.statement === "string" ? row.statement : ""
          });
        } else if (typeof row.plan === "string" && /^[0-9a-f]{64}$/.test(row.plan)) {
          deletes.push({ id: row.plan, expiresAt: Number(row.expiresAt) || 0 });
        }
      }
      const trusted = Number(body?.passkeysTrusted);
      return { plans, deletes, passkeysTrusted: Number.isInteger(trusted) && trusted > 0 ? trusted : 0 };
    }
    async function readApprovalPlans() {
      const response = await fetch("/approve/plans", {
        credentials: "same-origin",
        cache: "no-store",
        headers: { Accept: "application/json" }
      });
      if (!response.ok) throw new Error(`The approval list answered ${response.status}.`);
      return parseApprovalPlans(await response.json());
    }
    async function refreshApprovals() {
      if (approvals.fetching) return;
      if (!approvalConfigured()) {
        approvals.loaded = true;
        approvals.error = "The approval page is not configured on this canvas.";
        updatePanel();
        return;
      }
      approvals.fetching = true;
      try {
        const next = await readApprovalPlans();
        const now = Date.now();
        for (const [id, plan] of next.plans) {
          const existing = approvals.cards.get(id);
          if (existing) existing.plan = plan;
        }
        for (const [id, card] of approvals.cards) {
          if (next.plans.has(id) || card.state !== "waiting") continue;
          if (card.plan && now >= card.plan.expiresAt) card.state = "expired";
          else {
            card.state = "failed";
            card.reason = "This proposal is no longer waiting in Arca.";
          }
        }
        approvals.plans = next.plans;
        approvals.deletes = next.deletes;
        approvals.passkeysTrusted = next.passkeysTrusted;
        approvals.error = "";
      } catch (error) {
        approvals.error = plainError(error);
      } finally {
        approvals.fetching = false;
        approvals.loaded = true;
        updatePanel();
      }
    }
    function expireApprovals(now = Date.now()) {
      let changed = false;
      for (const card of approvals.cards.values()) {
        if (card.state === "waiting" && card.plan && now >= card.plan.expiresAt) {
          card.state = "expired";
          card.busy = "";
          changed = true;
        }
      }
      return changed;
    }
    function updateExpiryLabels(now = Date.now()) {
      for (const node of root2.querySelectorAll(".arca-approval-expiry[data-expires-at]")) {
        node.textContent = `Expires in ${expiryText(Number(node.dataset.expiresAt), now)}`;
      }
    }
    function cardBadge(card) {
      if (card.busy === "declining") return ["wait", "Declining"];
      if (card.state === "approving") return ["wait", "Confirm passkey"];
      if (card.state === "done") return ["ok", "Done"];
      if (card.state === "declined") return ["", "Declined"];
      if (card.state === "expired") return ["", "Expired"];
      if (card.state === "failed") return ["bad", "Failed"];
      return ["wait", "Waiting for approval"];
    }
    function proposalCard(doc2, id) {
      const key = proposalId({ id });
      const card = approvalCardFor(key);
      if (!card || !card.plan) return null;
      const plan = card.plan;
      const article = el(doc2, "article", "arca-card arca-card-warn arca-approval arca-proposal");
      article.dataset.proposal = key;
      article.dataset.state = card.state;
      article.setAttribute("aria-label", `Approval: ${plan.summary}`);
      const head = el(doc2, "div", "arca-card-head");
      const [tone, label] = cardBadge(card);
      head.append(el(doc2, "span", `arca-badge${tone ? ` ${tone}` : ""}`, label));
      const meta = el(doc2, "span", "arca-card-meta");
      if (card.state === "waiting" || card.state === "approving") {
        const expiry = el(doc2, "time", "arca-approval-expiry", `Expires in ${expiryText(plan.expiresAt)}`);
        expiry.dateTime = new Date(plan.expiresAt).toISOString();
        expiry.title = new Date(plan.expiresAt).toLocaleString();
        expiry.dataset.expiresAt = String(plan.expiresAt);
        if (plan.expiresAt - Date.now() < 5 * 6e4) expiry.dataset.soon = "true";
        meta.append(expiry);
      }
      head.append(meta);
      article.append(head, el(doc2, "p", "arca-card-text arca-approval-summary", plan.summary));
      const facts = el(doc2, "dl", "arca-approval-facts");
      for (const row of plan.details) {
        const line = el(doc2, "div", "arca-approval-fact");
        line.dataset.detail = "true";
        line.append(el(doc2, "dt", "", row.label), el(doc2, "dd", "", row.value));
        facts.append(line);
      }
      const technical = (label2, value, name) => {
        const line = el(doc2, "div", "arca-approval-fact");
        line.dataset.technical = name;
        line.append(el(doc2, "dt", "", label2), el(doc2, "dd", "", value));
        facts.append(line);
      };
      technical("Action", plan.action, "action");
      technical("Target", plan.id, "target");
      technical("Fingerprint", plan.argsDigest.slice(0, 12), "digest");
      article.append(facts);
      if (card.state === "waiting") {
        const actions = el(doc2, "div", "arca-approval-actions");
        const approve = el(doc2, "button", "arca-btn arca-btn-ink arca-approve", "Approve");
        approve.type = "button";
        approve.dataset.approve = key;
        approve.setAttribute("aria-label", `Approve: ${plan.summary}`);
        approve.disabled = Boolean(card.busy) || !plan.statement || approvals.passkeysTrusted < 1;
        approve.addEventListener("click", () => {
          void approveProposal(key);
        });
        const decline = el(doc2, "button", "arca-btn arca-btn-quiet arca-decline", "Decline");
        decline.type = "button";
        decline.dataset.decline = key;
        decline.disabled = Boolean(card.busy) || !plan.statement;
        decline.addEventListener("click", () => {
          void declineProposal(key);
        });
        actions.append(approve, decline);
        article.append(actions);
        if (card.busy === "declining") article.append(el(doc2, "span", "arca-card-note", "Declining\u2026"));
        else if (card.note) article.append(el(doc2, "span", "arca-card-note", card.note));
        if (!plan.statement) article.append(el(doc2, "span", "arca-card-note", "Arca did not send the approval statement. Reload the page."));
        else if (approvals.passkeysTrusted < 1) article.append(el(doc2, "span", "arca-card-note", "No passkey is set up for this Arca."));
      } else if (card.state === "approving") {
        article.append(el(doc2, "p", "arca-approval-status", "Confirm with your passkey\u2026"));
      } else if (card.state === "done") {
        article.append(el(doc2, "p", "arca-approval-status", card.result || "Arca finished the change."));
      } else if (card.state === "declined") {
        article.append(el(doc2, "p", "arca-approval-status", "Declined. Nothing changed."));
      } else if (card.state === "expired") {
        article.append(el(doc2, "p", "arca-approval-status", "Expired. Nothing changed."));
      } else if (card.state === "failed") {
        article.append(el(doc2, "p", "arca-approval-status arca-approval-reason", card.reason || "The approval failed."));
      }
      return article;
    }
    async function loadCredentialIds() {
      if (approvals.credentialIds) return approvals.credentialIds;
      const response = await fetch("/approve", {
        credentials: "same-origin",
        cache: "no-store",
        headers: { Accept: "text/html" }
      });
      if (!response.ok) throw new Error("Passkeys are not available from this page.");
      const page = await response.text();
      const parsed = new DOMParser().parseFromString(page, "text/html");
      let ids = [];
      try {
        ids = JSON.parse(parsed.body?.dataset?.credentialIds || "[]");
      } catch {
        ids = [];
      }
      if (!Array.isArray(ids)) ids = [];
      approvals.credentialIds = ids.filter((value) => typeof value === "string" && value).map((value) => ({ type: "public-key", id: fromB64url(value) }));
      return approvals.credentialIds;
    }
    async function passkeyProof(statement) {
      if (!navigator.credentials || typeof navigator.credentials.get !== "function") {
        throw new Error("This browser cannot use passkeys.");
      }
      if (typeof crypto?.subtle?.digest !== "function") {
        throw new Error("Passkeys can't be used at this page's address.");
      }
      const allowCredentials = await loadCredentialIds();
      if (!allowCredentials.length) throw new Error("No passkey is set up for this Arca.");
      const challenge = new Uint8Array(await crypto.subtle.digest(
        "SHA-256",
        new TextEncoder().encode(`${APPROVAL_DOMAIN}
${APPROVAL_NAMESPACE}
${statement}`)
      ));
      const credential = await navigator.credentials.get({
        publicKey: {
          challenge,
          rpId: location.hostname,
          userVerification: "required",
          timeout: 6e4,
          allowCredentials
        }
      });
      if (!credential) throw new Error("No passkey was selected.");
      const response = credential.response;
      return {
        alg: "ES256",
        authenticatorData: b64url(new Uint8Array(response.authenticatorData)),
        clientDataJSON: b64url(new Uint8Array(response.clientDataJSON)),
        credentialId: b64url(new Uint8Array(credential.rawId)),
        kind: "webauthn",
        signature: b64url(new Uint8Array(response.signature)),
        v: 1
      };
    }
    async function approveProposal(id) {
      const card = approvals.cards.get(id);
      const plan = card?.plan;
      if (!card || card.state !== "waiting" || !plan) return;
      if (!plan.statement) {
        card.state = "failed";
        card.reason = "Arca did not send the approval statement. Reload the page.";
        updatePanel();
        return;
      }
      if (Date.now() >= plan.expiresAt) {
        card.state = "expired";
        updatePanel();
        return;
      }
      card.state = "approving";
      card.busy = "";
      card.reason = "";
      card.note = "";
      updatePanel();
      try {
        const proof = await passkeyProof(plan.statement);
        const statementBytes = new TextEncoder().encode(plan.statement);
        const proofBytes = new TextEncoder().encode(JSON.stringify(proof));
        const approval = `${b64(statementBytes)}.${b64(proofBytes)}`;
        const response = await fetch("/approve/approve", {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json", Accept: "application/json" },
          body: JSON.stringify({ plan: id, approval })
        });
        const body = await response.json().catch(() => ({}));
        if (!response.ok) {
          card.state = body?.refusal === "Expired" || body?.refusal === "NoSuchPlan" ? "expired" : "failed";
          card.reason = plainRefusal(body?.refusal, response.status);
          return;
        }
        card.state = "done";
        card.result = plainResult(plan.action, body?.result);
        approvals.plans.delete(id);
      } catch (error) {
        if (error?.name === "NotAllowedError" || error?.name === "AbortError") {
          card.state = "waiting";
          card.note = "Passkey cancelled.";
        } else {
          card.state = "failed";
          card.reason = plainError(error);
        }
      } finally {
        updatePanel();
        void refreshApprovals();
      }
    }
    async function declineProposal(id) {
      const card = approvals.cards.get(id);
      if (!card || card.state !== "waiting") return;
      if (!proposalId({ id })) return;
      card.busy = "declining";
      updatePanel();
      try {
        const response = await fetch("/approve/decline", {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json", Accept: "application/json" },
          body: JSON.stringify({ plan: id })
        });
        const body = await response.json().catch(() => ({}));
        if (!response.ok) {
          card.busy = "";
          card.state = body?.refusal === "Expired" || body?.refusal === "NoSuchPlan" ? "expired" : "failed";
          card.reason = plainRefusal(body?.refusal, response.status);
          return;
        }
        card.state = "declined";
        approvals.plans.delete(id);
      } catch (error) {
        card.state = "failed";
        card.reason = plainError(error);
      } finally {
        card.busy = "";
        updatePanel();
        void refreshApprovals();
      }
    }
    function approvalSignature() {
      return JSON.stringify({
        plans: [...approvals.plans.values()].map((plan) => [plan.id, plan.summary, plan.action, plan.argsDigest, plan.expiresAt, plan.details]),
        deletes: approvals.deletes.map((plan) => plan.id),
        cards: [...approvals.cards].map(([id, card]) => [id, card.state, card.reason, card.result, card.note, card.busy]),
        passkeysTrusted: approvals.passkeysTrusted,
        loaded: approvals.loaded,
        error: approvals.error
      });
    }
    function approvalsWanted() {
      if (!approvalConfigured()) return false;
      if (!approvals.loaded || approvals.error) return true;
      if (approvals.plans.size || approvals.deletes.length) return true;
      if ([...approvals.cards.values()].some((card) => card.state === "waiting" || card.state === "approving")) return true;
      return (state.dashboard?.plans ?? 0) > 0 || state.tab === "steps";
    }
    const applyEvent = (event, data) => {
      if (event === "start") {
        chat.session = typeof data?.session === "string" && data.session ? data.session : "Doctor";
        chat.sessionId = data?.sessionId ?? null;
        chat.error = "";
      } else if (event === "text") {
        const message = activeAssistant();
        if (message && typeof data?.text === "string") message.text += data.text;
      } else if (event === "tool") {
        const message = activeAssistant();
        if (!message || !data?.callId) return;
        const card = message.tools.find((tool) => tool.callId === data.callId);
        if (card) Object.assign(card, data);
        else message.tools.push({ ...data });
        if (proposalId(data?.proposal)) void refreshApprovals();
      } else if (event === "budget") {
        const message = activeAssistant();
        const body = String(data?.message ?? "Daily budget reached, try tomorrow.");
        if (message) message.notes.push({
          kind: "budget",
          title: "Daily budget reached",
          body: `${body} (${Number(data?.used ?? 0)} of ${Number(data?.budget ?? 0)} tokens used today.)`
        });
      } else if (event === "cap") {
        const message = activeAssistant();
        if (message) message.notes.push({
          kind: "cap",
          title: "Answer stopped",
          body: String(data?.message ?? "This answer reached a per-turn limit.")
        });
      } else if (event === "error") {
        const message = activeAssistant();
        if (message) message.notes.push({
          kind: "error",
          title: "Doctor unavailable",
          body: String(data?.message ?? "The Doctor could not answer.")
        });
      } else if (event === "usage") {
        chat.usage = data;
      } else if (event === "arca-cut") {
        const message = activeAssistant();
        if (message) message.notes.push({
          kind: "cutoff",
          title: "Cut off",
          body: "The answer was cut off."
        });
      } else if (event === "done") {
        chat.done = true;
        chat.reason = data?.reason ?? "complete";
      }
    };
    const finishChat = () => {
      if (!chat.done) {
        const message = activeAssistant();
        if (message && !message.notes.some((note) => note.kind === "cutoff")) {
          message.notes.push({ kind: "cutoff", title: "Cut off", body: "The answer was cut off." });
        }
      }
      chat.streaming = false;
      chat.done = false;
      chat.controller = null;
      renderChat();
      updateSend();
      input.focus();
    };
    const sendChat = async () => {
      const text3 = input.value.trim();
      if (!text3 || chat.streaming) return;
      input.value = "";
      updateSend();
      chat.messages.push({ role: "user", text: text3, tools: [], notes: [] });
      chat.messages.push({ role: "assistant", text: "", tools: [], notes: [] });
      chat.streaming = true;
      chat.done = false;
      chat.error = "";
      renderChat();
      chatScroll.scrollTop = chatScroll.scrollHeight;
      const controller = new AbortController();
      chat.controller = controller;
      try {
        const response = await fetch(`${base}/chat`, {
          method: "POST",
          credentials: "same-origin",
          headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
          body: JSON.stringify({ message: text3 }),
          signal: controller.signal
        });
        if (!response.ok) {
          chat.error = chatStatusText(response.status);
          chat.messages.pop();
          return;
        }
        if (!response.body) {
          chat.error = chatStatusText(0);
          chat.messages.pop();
          return;
        }
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (; ; ) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const { blocks: blocks2, rest } = sseBlocks(buffer);
          buffer = rest;
          for (const block of blocks2) {
            const parsed = parseSseBlock(block);
            if (parsed) applyEvent(parsed.event, parsed.data);
          }
          renderChat();
          if (chatScroll.scrollHeight - chatScroll.scrollTop - chatScroll.clientHeight < 48) {
            chatScroll.scrollTop = chatScroll.scrollHeight;
          }
        }
        buffer += decoder.decode();
        const { blocks } = sseBlocks(`${buffer}

`);
        for (const block of blocks) {
          const parsed = parseSseBlock(block);
          if (parsed) applyEvent(parsed.event, parsed.data);
        }
      } catch (error) {
        if (error?.name !== "AbortError") chat.error = "The connection to the Doctor was lost.";
      } finally {
        finishChat();
      }
    };
    send.addEventListener("click", () => {
      void sendChat();
    });
    input.addEventListener("input", updateSend);
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey) {
        event.preventDefault();
        void sendChat();
      }
    });
    updateSend();
    const loadHistory = async () => {
      try {
        const response = await fetch(`${base}/chat/history`, { credentials: "same-origin" });
        if (!response.ok) {
          if (!chat.messages.length) chat.error = chatStatusText(response.status);
          chat.loaded = true;
          renderChat();
          return;
        }
        const data = await response.json();
        chat.session = typeof data?.session?.name === "string" ? data.session.name : "Doctor";
        chat.sessionId = data?.session?.id ?? null;
        if (!chat.messages.length) {
          chat.messages = (Array.isArray(data?.messages) ? data.messages : []).filter((message) => message && (message.role === "user" || message.role === "assistant")).map((message) => ({
            role: message.role,
            text: String(message.text ?? ""),
            tools: Array.isArray(message.tools) ? message.tools.map((tool) => ({ ...tool })) : [],
            notes: []
          }));
        }
        chat.usage = data?.usage ?? null;
        chat.loaded = true;
        renderChat();
        if (approvalsWanted()) void refreshApprovals();
      } catch {
        chat.loaded = true;
      }
    };
    const updatePanel = () => {
      const dashboard = state.dashboard;
      const journal = list2(dashboard?.journal);
      const plans = dashboard?.plans ?? 0;
      const url = approvalUrl(state.envelope.approvalUrl);
      const pending = (state.envelope.hosts ?? []).some((host) => host.journalPending);
      state.plans = plans;
      state.approvalUrl = url;
      const signature = JSON.stringify({ journal, plans, url, pending, approvals: approvalSignature() });
      if (signature === state.signatures.panel) return;
      state.signatures.panel = signature;
      const now = Date.now();
      const stepsScrollTop = stepsScroll.scrollTop;
      steps.replaceChildren();
      if (!journal.length) {
        const known = (state.envelope.hosts ?? []).some((host) => host.journalKnown);
        steps.append(pending ? emptyState(doc, "Reading the latest activity\u2026", "The journal is catching up; this fills in on the next refresh.") : emptyState(
          doc,
          known ? "No recent Arca activity" : "Activity journal unavailable",
          known ? "Arca has not applied a change recently." : "This host did not return a journal."
        ));
      } else {
        if (pending) steps.append(el(doc, "p", "arca-thread-note", "Reading the latest activity\u2026"));
        for (const record of journal) steps.append(stepCard(doc, record, now));
      }
      const shown = /* @__PURE__ */ new Set();
      for (const id of approvals.plans.keys()) {
        const card = proposalCard(doc, id);
        if (card) {
          steps.append(card);
          shown.add(id);
        }
      }
      for (const [id, card] of approvals.cards) {
        if (shown.has(id) || !card.plan || card.state === "waiting" || card.state === "approving") continue;
        const node = proposalCard(doc, id);
        if (node) steps.append(node);
      }
      if (approvals.deletes.length) steps.append(approvalCard(doc, approvals.deletes.length, url));
      else if ((!approvals.loaded || approvals.error) && plans > 0) steps.append(approvalCard(doc, plans, url));
      if (approvals.error) {
        steps.append(el(doc, "p", "arca-thread-note arca-approval-error", approvals.error));
      }
      stepsScroll.scrollTop = stepsScrollTop;
      renderChat();
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
      if (approvalsWanted()) void refreshApprovals();
    };
    const approvalTimer = setInterval(() => {
      if (expireApprovals()) updatePanel();
      else updateExpiryLabels();
    }, 1e3);
    void loadHistory();
    renderChat();
    return {
      update,
      stop() {
        clearInterval(clockTimer);
        clearInterval(approvalTimer);
        window.removeEventListener("hashchange", onHash);
        chat.controller?.abort();
      }
    };
  }
  function mountCanvas(root2, { fetchSnapshot, pollMs = 5e3 } = {}) {
    const base = (root2.dataset?.arcaBase ?? "/arca-canvas").replace(/\/+$/, "") || "/arca-canvas";
    const load = fetchSnapshot ?? (() => fetch(`${base}/snapshot`, { credentials: "same-origin" }));
    const view = createCanvas(root2, { base });
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
