import { build } from 'esbuild';

await build({
  entryPoints: ['src/client.js'],
  outfile: 'client.js',
  bundle: true,
  platform: 'browser',
  format: 'cjs',
  target: 'es2022',
  legalComments: 'none',
  banner: { js: 'window.__ModuleLoader__.load({ id: "arca-canvas-dsh", factory: (require) => {\nconst module = { exports: {} }; const exports = module.exports;' },
  footer: { js: 'return module.exports;\n} });' },
});
