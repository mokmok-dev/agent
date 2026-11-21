import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  array,
  arrayLenient,
  bool,
  lenient,
  nullable,
  num,
  object,
  oneOf,
  optional,
  raw,
  record,
  str,
  tagged,
} from "./decode.ts";
import { ProtocolError, RpcErrorCode, Rejection } from "./jsonrpc.ts";

const rejected = (fn: () => unknown): ProtocolError => {
  try {
    fn();
  } catch (error) {
    expect(error).toBeInstanceOf(ProtocolError);
    return error as ProtocolError;
  }
  throw new Error("expected a ProtocolError");
};

describe("primitives", () => {
  test("str keeps strings and rejects everything else", () => {
    expect(str("a")).toBe("a");
    expect(str("")).toBe("");
    for (const value of [1, null, undefined, {}, ["a"], true]) {
      expect(rejected(() => str(value)).code).toBe(RpcErrorCode.InvalidRequest);
    }
  });

  test("num keeps finite numbers and rejects the rest", () => {
    expect(num(0)).toBe(0);
    expect(num(-1.5)).toBe(-1.5);
    for (const value of [
      Number.NaN,
      Number.POSITIVE_INFINITY,
      Number.NEGATIVE_INFINITY,
      "1",
      null,
      true,
    ]) {
      expect(rejected(() => num(value)).code).toBe(RpcErrorCode.InvalidRequest);
    }
  });

  test("bool keeps only booleans", () => {
    expect(bool(true)).toBe(true);
    expect(bool(false)).toBe(false);
    for (const value of [0, 1, "true", null, undefined]) {
      expect(rejected(() => bool(value)).code).toBe(RpcErrorCode.InvalidRequest);
    }
  });

  test("a rejection carries the documented message and code", () => {
    const error = rejected(() => str(1));
    expect(error.message).toBe(Rejection.ErrorShape);
    expect(error.code).toBe(RpcErrorCode.InvalidRequest);
    expect(error.data).toBeUndefined();
  });
});

describe("oneOf", () => {
  const kind = oneOf("a", "b");

  test("accepts only the listed values", () => {
    expect(kind("a")).toBe("a");
    expect(kind("b")).toBe("b");
    expect(rejected(() => kind("c"))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => kind(1))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => kind(undefined))).toBeInstanceOf(ProtocolError);
  });

  test("a property test agrees with set membership", () => {
    fc.assert(
      fc.property(fc.string(), (value) => {
        const accepted = value === "a" || value === "b";
        let ok = true;
        try {
          kind(value);
        } catch {
          ok = false;
        }
        expect(ok).toBe(accepted);
      }),
    );
  });
});

describe("optional and nullable", () => {
  test("optional passes undefined through and decodes anything else", () => {
    const decoder = optional(str);
    expect(decoder(undefined)).toBeUndefined();
    expect(decoder("x")).toBe("x");
    expect(rejected(() => decoder(null))).toBeInstanceOf(ProtocolError);
  });

  test("nullable passes null through and decodes anything else", () => {
    const decoder = nullable(str);
    expect(decoder(null)).toBeNull();
    expect(decoder("x")).toBe("x");
    expect(rejected(() => decoder(undefined))).toBeInstanceOf(ProtocolError);
  });
});

describe("object", () => {
  const decoder = object({ id: str, count: num, note: optional(str) });

  test("keeps declared fields and drops the rest", () => {
    expect(decoder({ id: "a", count: 2, note: "n", extra: true })).toEqual({
      id: "a",
      count: 2,
      note: "n",
    });
  });

  test("an omitted optional key stays absent", () => {
    const decoded = decoder({ id: "a", count: 2 });
    expect(decoded).toEqual({ id: "a", count: 2 });
    expect(Object.keys(decoded)).toEqual(["id", "count"]);
  });

  test("a missing required field is a rejection", () => {
    expect(rejected(() => decoder({ id: "a" }))).toBeInstanceOf(ProtocolError);
  });

  test("a non-object is a rejection", () => {
    for (const value of [null, [], "x", 1]) {
      expect(rejected(() => decoder(value))).toBeInstanceOf(ProtocolError);
    }
  });

  test("a field that decodes to undefined is left out", () => {
    const drop = object({ kept: str, dropped: (): string | undefined => undefined });
    const decoded = drop({ kept: "k", dropped: "v" });
    expect(decoded).toEqual({ kept: "k" });
    expect(Object.keys(decoded)).toEqual(["kept"]);
  });
});

describe("array", () => {
  test("decodes every member and rejects a non-array", () => {
    expect(array(str)(["a", "b"])).toEqual(["a", "b"]);
    expect(array(str)([])).toEqual([]);
    expect(rejected(() => array(str)("a"))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => array(str)(["a", 1]))).toBeInstanceOf(ProtocolError);
  });

  test("arrayLenient keeps the members that decode and skips the rest", () => {
    const decoder = arrayLenient(str);
    expect(decoder(["a", 1, "b", null, "c"])).toEqual(["a", "b", "c"]);
    expect(decoder([])).toEqual([]);
    expect(rejected(() => decoder("a"))).toBeInstanceOf(ProtocolError);
  });

  test("a property test keeps exactly the decodable members in order", () => {
    fc.assert(
      fc.property(fc.array(fc.oneof(fc.string(), fc.integer(), fc.constant(null))), (values) => {
        const expected = values.filter((value): value is string => typeof value === "string");
        expect(arrayLenient(str)(values)).toEqual(expected);
      }),
    );
  });
});

describe("lenient", () => {
  test("returns undefined instead of throwing", () => {
    const decoder = lenient(num);
    expect(decoder(3)).toBe(3);
    expect(decoder("x")).toBeUndefined();
    expect(decoder(Number.NaN)).toBeUndefined();
  });
});

describe("tagged", () => {
  const decoder = tagged("type", {
    text: object({ type: oneOf("text"), text: str }),
    count: object({ type: oneOf("count"), count: num }),
  });

  test("picks the variant named by the tag", () => {
    expect(decoder({ type: "text", text: "hi" })).toEqual({ type: "text", text: "hi" });
    expect(decoder({ type: "count", count: 2 })).toEqual({ type: "count", count: 2 });
  });

  test("rejects an unknown tag, a missing tag, and a non-object", () => {
    expect(rejected(() => decoder({ type: "other" }))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => decoder({}))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => decoder({ type: 1 }))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => decoder(null))).toBeInstanceOf(ProtocolError);
  });

  test("rejects a tag that only exists on the prototype chain", () => {
    expect(rejected(() => decoder({ type: "toString" }))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => decoder({ type: "constructor" }))).toBeInstanceOf(ProtocolError);
  });
});

describe("exhaustive branch coverage", () => {
  test("a number that is not finite is rejected by num and by oneOf", () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(rejected(() => num(value))).toBeInstanceOf(ProtocolError);
      expect(rejected(() => oneOf("a")(value))).toBeInstanceOf(ProtocolError);
    }
  });

  test("a decoder that returns undefined leaves the field out of an object", () => {
    const decoder = object({ a: optional(str) });
    expect(decoder({})).toEqual({});
    expect(decoder({ a: "x" })).toEqual({ a: "x" });
  });

  test("arrayLenient skips a member whose decoder throws a non-ProtocolError", () => {
    const decoder = arrayLenient((value) => {
      if (value === "boom") throw new TypeError("boom");
      return String(value);
    });
    expect(decoder(["a", "boom", "b"])).toEqual(["a", "b"]);
  });

  test("tagged rejects a variant name inherited from Object.prototype", () => {
    const decoder = tagged("kind", { a: object({ kind: oneOf("a") }) });
    expect(rejected(() => decoder({ kind: "toString" }))).toBeInstanceOf(ProtocolError);
    expect(rejected(() => decoder({ kind: "hasOwnProperty" }))).toBeInstanceOf(ProtocolError);
  });
});

describe("diagnostics", () => {
  const diagnostic = "error needs a numeric code and a message";

  const rejectsWith = (fn: () => unknown): void => {
    const error = rejected(fn);
    expect(error.code).toBe(RpcErrorCode.InvalidRequest);
    expect(error.message).toBe(diagnostic);
  };

  test("array reports its diagnostic", () => {
    rejectsWith(() => array(str)("x"));
  });

  test("object reports its diagnostic", () => {
    rejectsWith(() => object({ a: str })("x"));
  });

  test("tagged reports its diagnostic for a non-object and an unknown tag", () => {
    rejectsWith(() => tagged("k", { a: str })("x"));
    rejectsWith(() => tagged("k", { a: str })({ k: "b" }));
  });

  test("tagged reports its diagnostic for a numeric or missing tag", () => {
    rejectsWith(() => tagged("k", { a: str })({ k: 1 }));
    rejectsWith(() => tagged("k", { a: str })({}));
  });

  test("arrayLenient reports its diagnostic", () => {
    rejectsWith(() => arrayLenient(str)("x"));
  });
});

describe("record and raw", () => {
  test("record keeps an object and rejects everything else", () => {
    expect(record({ a: 1 })).toEqual({ a: 1 });
    expect(record({})).toEqual({});
    for (const value of [null, [], "x", 1, undefined]) {
      const error = rejected(() => record(value));
      expect(error.code).toBe(RpcErrorCode.InvalidRequest);
      expect(error.message).toBe(Rejection.ErrorShape);
    }
  });

  test("raw passes any value through, including undefined", () => {
    for (const value of [null, undefined, [], "x", 1, { a: 1 }]) {
      expect(raw(value)).toBe(value);
    }
  });
});
