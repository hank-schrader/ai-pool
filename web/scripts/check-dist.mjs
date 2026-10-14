import { readdir, readFile } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
const root = new URL('../../crates/pool-server/static/dist/', import.meta.url);
let total = 0;
async function inspect(dir) {
  for (const item of await readdir(dir, { withFileTypes: true })) {
    const path = new URL(item.name, dir);
    if (item.isDirectory()) await inspect(new URL(`${item.name}/`, dir));
    else if (/\.(js|css)$/.test(item.name)) {
      const size = gzipSync(await readFile(path)).length;
      total += size;
      console.log(`${item.name}: ${size} bytes gzip`);
    }
  }
}
await inspect(root);
console.log(`Total JS + CSS: ${total} bytes gzip (limit 51200)`);
if (total > 50 * 1024) process.exitCode = 1;
