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
  valueOf,
  type Decoded,
} from "./decode.ts";

/** The refusal a decoder returned, or a failure naming what it returned instead. */
function refusal<T>(decoded: Decoded<T>): { status: number; message: string } {
  if (decoded.kind !== "rejected") {
    throw new Error(`expected a refusal, got a value: ${JSON.stringify(decoded)}`);
  }
  expect(decoded.kind).toBe("rejected");
  return { status: decoded.status, message: decoded.message };
}

/** The value a decoder returned, or a failure naming the refusal. */
function value<T>(decoded: Decoded<T>): T {
  if (decoded.kind !== "decoded") throw new Error("expected a value");
  expect(decoded.kind).toBe("decoded");
  return decoded.value;
}

describe("primitives", () => {
  test("str keeps strings and rejects everything else", () => {
    expect(value(str("a"))).toBe("a");
    expect(value(str(""))).toBe("");
    for (const input of [1, null, undefined, {}, ["a"], true]) {
      expect(refusal(str(input)).status).toBe(400);
    }
  });

  test("num keeps finite numbers and rejects the rest", () => {
    expect(value(num(0))).toBe(0);
    for (const input of ["1", null, undefined, {}, [], true, Number.NaN]) {
      expect(refusal(num(input)).status).toBe(400);
    }
  });

  test("bool keeps booleans and rejects the rest", () => {
    expect(value(bool(true))).toBe(true);
    expect(value(bool(false))).toBe(false);
    for (const input of [0, 1, "true", null, undefined]) {
      expect(refusal(bool(input)).status).toBe(400);
    }
  });

  test("a rejection names the shape, not the field", () => {
    expect(refusal(str(1))).toEqual({ status: 400, message: "value has the wrong shape" });
  });
});

describe("oneOf", () => {
  const kind = oneOf("a", "b");

  test("accepts only the listed values", () => {
    expect(value(kind("a"))).toBe("a");
    expect(value(kind("b"))).toBe("b");
    expect(refusal(kind("c")).status).toBe(400);
    expect(refusal(kind(1)).status).toBe(400);
    expect(refusal(kind(undefined)).status).toBe(400);
  });

  test("a property test agrees with set membership", () => {
    fc.assert(
      fc.property(fc.string(), (input) => {
        const accepted = input === "a" || input === "b";
        expect(kind(input).kind === "decoded").toBe(accepted);
      }),
    );
  });
});

describe("optional and nullable", () => {
  test("optional passes undefined through and decodes anything else", () => {
    const decoder = optional(str);
    expect(value(decoder(undefined))).toBeUndefined();
    expect(value(decoder("x"))).toBe("x");
    expect(refusal(decoder(null)).status).toBe(400);
  });

  test("nullable passes null through and decodes anything else", () => {
    const decoder = nullable(str);
    expect(value(decoder(null))).toBeNull();
    expect(value(decoder("x"))).toBe("x");
    expect(refusal(decoder(undefined)).status).toBe(400);
  });
});

describe("object", () => {
  const decoder = object({ id: str, count: num, note: optional(str) });

  test("keeps declared fields and drops the rest", () => {
    expect(value(decoder({ id: "a", count: 2, note: "n", extra: true }))).toEqual({
      id: "a",
      count: 2,
      note: "n",
    });
  });

  test("an omitted optional key stays absent", () => {
    const decoded = value(decoder({ id: "a", count: 2 }));
    expect(decoded).toEqual({ id: "a", count: 2 });
    expect(Object.keys(decoded)).toEqual(["id", "count"]);
  });

  test("a missing required field is a rejection", () => {
    expect(refusal(decoder({ id: "a" })).status).toBe(400);
  });

  test("a non-object is a rejection", () => {
    for (const input of [null, [], "x", 1]) {
      expect(refusal(decoder(input)).status).toBe(400);
    }
  });

  test("a field that decodes to undefined is left out", () => {
    const drop = object({
      kept: str,
      dropped: (): Decoded<string | undefined> => ({ kind: "decoded", value: undefined }),
    });
    const decoded = value(drop({ kept: "k", dropped: "v" }));
    expect(decoded).toEqual({ kept: "k" });
    expect(Object.keys(decoded)).toEqual(["kept"]);
  });
});

describe("array", () => {
  test("decodes every member and rejects a non-array", () => {
    expect(value(array(str)(["a", "b"]))).toEqual(["a", "b"]);
    expect(value(array(str)([]))).toEqual([]);
    expect(refusal(array(str)("a")).status).toBe(400);
    expect(refusal(array(str)(["a", 1])).status).toBe(400);
  });

  test("arrayLenient keeps the members that decode and skips the rest", () => {
    const decoder = arrayLenient(str);
    expect(value(decoder(["a", 1, "b", null, "c"]))).toEqual(["a", "b", "c"]);
    expect(value(decoder([]))).toEqual([]);
    expect(refusal(decoder("a")).status).toBe(400);
  });

  test("a property test keeps exactly the decodable members in order", () => {
    fc.assert(
      fc.property(fc.array(fc.oneof(fc.string(), fc.integer(), fc.constant(null))), (values) => {
        const expected = values.filter((item): item is string => typeof item === "string");
        expect(value(arrayLenient(str)(values))).toEqual(expected);
      }),
    );
  });
});

describe("lenient", () => {
  test("returns undefined instead of a refusal", () => {
    const decoder = lenient(num);
    expect(value(decoder(3))).toBe(3);
    expect(value(decoder("x"))).toBeUndefined();
    expect(value(decoder(Number.NaN))).toBeUndefined();
  });
});

describe("tagged", () => {
  const decoder = tagged("type", {
    text: object({ type: oneOf("text"), text: str }),
    count: object({ type: oneOf("count"), count: num }),
  });

  test("picks the variant named by the tag", () => {
    expect(value(decoder({ type: "text", text: "hi" }))).toEqual({ type: "text", text: "hi" });
    expect(value(decoder({ type: "count", count: 2 }))).toEqual({ type: "count", count: 2 });
  });

  test("rejects an unknown tag, a missing tag, and a non-object", () => {
    expect(refusal(decoder({ type: "other" })).status).toBe(400);
    expect(refusal(decoder({})).status).toBe(400);
    expect(refusal(decoder({ type: 1 })).status).toBe(400);
    expect(refusal(decoder(null)).status).toBe(400);
  });

  test("rejects a tag that only exists on the prototype chain", () => {
    expect(refusal(decoder({ type: "toString" })).status).toBe(400);
    expect(refusal(decoder({ type: "constructor" })).status).toBe(400);
  });
});

describe("exhaustive branch coverage", () => {
  test("a number that is not finite is rejected by num and by oneOf", () => {
    for (const input of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
      expect(refusal(num(input)).status).toBe(400);
      expect(refusal(oneOf("a")(input)).status).toBe(400);
    }
  });

  test("a decoder that returns undefined leaves the field out of an object", () => {
    const decoder = object({ a: optional(str) });
    expect(value(decoder({}))).toEqual({});
    expect(value(decoder({ a: "x" }))).toEqual({ a: "x" });
  });

  test("arrayLenient skips a member whose decoder refuses it", () => {
    const decoder = arrayLenient((input) =>
      input === "boom"
        ? { kind: "rejected", status: 400, message: "boom" }
        : { kind: "decoded", value: String(input) },
    );
    expect(value(decoder(["a", "boom", "b"]))).toEqual(["a", "b"]);
  });

  test("tagged rejects a variant name inherited from Object.prototype", () => {
    const decoder = tagged("kind", { a: object({ kind: oneOf("a") }) });
    expect(refusal(decoder({ kind: "toString" })).status).toBe(400);
    expect(refusal(decoder({ kind: "hasOwnProperty" })).status).toBe(400);
  });
});

describe("diagnostics", () => {
  test("every decoder reports the same shape message", () => {
    const message = "value has the wrong shape";
    expect(refusal(array(str)("x")).message).toBe(message);
    expect(refusal(object({ a: str })("x")).message).toBe(message);
    expect(refusal(tagged("k", { a: str })("x")).message).toBe(message);
    expect(refusal(tagged("k", { a: str })({ k: "b" })).message).toBe(message);
    expect(refusal(arrayLenient(str)("x")).message).toBe(message);
  });
});

describe("record and raw", () => {
  test("record keeps an object and rejects everything else", () => {
    expect(value(record({ a: 1 }))).toEqual({ a: 1 });
    expect(value(record({}))).toEqual({});
    for (const input of [null, [], "x", 1, undefined]) {
      const answer = refusal(record(input));
      expect(answer.status).toBe(400);
      expect(answer.message).toBe("value has the wrong shape");
    }
  });

  test("raw passes any value through, including undefined", () => {
    for (const input of [null, undefined, [], "x", 1, { a: 1 }]) {
      expect(value(raw(input))).toBe(input);
    }
  });
});

describe("valueOf", () => {
  test("returns the value of a decode and undefined for a refusal", () => {
    expect(valueOf(str("a"))).toBe("a");
    expect(valueOf(str(1))).toBeUndefined();
  });
});

describe("the exact branch each primitive takes", () => {
  test("num rejects a non-number before it checks finiteness", () => {
    expect(str(1)).toEqual({ kind: "rejected", status: 400, message: "value has the wrong shape" });
    expect(num("1")).toEqual({
      kind: "rejected",
      status: 400,
      message: "value has the wrong shape",
    });
    expect(bool("true")).toEqual({
      kind: "rejected",
      status: 400,
      message: "value has the wrong shape",
    });
  });

  test("oneOf rejects a non-string before it looks at membership", () => {
    expect(oneOf("a")(1)).toEqual({
      kind: "rejected",
      status: 400,
      message: "value has the wrong shape",
    });
  });

  test("lenient turns a refusal into an absent value and keeps a value", () => {
    expect(lenient(num)("x")).toEqual({ kind: "decoded", value: undefined });
    expect(lenient(num)(2)).toEqual({ kind: "decoded", value: 2 });
  });

  test("valueOf reads a value and an absent refusal the same way the caller expects", () => {
    expect(valueOf(num(2))).toBe(2);
    expect(valueOf(num("x"))).toBeUndefined();
  });
});
