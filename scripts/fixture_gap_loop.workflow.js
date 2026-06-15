// fixture_gap_loop.workflow.js — reusable coverage/implementation loop.
//
// Invoke with the Workflow tool:
//   Workflow({ scriptPath: "scripts/fixture_gap_loop.workflow.js",
//              args: { startNumber: <max+1>, implCap: 12, backlog: <specs[]>, clusters: <optional[]> } })
//
// What it does (one batch):
//   Ideate fresh C fixtures for under-covered constructs -> capture both oracles ->
//   commit the ones our compilers already reproduce (free coverage wins) ->
//   implement the gaps (BACKLOG first), each gated on BOTH pools staying 0-fail.
//
// CONTRACT / SAFETY:
//   - The corpus only ever grows with PASSING fixtures; both pools stay 100% green.
//   - Numbers are assigned sequentially from args.startNumber (== current global max + 1).
//     Agents MUST use the exact assigned dir/number — never invent or reuse one
//     (runs 2-3 drifted by ignoring this; see specs/plans/FIXTURE_GAP_LOOP.md).
//   - PER-COMPILER SPLIT: if an implementation closes ONE compiler byte-exact (both
//     pools green) but not the other, commit the solved half as a SINGLE-COMPILER
//     fixture (only that compiler's invocation.<c>.toml) and report the other half
//     so it can be re-queued — partial progress is never thrown away.
//
// args:
//   startNumber : integer — first number to assign (compute as current max + 1 before launching).
//   implCap     : integer — max serial implementation attempts this batch (default 12).
//   backlog     : array of specs { slug, area, sub, needs:["bcc"|"msc"...], c_source, notes } — known gaps,
//                 implemented first. needs = which compilers still fail. No numbers (assigned here).
//   clusters    : optional override of fresh-ideation clusters [{ key, area, sub, desc }].

export const meta = {
  name: 'fixture-gap-loop',
  description: 'Coverage/implementation loop: ideate fresh C fixtures + clear a known-gap backlog; commit free wins and implement gaps, never regressing either compiler pool',
  phases: [
    { title: 'Ideate' },
    { title: 'Capture' },
    { title: 'Commit wins' },
    { title: 'Implement gaps' },
  ],
}

const START = (args && args.startNumber) || 5000
const IMPL_CAP = (args && args.implCap) || 12
const BACKLOG = (args && args.backlog) || []

const DEFAULT_CLUSTERS = [
  { key: 'control-flow', area: 'control-flow', sub: 'loops', desc: 'do-while with break/continue, goto/labels, switch density variants, nested loops, deeply nested conditionals.' },
  { key: 'multidim-arrays', area: 'arrays', sub: 'multidim', desc: '2D/3D arrays, array-of-array, pointer-to-row, passing arrays with omitted leading dimension to functions.' },
  { key: 'aggregates', area: 'aggregates', sub: 'struct', desc: 'struct return-by-value, struct assignment/copy, nested structs, array-of-struct, signed/spanning bitfields, unions of mismatched widths.' },
  { key: 'pointers', area: 'pointers', sub: 'arithmetic', desc: 'pointer arithmetic and difference, array decay, pointer-to-pointer walks, comparing pointers, far/huge pointer ops, address-of function.' },
  { key: 'expressions', area: 'expressions', sub: 'bitwise', desc: 'shift by variable, signed vs unsigned shifts, signed div/mod with negatives, strength reduction, nested ternary, comma sequencing, casts between widths.' },
  { key: 'types-lib', area: 'types', sub: 'integer', desc: 'typedef, enum corners, const/volatile/register qualifiers, more stdio/string-lib calls (printf specifiers, strcpy/strcat), sizeof corners.' },
  { key: 'preprocessor', area: 'preprocessor', sub: 'macros', desc: 'token-paste ##, stringize #, nested/recursive-ish macro expansion, macros used as array dimensions and in arithmetic, #line/#pragma. Must expand identically on both 1987-MSC and 1991-Borland.' },
]
const clusters = (args && args.clusters) || DEFAULT_CLUSTERS

const CONV = `
PROJECT: retrochains — byte-exact reimplementation of Borland C++ 2.0 (bcc, 1991) and
Microsoft C 5.0 (cl, 1987) for 16-bit DOS. Work from repo root /home/wilkie/retrochains.
Binaries at target/debug/{xfix,bcc,msc} (rebuild after editing crates/).

FIXTURE dir: fixtures/c/<area>/<sub>/<NNNN>-<slug>-obj/ with HELLO.C and the invocation toml(s):
  invocation.bcc.toml:
tool = "bcc"
args = ["-c", "-ms", "HELLO.C"]
inputs = ["HELLO.C"]
asm_args = ["-S", "-ms", "HELLO.C"]
  invocation.msc.toml:
tool = "cl"
args = ["/c", "/Fa", "/AS", "HELLO.C"]
inputs = ["HELLO.C"]
A fixture may target ONE compiler (include only that invocation toml) or both.

NUMBERING: use the EXACT directory path / number you are given. NEVER invent, increment,
or reuse a number — number assignment is centralized to keep the global namespace unique.

C SOURCE: strict K&R/C89, compiling UNCHANGED on BOTH 1987-MSC-5.0 and 1991-Borland-2.0,
DOS small model, 16-bit int. No // comments, no C99 (no decl-after-stmt, long long, _Bool,
compound/designated initializers, mixed decl/code). Declarations at top of block. main()
returns int. Deterministic, minimal.

COMMANDS (capture needs DOSBox; wrap EACH capture in 'timeout 90'):
  target/debug/xfix capture --compiler {bcc,msc} <dir>   (oracle exit lands in expected/<c>/manifest.toml exit_code; nonzero = compile failure)
  target/debug/xfix verify --toolchain ours --compiler {bcc,msc} <dir>   (exit 0 = byte-exact OBJ match)
  target/debug/xfix verify-all --toolchain ours                 (BCC pool — must be 0 fail)
  target/debug/xfix verify-all --toolchain ours --compiler msc  (MSC pool — must be 0 fail)

GIT: never 'git add -A'. Stage explicit paths only. .OBJ/.ASM goldens are gitignored — never add them
(check: git diff --cached --name-only | grep -ciE '\\.(obj|asm)$'  must print 0). Commit footer:
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
`

const IDEA_SCHEMA = { type:'object', additionalProperties:false, required:['candidates'], properties:{
  candidates:{ type:'array', items:{ type:'object', additionalProperties:false,
    required:['slug','area','sub','c_source','rationale'], properties:{
      slug:{type:'string'}, area:{type:'string'}, sub:{type:'string'}, c_source:{type:'string'}, rationale:{type:'string'} } } } } }
const CLASSIFY_SCHEMA = { type:'object', additionalProperties:false, required:['results'], properties:{
  results:{ type:'array', items:{ type:'object', additionalProperties:false,
    required:['number','slug','dir','verdict'], properties:{
      number:{type:'integer'}, slug:{type:'string'}, area:{type:'string'}, sub:{type:'string'}, dir:{type:'string'},
      c_source:{type:'string'}, verdict:{type:'string', enum:['win','gap','discard']},
      bcc_ours:{type:'string', enum:['pass','fail','na']}, msc_ours:{type:'string', enum:['pass','fail','na']}, notes:{type:'string'} } } } } }
const COMMIT_SCHEMA = { type:'object', additionalProperties:false, required:['committed','bcc_green','msc_green'], properties:{
  committed:{type:'integer'}, commit_sha:{type:'string'}, bcc_green:{type:'boolean'}, msc_green:{type:'boolean'}, notes:{type:'string'} } }
const IMPL_SCHEMA = { type:'object', additionalProperties:false, required:['slug','outcome'], properties:{
  slug:{type:'string'}, outcome:{type:'string', enum:['implemented','partial','already-passing','shelved']},
  compilers_fixed:{type:'array', items:{type:'string'}}, still_open:{type:'array', items:{type:'string'}},
  commit_sha:{type:'string'}, root_cause:{type:'string'}, change_summary:{type:'string'},
  bcc_green:{type:'boolean'}, msc_green:{type:'boolean'} } }

function ideatePrompt(c) {
  return CONV + `
TASK: Propose exactly 3 MINIMAL, VARIED C fixtures for this cluster:
  ${c.key}  (area/sub ${c.area}/${c.sub}) — focus: ${c.desc}
Each is a complete HELLO.C compiling UNCHANGED on both oracles, with main() returning a construct-dependent int.
Short kebab-case slug (no number, no -obj). Return via schema; do not create files or run anything.`
}
function capturePrompt(batch) {
  const list = batch.map(c => `- USE EXACTLY: number ${c.number}, dir ${c.dir}\n  HELLO.C:\n${c.c_source}`).join('\n')
  return CONV + `
TASK: For EACH candidate, create the fixture at the EXACT dir given (both invocation tomls), capture both
oracles ('timeout 90' each), classify, clean up. Use the assigned number/dir verbatim — do NOT renumber.
${list}
Per candidate: 1) write files. 2) capture both. timeout/error -> discard. 3) if either oracle exit_code != 0 -> discard.
4) else verify ours for both: both pass -> win (leave dir, don't commit); both compiled but >=1 ours fails -> gap
(record the failing verify stderr in notes, keep c_source, then rm -rf dir). 5) discard -> rm -rf dir.
Return one result per candidate via schema.`
}
function commitPrompt(wins) {
  const dirs = wins.map(w => w.dir).join(' ')
  return CONV + `
TASK: commit these already-passing fixtures as free coverage wins.
WIN DIRS (${wins.length}): ${dirs || '(none)'}
1) if none, report committed=0 and still run step 3. 2) 'git add <each dir>' (explicit; confirm 0 goldens staged);
commit "fixtures: +N coverage fixtures our compilers already match" (list areas) + footer. 3) run BOTH verify-all
pools; confirm 0 fail. Report committed, sha, per-pool green.`
}
function implPrompt(g) {
  const needs = (g.needs && g.needs.length) ? g.needs.join('+') : 'bcc+msc'
  return CONV + `
TASK (byte-exact reverse-engineering): make our compiler(s) reproduce this fixture without regressing either pool.
USE EXACTLY: number ${g.number}, dir ${g.dir}.  Compilers that still need work: ${needs}.
HELLO.C:
${g.c_source}
Known failure signature: ${g.notes || '(investigate)'}

1) BASE=$(git rev-parse HEAD). 2) Create the fixture dir. Include invocation tomls for the compiler(s) in [${needs}]
   (if only one, make it a single-compiler fixture). Capture those oracle(s) ('timeout 90'); confirm exit_code 0.
3) ALREADY FIXED? verify ours for the targeted compiler(s). If all targeted already pass, commit as a win
   (stage <dir>, commit, footer), outcome=already-passing, done.
4) Else study the diff (BCC: scripts/objdis.py; MSC: scripts/mscdiff.sh ${g.number}). Find the GENERAL rule.
5) Implement a MINIMAL, GENERAL fix in crates/bcc/ or crates/msc/ (no special-casing, no weakening). cargo build --workspace --bins.
6) GATE: both verify-all pools 0 fail AND the fixture passes on every compiler it targets.
7a) ALL targeted compilers byte-exact + both pools green: 'git add crates/ <dir>' (no goldens), commit
    "<compiler>: <feature> (${g.number}) — implements ${g.slug}" + footer. outcome=implemented.
7b) PER-COMPILER SPLIT — you fixed SOME but not all targeted compilers, both pools still green: KEEP only the
    solved compiler's invocation toml in <dir> (rm the unsolved one), 'git add crates/ <dir>', commit
    "<compiler>: <feature> (${g.number}) — partial, <other> half open" + footer. outcome=partial,
    compilers_fixed=[...], still_open=[the unsolved compiler]. Record root_cause/change_summary for re-queue.
7c) Fixed NOTHING / regressed: 'git reset --hard $BASE'; 'rm -rf ${g.dir}'; 'cargo build --workspace --bins'
    (no git clean, don't touch other dirs). outcome=shelved; detailed root_cause + change_summary.
Report via schema.`
}

// ---- number assignment (centralized, collision-proof) ----------------------
let next = START
const backlogTargets = BACKLOG.map(b => ({ ...b, number: next, dir: `fixtures/c/${b.area}/${b.sub}/${next++}-${b.slug}-obj` }))

// ---- Phase 1: Ideate fresh -------------------------------------------------
phase('Ideate')
const ideaResults = (await parallel(clusters.map(c => () =>
  agent(ideatePrompt(c), { schema: IDEA_SCHEMA, phase:'Ideate', label:'ideate:'+c.key })))).filter(Boolean)
let candidates = []
for (const r of ideaResults) for (const cand of (r.candidates || [])) if (cand && cand.slug && cand.c_source) candidates.push(cand)
const seen = new Set()
candidates = candidates.filter(c => !seen.has(c.slug) && seen.add(c.slug))
candidates.forEach(c => { c.number = next; c.dir = `fixtures/c/${c.area}/${c.sub}/${next++}-${c.slug}-obj` })
log(`backlog: ${backlogTargets.length}; fresh ideated: ${candidates.length}; numbers ${START}..${next - 1}`)

// ---- Phase 2: Capture + classify fresh ------------------------------------
phase('Capture')
const batches = []
for (let i = 0; i < candidates.length; i += 4) batches.push(candidates.slice(i, i + 4))
const classifyResults = (await parallel(batches.map((b, bi) => () =>
  agent(capturePrompt(b), { schema: CLASSIFY_SCHEMA, phase:'Capture', label:'capture:batch'+bi })))).filter(Boolean)
let classified = []
for (const r of classifyResults) for (const x of (r.results || [])) classified.push(x)
const freshWins = classified.filter(c => c.verdict === 'win')
const freshGaps = classified.filter(c => c.verdict === 'gap')
log(`fresh: ${freshWins.length} wins, ${freshGaps.length} gaps, ${classified.filter(c=>c.verdict==='discard').length} discards`)

// ---- Phase 3: Commit fresh wins -------------------------------------------
phase('Commit wins')
const commitRes = await agent(commitPrompt(freshWins), { schema: COMMIT_SCHEMA, phase:'Commit wins', label:'commit-wins' })

// ---- Phase 4: Implement — BACKLOG first, then fresh (SERIAL) ---------------
phase('Implement gaps')
const allGaps = backlogTargets.concat(freshGaps)
const toImpl = allGaps.slice(0, IMPL_CAP)
const implResults = []
for (const g of toImpl) {
  const r = await agent(implPrompt(g), { schema: IMPL_SCHEMA, phase:'Implement gaps', label:'impl:'+g.slug })
  if (r) implResults.push(r)
  log(`impl ${g.slug}: ${r ? r.outcome : 'agent-died'}`)
}

return {
  backlog: backlogTargets.length,
  fresh_wins: freshWins.length,
  fresh_gaps: freshGaps.length,
  fresh_win_commit: commitRes,
  gaps_attempted: toImpl.length,
  implemented: implResults.filter(r => r.outcome === 'implemented').length,
  partial: implResults.filter(r => r.outcome === 'partial').length,
  already_passing: implResults.filter(r => r.outcome === 'already-passing').length,
  shelved: implResults.filter(r => r.outcome === 'shelved').length,
  impl_details: implResults,
  // Re-queue for the NEXT batch: still-open halves from partials + un-attempted gaps.
  next_backlog: implResults.filter(r => r.outcome === 'partial').map(r => ({ slug: r.slug, still_open: r.still_open }))
    .concat(allGaps.slice(IMPL_CAP).map(g => ({ slug: g.slug, needs: g.needs || ['bcc','msc'] }))),
}
