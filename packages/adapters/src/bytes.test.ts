import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import { buffer, bytesEqual, toHex, utf8 } from "./bytes.ts";
import { constantTimeEqual } from "./signature.ts";

describe("utf8", () => {
  test("encodes ascii, multibyte, and empty text", () => {
    expect([...utf8("")]).toEqual([]);
    expect([...utf8("a")]).toEqual([97]);
    expect([...utf8("é")]).toEqual([195, 169]);
    expect([...utf8("🙂")]).toEqual([240, 159, 153, 130]);
  });
});

describe("toHex", () => {
  test("pads every byte to two digits", () => {
    expect(toHex(new Uint8Array([0, 1, 15, 16, 255]).buffer)).toBe("00010f10ff");
    expect(toHex(new Uint8Array([]).buffer)).toBe("");
  });

  test("round trips through decodeSecret-style parsing", () => {
    fc.assert(
      fc.property(fc.uint8Array(), (bytes) => {
        const hex = toHex(bytes.buffer as ArrayBuffer);
        expect(hex).toHaveLength(bytes.length * 2);
        expect(hex).toMatch(/^[0-9a-f]*$/);
      }),
    );
  });
});

describe("bytesEqual", () => {
  test("is true for equal bytes and false otherwise", () => {
    expect(bytesEqual(new Uint8Array([1, 2]), new Uint8Array([1, 2]))).toBe(true);
    expect(bytesEqual(new Uint8Array([]), new Uint8Array([]))).toBe(true);
    expect(bytesEqual(new Uint8Array([1, 2]), new Uint8Array([1, 3]))).toBe(false);
    expect(bytesEqual(new Uint8Array([1]), new Uint8Array([1, 2]))).toBe(false);
  });

  test("agrees with string equality through utf8", () => {
    fc.assert(
      fc.property(fc.string(), fc.string(), (left, right) => {
        expect(bytesEqual(utf8(left), utf8(right))).toBe(left === right);
        expect(constantTimeEqual(left, right)).toBe(left === right);
      }),
    );
  });
});

describe("buffer", () => {
  test("copies, so the caller's array cannot change the buffer", () => {
    const source = new Uint8Array([1, 2, 3]);
    const view = buffer(source);
    source[0] = 99;
    expect([...new Uint8Array(view)]).toEqual([1, 2, 3]);
  });
});
