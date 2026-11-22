import { isRecord } from "./jsonrpc.ts";

/**
 * A decoder reads one untrusted value and returns the typed model, or says the
 * shape is wrong. It never throws, so a caller at a boundary cannot forget to
 * handle a bad body.
 *
 * `Reason` carries the field-level detail when a caller wants to log it; the
 * status is the same 400 for every decode refusal, because the problem is
 * always the body.
 */
export type Rejected = { kind: "rejected"; status: number; message: string };

export type Decoded<T> = { kind: "decoded"; value: T } | Rejected;

export type Decoder<T> = (value: unknown) => Decoded<T>;

const invalid = (): Rejected => ({
  kind: "rejected",
  status: 400,
  message: "value has the wrong shape",
});

export const str: Decoder<string> = (value) =>
  typeof value === "string" ? { kind: "decoded", value } : invalid();

export const num: Decoder<number> = (value) =>
  typeof value === "number" && Number.isFinite(value) ? { kind: "decoded", value } : invalid();

export const bool: Decoder<boolean> = (value) =>
  typeof value === "boolean" ? { kind: "decoded", value } : invalid();

export function oneOf<const T extends readonly string[]>(...values: T): Decoder<T[number]> {
  return (value) =>
    typeof value === "string" && (values as readonly string[]).includes(value)
      ? { kind: "decoded", value: value as T[number] }
      : invalid();
}

export function optional<T>(decoder: Decoder<T>): Decoder<T | undefined> {
  return (value) => (value === undefined ? { kind: "decoded", value: undefined } : decoder(value));
}

export function nullable<T>(decoder: Decoder<T>): Decoder<T | null> {
  return (value) => (value === null ? { kind: "decoded", value: null } : decoder(value));
}

export function array<T>(decoder: Decoder<T>): Decoder<T[]> {
  return (value) => {
    if (!Array.isArray(value)) return invalid();
    const out: T[] = [];
    for (const item of value) {
      const decoded = decoder(item);
      if (decoded.kind === "rejected") return decoded;
      out.push(decoded.value);
    }
    return { kind: "decoded", value: out };
  };
}

/**
 * The object a `shape` decodes to: a field whose decoder can return
 * `undefined` becomes an optional property, so the key is absent when the
 * agent omitted it. Every other field is required.
 */
export type Shape<S extends Record<string, Decoder<unknown>>> = {
  [
    K in keyof S as S[K] extends Decoder<infer V> ? (undefined extends V ? never : K) : never
  ]: S[K] extends Decoder<infer V> ? V : never;
} & {
  [
    K in keyof S as S[K] extends Decoder<infer V> ? (undefined extends V ? K : never) : never
  ]?: S[K] extends Decoder<infer V> ? Exclude<V, undefined> : never;
};

/**
 * An object whose fields are decoded by `shape`. A field declared `optional`
 * may be absent; every other declared field must be present. Keys the shape
 * does not declare are dropped, so an agent's extension fields never reach the
 * typed model.
 */
export function object<S extends Record<string, Decoder<unknown>>>(shape: S): Decoder<Shape<S>> {
  return (value) => {
    if (!isRecord(value)) return invalid();
    const out: Record<string, unknown> = {};
    for (const [key, decode] of Object.entries(shape)) {
      const decoded = (decode as Decoder<unknown>)(value[key]);
      if (decoded.kind === "rejected") return decoded;
      if (decoded.value !== undefined) out[key] = decoded.value;
    }
    return { kind: "decoded", value: out as Shape<S> };
  };
}

export function tagged<R>(
  tag: string,
  variants: { readonly [key: string]: Decoder<unknown> },
): Decoder<R> {
  const byTag = new Map(Object.entries(variants));
  return (value) => {
    if (!isRecord(value)) return invalid();
    const name = value[tag];
    if (typeof name !== "string") return invalid();
    const variant = byTag.get(name);
    if (variant === undefined) return invalid();
    return variant(value) as Decoded<R>;
  };
}

/** The value a decoder would reject, read as absent instead. */
export function lenient<T>(decoder: Decoder<T>): Decoder<T | undefined> {
  return (value) => {
    const decoded = decoder(value);
    return decoded.kind === "decoded" ? decoded : { kind: "decoded", value: undefined };
  };
}

/** The items a decoder accepts, dropping the ones it rejects. */
export function arrayLenient<T>(decoder: Decoder<T>): Decoder<T[]> {
  return (value) => {
    if (!Array.isArray(value)) return invalid();
    const out: T[] = [];
    for (const item of value) {
      const decoded = decoder(item);
      if (decoded.kind === "decoded") out.push(decoded.value);
    }
    return { kind: "decoded", value: out };
  };
}

export const record: Decoder<Record<string, unknown>> = (value) =>
  isRecord(value) ? { kind: "decoded", value } : invalid();

export const raw: Decoder<unknown> = (value) => ({ kind: "decoded", value });

/** The value of a decode, or `undefined`; for a caller that treats absence as absent. */
export function valueOf<T>(decoded: Decoded<T>): T | undefined {
  return decoded.kind === "decoded" ? decoded.value : undefined;
}
