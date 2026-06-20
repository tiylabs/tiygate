// Generates a Windows multi-resolution .ico from a source PNG.
// Usage: node scripts/make-ico.mjs <src.png> <out.ico> [size1,size2,...]
import { readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const src = process.argv[2];
const out = process.argv[3];
const sizes = (process.argv[4] || '16,24,32,48,64,128,256').split(',').map(Number);

const pngs = sizes.map((size) => {
  const tmp = join(tmpdir(), `ico_${size}_${Date.now()}_${Math.random().toString(36).slice(2)}.png`);
  execSync(`sips -s format png -z ${size} ${size} "${src}" --out "${tmp}"`, { stdio: 'pipe' });
  return readFileSync(tmp);
});

// ICO header (6 bytes) + ICONDIRENTRY (16 bytes each)
const headerSize = 6;
const entrySize = 16;
const count = pngs.length;
const header = Buffer.alloc(headerSize);
header.writeUInt16LE(0, 0); // reserved
header.writeUInt16LE(1, 2); // type = icon
header.writeUInt16LE(count, 4);

let offset = headerSize + entrySize * count;
const entries = [];
for (let i = 0; i < pngs.length; i++) {
  const png = pngs[i];
  const s = sizes[i];
  const entry = Buffer.alloc(entrySize);
  entry.writeUInt8(s >= 256 ? 0 : s, 0); // width
  entry.writeUInt8(s >= 256 ? 0 : s, 1); // height
  entry.writeUInt8(0, 2); // color palette
  entry.writeUInt8(0, 3); // reserved
  entry.writeUInt16LE(1, 4); // color planes
  entry.writeUInt16LE(32, 6); // bits per pixel
  entry.writeUInt32LE(png.length, 8); // image size
  entry.writeUInt32LE(offset, 12); // image offset
  entries.push(entry);
  offset += png.length;
}

const result = Buffer.concat([header, ...entries, ...pngs]);
writeFileSync(out, result);
console.log(`Generated ${out} with sizes: ${sizes.join(', ')}`);
