// Regression test for GitHub issue #11:
// `readVarint` truncated to 32 bits, corrupting millisecond timestamps.
//
// Run with: node --experimental-strip-types --experimental-transform-types \
//   adapters/antigravity/protobuf.regression.mjs
import { strict as assert } from 'node:assert';
import { readVarint, extractTimestampMs } from './protobuf.ts';

function encodeVarint(v) {
  let x = BigInt(v);
  const out = [];
  while (x >= 0x80n) {
    out.push(Number(x & 0x7fn) | 0x80);
    x >>= 7n;
  }
  out.push(Number(x));
  return Uint8Array.from(out);
}

// Small values (tags, field lengths) must decode exactly as before.
for (const v of [0, 1, 127, 128, 300, 624485, 1_000_000_000, 2 ** 32 - 1]) {
  const [decoded, next] = readVarint(encodeVarint(v), 0);
  assert.equal(decoded, v, `small varint ${v}`);
  assert.equal(next, encodeVarint(v).length, `offset for ${v}`);
}

// Values above 2^32 must survive (old code returned value mod 2^32).
// 1770000000000 previously decoded as 473474456.
for (const v of [2 ** 32, 1_770_000_000_000, 1_767_225_600_000]) {
  const [decoded] = readVarint(encodeVarint(v), 0);
  assert.equal(decoded, v, `large varint ${v}`);
}

// End to end: a field-5 wire-0 unix-millisecond timestamp must come back as
// milliseconds, not multiplied by 1000 (the old truncation pushed it under
// the 1e12 threshold, yielding year-2079+ dates).
{
  const ts = 1_770_000_000_000;
  const payload = Buffer.concat([
    Buffer.from(encodeVarint((5 << 3) | 0)),
    Buffer.from(encodeVarint(ts)),
  ]);
  assert.equal(
    extractTimestampMs(new Uint8Array(0), new Uint8Array(payload)),
    ts,
  );
}

console.log('protobuf regression test passed (#11)');
