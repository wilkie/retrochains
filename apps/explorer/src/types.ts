// Shape of apps/explorer/public/fixtures.json (see scripts/gen-fixture-manifest.mjs).

export type Family = "bcc" | "msc";

export interface CompilerEntry {
  /** The oracle's command-line args, e.g. `["-c", "-ms", "HELLO.C"]`. */
  args: string[];
  /** sha256 of the golden `.OBJ` — the byte-exact target. */
  objSha: string;
  objSize: number | null;
  /** The faketime the oracle pinned, in Unix seconds (BCC embeds it in the OBJ). */
  fakeTimeUnix?: number;
}

export interface Fixture {
  /** Path under fixtures/c, e.g. `floating-point/scalar/2478-double-not-obj`. */
  id: string;
  area: string;
  sub: string;
  name: string;
  source: string;
  bcc?: CompilerEntry;
  msc?: CompilerEntry;
}

export interface Manifest {
  generated: string;
  count: number;
  fixtures: Fixture[];
}
