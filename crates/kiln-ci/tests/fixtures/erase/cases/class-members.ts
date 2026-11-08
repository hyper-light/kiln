class C extends A implements I, J {
  private p = 2;
  protected q?: string;
  public readonly r: number = 1;
  static readonly s = 1;
  declare z: number;
  y!: string;
  override n(): void {}
  get v(): number { return 1 }
  set v(x: number) {}
  o?(): void {}
}
