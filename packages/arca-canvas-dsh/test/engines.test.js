import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = async (name) => JSON.parse(await readFile(new URL(`../${name}`, import.meta.url), 'utf8'));

// DSH reads engines.dsh before it loads a plugin and leaves the plugin out when its own version is not in the range
// (dsh-app-boot, in every 0.2.1-rc and 0.2.2-rc release). It compares with prereleases counted. So the range decides
// on which hosts the canvas exists at all:
// - The floor is rc.1 and not rc.2: a floor of rc.2 takes the canvas away from every host still on rc.1.
// - The ceiling is `<0.3.0-0` and not `<0.3.0`: with prereleases counted, `<0.3.0` takes 0.3.0-alpha.1, a release
//   nobody has run the canvas on.
// - The range this replaced, `>=0.2.2-rc.1 <0.2.2-rc.2`, refused rc.2: on an rc.2 host the canvas was gone.
// Change it only together with a check of the canvas against the release the new range lets in.
test('engines.dsh takes the 0.2.2 releases from rc.1 on, and no 0.3.0 prerelease', async () => {
  const manifest = await read('package.json');
  assert.equal(manifest.engines.dsh, '>=0.2.2-rc.1 <0.3.0-0');
  // npm copies the range into the lock file; the two may not differ.
  const lock = await read('package-lock.json');
  assert.equal(lock.packages[''].engines.dsh, manifest.engines.dsh);
});
