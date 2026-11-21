/** A permission request the worker is waiting on. */
export class PendingPermission {
  readonly #resolve: (optionId: string | undefined) => void;
  readonly #promise: Promise<string | undefined>;

  constructor() {
    let resolve!: (optionId: string | undefined) => void;
    this.#promise = new Promise<string | undefined>((settle) => {
      resolve = settle;
    });
    this.#resolve = resolve;
  }

  /** Settles the turn's wait. A later call does nothing. */
  decide(optionId: string | undefined): void {
    this.#resolve(optionId);
  }

  /** The chosen option id, or `undefined` when the person declined. */
  chosen(): Promise<string | undefined> {
    return this.#promise;
  }
}
