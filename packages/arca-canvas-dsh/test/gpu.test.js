import assert from 'node:assert/strict';
import test from 'node:test';
import {
  buildDashboard, formatCelsius, formatGb, formatWatts, gpuCards, gpuPowerCapped, WATCH_ONLY_SERVICES,
} from '../src/model.js';
import {
  PROPOSE_ACTIONS, createReadTools, doctorPrompt, formatToolResult,
} from '../src/chat.js';

const GB = 1e9;
const CONTAINER_ID = 'a'.repeat(64);
const OTHER_CONTAINER_ID = 'b'.repeat(64);

function telemetry(overrides = {}) {
  return {
    utilizationPct: 12,
    memoryUsedBytes: 6.1 * GB,
    memoryTotalBytes: 17.2 * GB,
    powerDrawW: 61.2,
    powerLimitW: 165,
    temperatureC: 63.5,
    smClockMhz: 2100,
    eventReasons: '0x0000000000000004',
    ...overrides,
  };
}

function card(pci, product, overrides = {}) {
  return {
    pci,
    uuid: null,
    vendor: 'NVIDIA',
    product,
    driver: 'nvidia',
    driverVersion: '590.1',
    numaNode: 0,
    vramTotalBytes: 17.2 * GB,
    use: 'driver-bound',
    telemetry: telemetry(),
    reason: null,
    processes: [],
    processReason: null,
    ...overrides,
  };
}

function gpu(cards, overrides = {}) {
  return { reason: null, skipped: null, cards, ...overrides };
}

function host(name, gpuValue, overrides = {}) {
  return {
    name,
    machine: { mode: 'kernel', incusReachable: true },
    instances: [],
    services: [],
    metrics: { latest: { gpu: gpuValue } },
    ...overrides,
  };
}

test('two identical cards stay two rows and are labelled by slot with the PCI hover', () => {
  const dashboard = buildDashboard({
    hosts: [host('alpha', gpu([
      card('0000:01:00.0', 'NVIDIA GeForce RTX 4060 Ti'),
      card('0000:02:00.0', 'NVIDIA GeForce RTX 4060 Ti', { telemetry: telemetry({ utilizationPct: 35 }) }),
    ]))],
  });
  const cards = dashboard.hosts[0].gpus;
  assert.equal(cards.length, 2);
  assert.deepEqual(cards.map((entry) => entry.label), ['GPU 1', 'GPU 2']);
  assert.deepEqual(cards.map((entry) => entry.pci), ['0000:01:00.0', '0000:02:00.0']);
  assert.match(cards[0].title, /RTX 4060 Ti/);
  assert.match(cards[0].title, /0000:01:00\.0/);
  assert.match(cards[1].title, /0000:02:00\.0/);
  assert.equal(cards[0].utilizationText, '12%');
  assert.equal(cards[1].utilizationText, '35%');
  assert.equal(cards[0].vramText, `${formatGb(6.1 * GB)} / ${formatGb(17.2 * GB)} GB`);
  assert.ok(cards[0].vramPercent > 35 && cards[0].vramPercent < 36);
});

test('a remote host with inventory only shows the card and its plain reason, never zero', () => {
  const dashboard = buildDashboard({
    hosts: [host('beta', gpu([
      card('0000:03:00.0', 'NVIDIA GeForce RTX 4090', {
        telemetry: null,
        vramTotalBytes: null,
        reason: 'not measurable from here',
        processes: [],
        processReason: 'not measurable from here',
      }),
    ]))],
  });
  const [entry] = dashboard.hosts[0].gpus;
  assert.equal(entry.hasReading, false);
  assert.equal(entry.utilizationPct, null);
  assert.equal(entry.utilizationText, '');
  assert.equal(entry.vramPercent, null);
  assert.equal(entry.vramText, '');
  assert.equal(entry.statusText, 'not measurable from here');
  assert.doesNotMatch(entry.statusText, /0%/);
});

test('a host with no GPU has no GPU rows', () => {
  const dashboard = buildDashboard({ hosts: [host('gamma', gpu([]))] });
  assert.deepEqual(dashboard.hosts[0].gpus, []);
  assert.equal(gpuCards(undefined).length, 0);
  assert.equal(gpuCards({ cards: null }).length, 0);
});

test('telemetry with a reason stays a reason and no invented percentage', () => {
  const dashboard = buildDashboard({
    hosts: [host('beta', gpu([
      card('0000:04:00.0', 'NVIDIA GeForce RTX 3060', {
        telemetry: null,
        reason: 'nvidia-smi is not installed',
        vramTotalBytes: null,
      }),
    ], { reason: 'the nvidia-smi read timed out' }))],
  });
  const [entry] = dashboard.hosts[0].gpus;
  assert.equal(entry.hasReading, false);
  assert.equal(entry.statusText, 'nvidia-smi is not installed');
  assert.equal(entry.utilizationText, '');

  const emptyReading = buildDashboard({
    hosts: [host('beta', gpu([
      card('0000:05:00.0', 'NVIDIA GeForce RTX 3060', {
        telemetry: telemetry({ utilizationPct: null, memoryUsedBytes: null, memoryTotalBytes: null }),
        vramTotalBytes: null,
        reason: 'no reading yet',
      }),
    ]))],
  }).hosts[0].gpus[0];
  assert.equal(emptyReading.hasReading, false);
  assert.equal(emptyReading.statusText, 'no reading yet');
  assert.equal(emptyReading.utilizationText, '');
});

test('process owners of every kind are named with their own VRAM in GB', () => {
  const dashboard = buildDashboard({
    hosts: [host('alpha', gpu([
      card('0000:01:00.0', 'NVIDIA GeForce RTX 4060 Ti', {
        processes: [
          { pci: '0000:01:00.0', pid: 10, vramBytes: 1.5 * GB, owner: { kind: 'instance', project: 'studio', name: 'app' } },
          { pci: '0000:01:00.0', pid: 11, vramBytes: 0.5 * GB, owner: { kind: 'instance', project: 'studio', name: 'app' } },
          { pci: '0000:01:00.0', pid: 12, vramBytes: 2 * GB, owner: { kind: 'instance', project: 'media', name: 'render', container: CONTAINER_ID } },
          { pci: '0000:01:00.0', pid: 13, vramBytes: 3 * GB, owner: { kind: 'docker', container: OTHER_CONTAINER_ID } },
          { pci: '0000:01:00.0', pid: 14, vramBytes: 0.25 * GB, owner: { kind: 'host' } },
        ],
      }),
    ]))],
  });
  const owners = dashboard.hosts[0].gpus[0].owners;
  assert.deepEqual(owners.map((owner) => [owner.kind, owner.label, owner.vramText]), [
    ['docker', `Docker container ${OTHER_CONTAINER_ID.slice(0, 12)}`, '3 GB'],
    ['instance', 'Media / render', '2 GB'],
    ['instance', 'Studio / app', '2 GB'],
    ['host', 'on the host', '0.3 GB'],
  ]);
  assert.equal(owners[0].container, OTHER_CONTAINER_ID);
  assert.equal(owners[1].detail, `Docker ${CONTAINER_ID.slice(0, 12)}`);
  assert.equal(owners[3].container, '');
});

test('the power-limit sentence needs the SW power cap bit, and only that bit', () => {
  assert.equal(gpuPowerCapped('0x0000000000000004'), true);
  assert.equal(gpuPowerCapped('0x0000000000000005'), true);
  assert.equal(gpuPowerCapped('0x0000000000000001'), false);
  assert.equal(gpuPowerCapped('0x0000000000000002'), false);
  assert.equal(gpuPowerCapped(4), true);
  assert.equal(gpuPowerCapped('4'), true);
  assert.equal(gpuPowerCapped(null), false);
  assert.equal(gpuPowerCapped('not a mask'), false);

  const dashboard = buildDashboard({
    hosts: [host('alpha', gpu([
      card('0000:01:00.0', 'NVIDIA GeForce RTX 4060 Ti', { telemetry: telemetry({ eventReasons: '0x4' }) }),
      card('0000:02:00.0', 'NVIDIA GeForce RTX 4060 Ti', { telemetry: telemetry({ eventReasons: '0x1' }) }),
    ]))],
  });
  assert.deepEqual(dashboard.hosts[0].gpus.map((entry) => entry.powerCapped), [true, false]);
  assert.equal(formatWatts(61.2), '61.2 W');
  assert.equal(formatCelsius(63.5), '63.5 °C');
});

test('a watch-only host is keyed off arca_status mode, not a service refusal', () => {
  const dashboard = buildDashboard({
    hosts: [{
      ...host('gamma', gpu([])),
      machine: { mode: 'watch-only', incusReachable: true },
      services: [{ name: 'web1', state: 'running', lanPort: 8080 }],
      error: 'service_list: Arca refused the request (403).',
    }],
  });
  const machine = dashboard.hosts[0];
  assert.equal(machine.watchOnly, true);
  assert.equal(machine.mode, 'watch-only');
  assert.equal(WATCH_ONLY_SERVICES, "Arca watches this machine; it doesn't run services here yet");
  const kernel = buildDashboard({ hosts: [host('alpha', gpu([]))] });
  assert.equal(kernel.hosts[0].watchOnly, false);
});

test('the Doctor tool text names the cards and every GPU user in plain words', () => {
  const value = {
    host: 'alpha',
    latest: {
      gpu: gpu([
        card('0000:01:00.0', 'NVIDIA GeForce RTX 4060 Ti', {
          processes: [
            { pci: '0000:01:00.0', pid: 1, vramBytes: 6.1 * GB, owner: { kind: 'instance', project: 'studio', name: 'app' } },
            { pci: '0000:01:00.0', pid: 2, vramBytes: 2 * GB, owner: { kind: 'docker', container: CONTAINER_ID } },
            { pci: '0000:01:00.0', pid: 3, vramBytes: 0.5 * GB, owner: { kind: 'host' } },
          ],
        }),
        card('0000:02:00.0', 'NVIDIA GeForce RTX 4090', {
          telemetry: null,
          reason: 'not measurable from here',
          processes: [],
          processReason: 'not measurable from here',
        }),
      ]),
    },
  };
  const text = formatToolResult('node_metrics', value);
  assert.match(text, /GPU 4060 Ti: 12% busy, 6\.1 GB used of 17\.2 GB, 61\.2 W of 165 W, 63\.5 °C\./);
  assert.match(text, /Who is using GPU 4060 Ti: project Studio, instance app \(6\.1 GB\); Docker container a{12} \(2\.0 GB\); on the host \(0\.5 GB\)\./);  assert.match(text, /GPU 4060 Ti is held back by its power limit\./);
  assert.match(text, /GPU 4090: not measurable from here\./);
  assert.doesNotMatch(text, /GPU 4090: 0%/);

  const prompt = doctorPrompt({ hosts: [{ name: 'alpha' }] });
  assert.match(prompt, /When the owner asks about a GPU/);
  assert.match(prompt, /held back by its power limit/);
  assert.match(prompt, /Docker container/);
  const nodeMetrics = createReadTools({ hosts: [{ name: 'alpha' }], clientFor: () => null })
    .find((tool) => tool.name === 'node_metrics');
  assert.match(nodeMetrics.description, /who is using each GPU/);
});

test('the proposal tool offers gpu_attach and gpu_detach beside the service actions', () => {
  for (const action of ['gpu_attach', 'gpu_detach']) assert.equal(PROPOSE_ACTIONS.includes(action), true);
  assert.equal(PROPOSE_ACTIONS.includes('gpu_delete'), false);
});
