/**
 * A request's key, kept while the same request is tried again (CP-T3.2): asked again after a
 * failure or a lost answer, it's the same request, never a second; anything else asked is new.
 */
export class Tries {
  private last: { said: string; key: string } | null = null;

  constructor(private readonly make: () => string) {}

  /** The key for `said` (the request as it would be sent): the last try's, if it's the same. */
  keyFor(said: string): string {
    const key = this.last?.said === said ? this.last.key : this.make();
    this.last = { said, key };
    return key;
  }

  /** It went through: the next request is new. */
  done(): void {
    this.last = null;
  }
}
