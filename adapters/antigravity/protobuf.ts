/**
 * Hand-rolled protobuf walker for Antigravity `steps.step_payload`.
 *
 * Field numbers are reverse-engineered (txcript / agy-acp / pi-antigravity-bridge):
 *   19.2  user text (step_type 14)
 *   20.1  assistant text (step_type 15)
 *   30.4  title (step_type 23)
 *    5    CortexStepMetadata (wire 2): Timestamp at 5.1.{1 seconds, 2 nanos};
 *         tool call remains at 5.4 (`extractToolCall`). Same field may also be
 *         a unix varint (wire 0).
 */

export function readVarint(buf: Uint8Array, i: number): [number, number] {
  let result = 0;
  let shift = 0;
  let offset = i;
  for (let count = 0; count < 10; count++) {
    if (offset >= buf.length) {
      throw new RangeError(`varint at ${i} ran past end of buffer`);
    }
    const byte = buf[offset++];
    // NOTE: arithmetic accumulation, NOT `result |= (byte & 0x7f) << shift`.
    // Bitwise ops truncate to 32 bits (`<<` also masks the shift mod 32), so
    // unix-millisecond timestamps (> 2^32, 6-byte varints) decoded as garbage.
    // Doubles stay exact up to 2^53, which covers all realistic timestamps.
    result += (byte & 0x7f) * 2 ** shift;
    if ((byte & 0x80) === 0) return [result, offset];
    shift += 7;
  }
  throw new RangeError(`varint at ${i} exceeded 10 bytes`);
}

export interface Field {
  field: number;
  wire: number;
  bytes: Uint8Array | null;
  varint: number | null;
}

export function walkFields(buf: Uint8Array): Field[] {
  const out: Field[] = [];
  let i = 0;
  while (i < buf.length) {
    let tag: number;
    try {
      [tag, i] = readVarint(buf, i);
    } catch {
      break;
    }
    const field = tag >>> 3;
    const wire = tag & 0x07;
    if (wire === 0) {
      let val: number;
      try {
        [val, i] = readVarint(buf, i);
      } catch {
        break;
      }
      out.push({ field, wire, bytes: null, varint: val });
    } else if (wire === 2) {
      let len: number;
      try {
        [len, i] = readVarint(buf, i);
      } catch {
        break;
      }
      if (i + len > buf.length) break;
      out.push({ field, wire, bytes: buf.subarray(i, i + len), varint: null });
      i += len;
    } else if (wire === 5) {
      i += 4;
      out.push({ field, wire, bytes: null, varint: null });
    } else if (wire === 1) {
      i += 8;
      out.push({ field, wire, bytes: null, varint: null });
    } else {
      break;
    }
  }
  return out;
}

export function getField(buf: Uint8Array, target: number): Uint8Array | null {
  for (const f of walkFields(buf)) {
    if (f.field === target && f.wire === 2 && f.bytes) return f.bytes;
  }
  return null;
}

const utf8 = new TextDecoder('utf-8', { fatal: false });

export function utf8String(bytes: Uint8Array): string {
  return utf8.decode(bytes);
}

export function toUint8(v: unknown): Uint8Array {
  if (v instanceof Uint8Array) return v;
  if (ArrayBuffer.isView(v)) {
    const view = v as ArrayBufferView;
    return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  }
  return new Uint8Array(0);
}

export function extractUserText(payload: Uint8Array): string | null {
  const user = getField(payload, 19);
  if (!user) return null;
  const direct = getField(user, 2);
  if (direct) return utf8String(direct);
  const nested = getField(user, 3);
  if (nested) {
    const inner = getField(nested, 1);
    if (inner) return utf8String(inner);
  }
  return null;
}

export function extractAgentText(payload: Uint8Array): string | null {
  const agentText = getField(payload, 20);
  if (!agentText) return null;
  const text = getField(agentText, 1);
  return text ? utf8String(text) : null;
}

export function extractTitle(payload: Uint8Array): string | null {
  const titleUpdate = getField(payload, 30);
  if (!titleUpdate) return null;
  const title = getField(titleUpdate, 4);
  return title ? utf8String(title) : null;
}

export function extractToolCall(payload: Uint8Array): { name: string; inputJson: string } | null {
  const toolRun = getField(payload, 5);
  if (!toolRun) return null;
  const toolCall = getField(toolRun, 4);
  if (!toolCall) return null;
  let name = '';
  let inputJson = '';
  for (const f of walkFields(toolCall)) {
    if (f.field === 2 && f.bytes) name ||= utf8String(f.bytes);
    else if (f.field === 9 && f.bytes && !name) name = utf8String(f.bytes);
    else if (f.field === 3 && f.bytes) inputJson ||= utf8String(f.bytes);
  }
  if (!name && !inputJson) return null;
  return { name, inputJson };
}

function asTimestampMs(varint: number | null): number | undefined {
  if (varint == null || varint <= 1_000_000_000) return undefined;
  return Math.floor(varint > 1e12 ? varint : varint * 1000);
}

/** CortexStepMetadata / Timestamp envelope: field 1 is google.protobuf.Timestamp. */
function timestampFromEnvelope(buf: Uint8Array): number | undefined {
  if (!buf.length) return undefined;
  const envelope = getField(buf, 1) ?? buf;
  for (const f of walkFields(envelope)) {
    const ts = asTimestampMs(f.wire === 0 ? f.varint : null);
    if (ts !== undefined) return ts;
  }
  return undefined;
}

/**
 * Timestamp from step `metadata`, or from payload field 5.
 *
 * Field 5 is a oneof-ish slot: a unix varint (wire 0), or CortexStepMetadata
 * (wire 2) which also holds the tool-call message at 5.4 (`extractToolCall`).
 */
export function extractTimestampMs(
  metadata: Uint8Array,
  payload?: Uint8Array,
): number | undefined {
  const fromMeta = timestampFromEnvelope(metadata);
  if (fromMeta !== undefined) return fromMeta;
  if (!payload?.length) return undefined;
  for (const f of walkFields(payload)) {
    if (f.field !== 5) continue;
    if (f.wire === 0) {
      const ts = asTimestampMs(f.varint);
      if (ts !== undefined) return ts;
    } else if (f.wire === 2 && f.bytes) {
      const ts = timestampFromEnvelope(f.bytes);
      if (ts !== undefined) return ts;
    }
  }
  return undefined;
}
