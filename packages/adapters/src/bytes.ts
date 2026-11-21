/** Byte-level helpers the signature code uses. */

/** The bytes of a string in UTF-8. */
const encoder = new TextEncoder();

export function utf8(text: string): Uint8Array {
  return encoder.encode(text);
}

/** Lower-case hex of the bytes in a digest. */
export function toHex(bytes: ArrayBuffer): string {
  const digits: string[] = [];
  for (const byte of new Uint8Array(bytes)) digits.push(byte.toString(16).padStart(2, "0"));
  return digits.join("");
}

/**
 * Two byte arrays are equal. The caller decodes secrets whose length carries
 * meaning, so equality is the whole contract.
 */
export function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.length !== right.length) return false;
  let difference = 0;
  for (const [index, byte] of left.entries()) difference |= byte ^ right[index]!;
  return difference === 0;
}

/** Bytes to a byte array a WebCrypto call accepts. */
export function buffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.length);
  copy.set(bytes);
  return copy.buffer;
}
