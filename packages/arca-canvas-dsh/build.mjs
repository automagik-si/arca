import { build } from 'esbuild';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export async function buildBrowser(outdir = '.') {
  const absWorkingDir = fileURLToPath(new URL('.', import.meta.url));
  await build({
    absWorkingDir, entryPoints: ['src/client.js'], outfile: resolve(absWorkingDir, outdir, 'client.js'),
    bundle: true, external: ['react'], platform: 'browser', format: 'cjs',
    target: 'es2022', legalComments: 'none',
    banner: { js: 'window.__ModuleLoader__.load({ id: "arca-canvas-dsh", factory: (require) => {\nconst module = { exports: {} }; const exports = module.exports;' },
    footer: { js: 'return module.exports;\n} });' },
  });
  await build({
    absWorkingDir, entryPoints: ['src/page.js'], outfile: resolve(absWorkingDir, outdir, 'page.js'),
    bundle: true, platform: 'browser', format: 'iife', target: 'es2022', legalComments: 'none',
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await buildBrowser();
