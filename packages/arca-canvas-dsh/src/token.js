import { constants } from 'node:fs';
import { open } from 'node:fs/promises';

export async function readToken(path) {
  let file;
  try {
    // Inspect the opened inode: refuse symlinks, devices, FIFOs and permission races.
    file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    const stat = await file.stat();
    if (!stat.isFile() || (stat.mode & 0o7777) !== 0o400 || stat.size > 4096) {
      throw new Error('Token requires a regular 0400 file with a valid bearer.');
    }
    const token = (await file.readFile('utf8')).trim();
    if (!/^[A-Za-z0-9_-]+$/.test(token)) throw new Error('Token requires a regular 0400 file with a valid bearer.');
    return token;
  } catch {
    throw new Error('Token requires a regular 0400 file with a valid bearer.');
  } finally {
    await file?.close().catch(() => {});
  }
}

