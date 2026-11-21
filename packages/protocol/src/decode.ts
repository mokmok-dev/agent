import { isRecord, reject } from "./jsonrpc.ts";

/**
 * A decoder reads one untrusted value and returns the typed model. It throws
 * `ProtocolError` (`InvalidRequest`) on anything it cannot read, so callers
 * never see a partially valid object.
 */
export type Decoder<T> = (value: unknown) => T;

const fail: Decoder<never> = () => {
  throw reject("ErrorShape");
};

export const str: Decoder<string> = (value) => (typeof value === "string" ? value : fail(value));

export const num: Decoder<number> = (value) =>
  Number.isFinite(value) ? (value as number) : fail(value);

export const bool: Decoder<boolean> = (value) => (typeof value === "boolean" ? value : fail(value));

export function oneOf<const T extends readonly string[]>(...values: T): Decoder<T[number]> {
  return (value) =>
    (values as readonly string[]).includes(value as string) ? (value as T[number]) : fail(value);
}

export function optional<T>(decoder: Decoder<T>): Decoder<T | undefined> {
  return (value) => (value === undefined ? undefined : decoder(value));
}

export function nullable<T>(decoder: Decoder<T>): Decoder<T | null> {
  return (value) => (value === null ? null : decoder(value));
}

export function array<T>(decoder: Decoder<T>): Decoder<T[]> {
  return (value) => {
    if (!Array.isArray(value)) throw reject("ErrorShape");
    return value.map(decoder);
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
    if (!isRecord(value)) throw reject("ErrorShape");
    const out: Record<string, unknown> = {};
    for (const [key, decode] of Object.entries(shape)) {
      const decoded = (decode as Decoder<unknown>)(value[key]);
      if (decoded !== undefined) out[key] = decoded;
    }
    return out as Shape<S>;
  };
}

export function tagged<R>(
  tag: string,
  variants: { readonly [key: string]: Decoder<unknown> },
): Decoder<R> {
  const byTag = new Map(Object.entries(variants));
  return (value) => {
    if (!isRecord(value)) throw reject("ErrorShape");
    const variant = byTag.get(str(value[tag]));
    if (!variant) throw reject("ErrorShape");
    return variant(value) as R;
  };
}

function discard<T>(run: () => T): T | undefined {
  try {
    return run();
  } catch {
    // The decoder rejected the value: the caller treats it as absent.
  }
}

export function lenient<T>(decoder: Decoder<T>): Decoder<T | undefined> {
  return (value) => discard(() => decoder(value));
}

export function arrayLenient<T>(decoder: Decoder<T>): Decoder<T[]> {
  return (value) => {
    if (!Array.isArray(value)) throw reject("ErrorShape");
    const out: T[] = [];
    for (const item of value) {
      const decoded = discard(() => decoder(item));
      if (decoded !== undefined) out.push(decoded);
    }
    return out;
  };
}

export const record: Decoder<Record<string, unknown>> = (value) => {
  if (!isRecord(value)) throw reject("ErrorShape");
  return value;
};

export const raw: Decoder<unknown> = (value) => value;
