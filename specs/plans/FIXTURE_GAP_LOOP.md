# Fixture gap loop — coverage & implementation engine

A reusable, multi-agent loop that grows the fixture corpus and the compilers
together: it **ideates** small C fixtures for under-covered constructs,
**captures** the real BCC/MSC oracle output, **commits** the ones our compilers
already reproduce byte-exactly (free coverage), and **implements** the ones they
don't — every change gated on the full suite staying **0-fail on both pools**.

- Harness: [`scripts/fixture_gap_loop.workflow.js`](../../scripts/fixture_gap_loop.workflow.js)
- Backlog (known gaps to clear next): [`fixture-gap-backlog.json`](./fixture-gap-backlog.json)

## Why it's safe (no regressions, by construction)

Each iteration is atomic and the regression gate is the **whole suite on both
compilers**:

- A fixture is committed only if our compiler already matches the oracle (a
  *win*), or after an implementation lands and `verify-all` is still **0-fail on
  both BCC and MSC** (including the new fixture). The corpus only ever grows
  green.
- A failed implementation reverts fully (`git reset --hard $BASE` + remove the
  fixture), leaving the tree clean for the next serial attempt.
- The byte-exact contract is the `sha256` in each `expected/<c>/manifest.toml`;
  the `.OBJ/.ASM` goldens are gitignored (see the goldens-cache convention).

## How to invoke (a batch = "run N")

1. **Compute the next number** (collision-proof numbering is essential — see
   "Known debt"):
   ```
   find fixtures/c -type d -name '[0-9]*-*' -printf '%f\n' | grep -oE '^[0-9]+' | sort -n | tail -1
   ```
   `startNumber = that + 1`.
2. **Read the backlog** `specs/plans/fixture-gap-backlog.json` (an array of
   numberless specs `{slug, area, sub, needs, c_source, notes}`; `needs` =
   which compilers still fail). Pass it as `args.backlog`.
3. **Launch** the workflow:
   ```
   Workflow({
     scriptPath: "scripts/fixture_gap_loop.workflow.js",
     args: { startNumber: <max+1>, implCap: 12, backlog: <contents of fixture-gap-backlog.json> }
   })
   ```
   Optional: override `args.clusters` to aim fresh ideation at specific territory.
4. **After it completes**, independently verify and reconcile:
   ```
   target/debug/xfix verify-all --toolchain ours              # BCC, must be 0 fail
   target/debug/xfix verify-all --toolchain ours --compiler msc  # MSC, must be 0 fail
   git status --short      # tree must be clean
   ```
   Then take the workflow's `next_backlog` (still-open partial halves +
   un-attempted gaps) and **fold it back into `fixture-gap-backlog.json`** for
   the following batch, recovering their `c_source` from the run transcript if
   needed (see "Recovering sources").

The workflow runs in the background and commits locally on `main`. Reword/push
between batches as desired — keep the corpus clean; don't rebase `main` while a
batch is committing.

## Phases

1. **Ideate** (parallel, one agent per cluster) → candidate fixtures.
2. **Capture** (parallel batches) → create each at its assigned number, run both
   oracles (90s timeout), classify `win` / `gap` / `discard`.
3. **Commit wins** (serial) → stage only tracked files; both pools re-verified.
4. **Implement gaps** (serial — shared compiler source + git) → **backlog
   first**, then fresh gaps, capped at `implCap`. Backlog is ordered by the
   launcher (put panics / high-value first).

## Refinements baked in (learned from runs 1–3)

- **Per-compiler split.** If an implementation fixes one compiler byte-exact but
  not the other (both pools still green), it commits the solved half as a
  **single-compiler fixture** (only that compiler's `invocation.<c>.toml`) and
  reports the still-open half (`outcome: partial`). Runs 1–3 used an all-or-
  nothing gate that *threw away* a complete BCC fix (4226) and a complete MSC fix
  (4229) because the sibling wasn't done — both were later reclaimed by hand as
  single-compiler fixtures.
- **Already-passing detection.** A backlog gap may have been closed by an earlier
  commit in the same batch; the impl agent checks first and just commits it.
- **Centralized numbering.** Numbers are assigned in the script from
  `startNumber`; agents are told to use the exact dir verbatim and never invent a
  number.

## Known debt

- **Duplicate fixture numbers.** Runs 2–3 both numbered fresh fixtures from 4213
  (a `startNumber` arg didn't take effect), so ~7 numbers collided
  (4213, 4214, 4220, 4222–4224, 4226); a further ~16 (4145–4164) pre-date this
  work (curated vs bulk overlap). The global namespace is no longer unique. A
  cleanup pass should renumber the newer of each colliding pair to fresh numbers
  above the current max and update any code comments that cite them. The harness
  now assigns numbers centrally to stop *new* collisions, but a one-time cleanup
  of the existing 23 is still outstanding.

## Recovering sources from a run transcript

Run outputs are under
`…/subagents/workflows/<run-id>/agent-*.jsonl`. The capture agents' structured
results carry `c_source` for every `gap`. Recover with a small scan for dict
records having `number` + `c_source` + `verdict == "gap"` (see how
`fixture-gap-backlog.json` was assembled).

## History

| Run | Wins | Implemented | Pools after |
|----:|-----:|------------:|-------------|
| 1 | 4 | 6 | BCC 4139/0, MSC 4014/0 |
| 2 | 5 | 12 | BCC 4156/0, MSC 4031/0 |
| 3 | 6 | 10 (+2 shelved) | BCC 4172/0, MSC 4047/0 |
| reclaim | — | 2 (single-compiler) | BCC 4173/0, MSC 4048/0 |

Cumulative: **30 compiler generalizations + 15 coverage wins**, both pools 100%
green throughout.
