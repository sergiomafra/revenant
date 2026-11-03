// Puts the hand-tuned small PNGs into a generated .ico or .icns, in place of the 16 and 32 px images that
// `tauri icon` scales down from the 1024 px master. Node only, no dependencies.
// Usage: node scripts/small-icons.mjs <icon.ico|icon.icns> <16 px png> <32 px png>
// .ico: the 16 and 32 px entries are replaced (PNG entries). .icns: is32/s8mk become icp4 (16 px), il32/l8mk
// become icp5 (32 px), and ic11 (16 px @2x) takes the 32 px image.
import fs from 'node:fs';
const [, , file, p16, p32] = process.argv;
const png = { 16: fs.readFileSync(p16), 32: fs.readFileSync(p32) };
const b = fs.readFileSync(file);
if (file.endsWith('.ico')) {
  const n = b.readUInt16LE(4); const imgs = [];
  for (let i = 0; i < n; i++) { const o = 6 + 16 * i; const w = b[o] || 256;
    imgs.push({ w, data: png[w] ?? b.subarray(b.readUInt32LE(o + 12), b.readUInt32LE(o + 12) + b.readUInt32LE(o + 8)) }); }
  const head = Buffer.alloc(6 + 16 * n); head.writeUInt16LE(0, 0); head.writeUInt16LE(1, 2); head.writeUInt16LE(n, 4);
  let off = head.length;
  imgs.forEach((im, i) => { const o = 6 + 16 * i; head[o] = im.w === 256 ? 0 : im.w; head[o + 1] = im.w === 256 ? 0 : im.w;
    head.writeUInt16LE(1, o + 4); head.writeUInt16LE(32, o + 6); head.writeUInt32LE(im.data.length, o + 8);
    head.writeUInt32LE(off, o + 12); off += im.data.length; });
  fs.writeFileSync(file, Buffer.concat([head, ...imgs.map((i) => i.data)]));
} else {
  const out = []; let o = 8;
  const chunk = (t, d) => { const h = Buffer.alloc(8); h.write(t, 0); h.writeUInt32BE(d.length + 8, 4); return Buffer.concat([h, d]); };
  while (o < b.length) { const t = b.subarray(o, o + 4).toString(); const l = b.readUInt32BE(o + 4);
    if (t === 'is32') out.push(chunk('icp4', png[16]));
    else if (t === 'il32') out.push(chunk('icp5', png[32]));
    else if (t === 'ic11') out.push(chunk('ic11', png[32]));
    else if (t !== 's8mk' && t !== 'l8mk') out.push(b.subarray(o, o + l));
    o += l; }
  const body = Buffer.concat(out); const h = Buffer.alloc(8); h.write('icns', 0); h.writeUInt32BE(body.length + 8, 4);
  fs.writeFileSync(file, Buffer.concat([h, body]));
}
