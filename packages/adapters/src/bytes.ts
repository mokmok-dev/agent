const encoder = new TextEncoder();

export function utf8(text: string): Uint8Array {
  return encoder.encode(text);
}

export function toHex(bytes: ArrayBuffer): string {
  const digits: string[] = [];
  for (const byte of new Uint8Array(bytes)) digits.push(byte.toString(16).padStart(2, "0"));
  return digits.join("");
}

export function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.length !== right.length) return false;
  let difference = 0;
  for (const [index, byte] of left.entries()) difference |= byte ^ right[index]!;
  return difference === 0;
}

export function buffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.length);
  copy.set(bytes);
  return copy.buffer;
}
