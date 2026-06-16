//! Cross-compiler coverage dashboard. Runs the `verify_ours` path for every
//! compiler in the toolchain registry (`oracle::TOOLCHAINS`) across the whole
//! fixture corpus and renders a self-contained HTML page: an overall
//! scorecard, a per-feature breakdown, and a sortable/filterable
//! fixture×compiler matrix.
//!
//! Because it reuses the same verify path as `xfix verify-all`, its overall
//! per-compiler totals reproduce that command's numbers exactly.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fixture::{Fixture, LoadError};
use crate::harness::{HarnessError, ToolPaths, verify_ours};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    Error,
    /// No `invocation.<compiler>.toml` — this compiler doesn't target the fixture.
    NotApplicable,
}

impl Status {
    fn css(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Error => "err",
            Status::NotApplicable => "na",
        }
    }
    fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "✓",
            Status::Fail => "✗",
            Status::Error => "!",
            Status::NotApplicable => "·",
        }
    }
    /// Sort rank for matrix column sorting: failures float to the top.
    fn rank(self) -> u8 {
        match self {
            Status::Fail => 0,
            Status::Error => 1,
            Status::Pass => 2,
            Status::NotApplicable => 3,
        }
    }
}

struct Row {
    name: String,
    category: &'static str,
    /// One status per compiler, in registry order.
    statuses: Vec<Status>,
}

/// Per-compiler tallies for the summary line `main` prints.
pub struct Summary {
    pub out: PathBuf,
    /// `(compiler, pass, applicable_total)` in registry order.
    pub totals: Vec<(&'static str, usize, usize)>,
}

/// Generate the dashboard to `out`. `jobs` caps worker threads.
///
/// # Errors
/// Returns an error if the fixtures directory can't be read or the HTML can't
/// be written.
pub fn generate(
    workspace_root: &Path,
    jobs: Option<usize>,
    out: &Path,
) -> Result<Summary, Box<dyn std::error::Error>> {
    let compilers: Vec<&'static str> = oracle::TOOLCHAINS.iter().map(|t| t.name).collect();
    let tool_paths = ToolPaths::from_workspace_debug(workspace_root);

    // Every fixture directory (any language/category subtree) that targets at
    // least one registered compiler. Recursive discovery; `None` matches any
    // invocation, then keep those whose compiler is registered.
    let mut paths: Vec<PathBuf> =
        crate::discover_fixtures(&workspace_root.join("fixtures"), None)?
            .into_iter()
            .filter(|p| {
                compilers
                    .iter()
                    .any(|c| p.join(format!("invocation.{c}.toml")).is_file())
            })
            .collect();
    paths.sort();
    let total = paths.len();

    let num_threads = jobs
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(8)
        .max(1);
    let chunk_size = total.div_ceil(num_threads).max(1);

    let rows: Mutex<Vec<Row>> = Mutex::new(Vec::with_capacity(total));
    std::thread::scope(|s| {
        for chunk in paths.chunks(chunk_size) {
            let (compilers, tool_paths, rows) = (&compilers, &tool_paths, &rows);
            s.spawn(move || {
                let mut local = Vec::with_capacity(chunk.len());
                for path in chunk {
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("<unknown>")
                        .to_owned();
                    let statuses = compilers
                        .iter()
                        .map(|c| status_for(path, c, tool_paths))
                        .collect();
                    local.push(Row { category: category(&name), name, statuses });
                }
                rows.lock().expect("rows mutex").extend(local);
            });
        }
    });

    let mut rows = rows.into_inner().expect("rows mutex");
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let totals: Vec<(&'static str, usize, usize)> = compilers
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let mut pass = 0;
            let mut applicable = 0;
            for r in &rows {
                match r.statuses[i] {
                    Status::Pass => {
                        pass += 1;
                        applicable += 1;
                    }
                    Status::Fail | Status::Error => applicable += 1,
                    Status::NotApplicable => {}
                }
            }
            (c, pass, applicable)
        })
        .collect();

    let html = render(&compilers, &rows, &totals);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, html)?;

    Ok(Summary { out: out.to_path_buf(), totals })
}

fn status_for(path: &Path, compiler: &str, tool_paths: &ToolPaths) -> Status {
    match Fixture::load(path, compiler) {
        // No invocation for this compiler → it doesn't target this fixture.
        Err(LoadError::Layout(_)) => Status::NotApplicable,
        Err(_) => Status::Error,
        Ok(fixture) => match verify_ours(&fixture, tool_paths) {
            Ok(diff) if diff.is_empty() => Status::Pass,
            Ok(_) => Status::Fail,
            // Tool not reimplemented yet (e.g. standalone-linker fixtures) →
            // our toolchain doesn't target it, same as a missing invocation.
            Err(HarnessError::ToolNotImplemented(_)) => Status::NotApplicable,
            Err(_) => Status::Error,
        },
    }
}

/// Bucket a fixture by feature from its directory name. A keyword heuristic —
/// good enough for a coverage heatmap; the first matching rule wins. Refine
/// later with an explicit `category` field in `invocation.*.toml` if needed.
fn category(name: &str) -> &'static str {
    const RULES: &[(&str, &[&str])] = &[
        ("float/double", &["float", "double", "-fp-", "fp-", "fpu"]),
        ("union", &["union"]),
        ("bitfield", &["bitfield"]),
        ("struct", &["struct"]),
        ("array 2d/3d", &["2d-array", "3d-array", "2d-arr", "3d-arr", "multidim", "2x2"]),
        ("fn-pointer", &["fn-ptr", "fnptr", "func-ptr", "array-of-fn", "fn-takes-fn"]),
        ("typedef/decl", &["typedef", "knr", "goto", "nested-block", "block-decl", "block-local"]),
        ("string", &["string", "strlen", "strcpy", "strcmp", "str-", "-str"]),
        ("recursion", &["recursive", "recursion", "fib", "factorial", "gcd", "mutual"]),
        ("static-local", &["static-local", "static-fn", "static-arr", "static-int", "static"]),
        ("long", &["long"]),
        ("switch", &["switch"]),
        ("pointer/deref", &["ptr", "pointer", "deref"]),
        ("array 1d", &["arr", "array"]),
        ("loop/control", &["for-", "while", "do-while", "loop", "break", "continue", "ternary", "if-"]),
        ("char/cast", &["char", "cast", "uchar"]),
        ("int/arith", &["int", "arith", "add", "sub", "mul", "div", "mod", "shl", "shr", "xor", "cmp", "mask", "bitnot"]),
    ];
    let lower = name.to_ascii_lowercase();
    for (cat, kws) in RULES {
        if kws.iter().any(|k| lower.contains(k)) {
            return cat;
        }
    }
    "other"
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn pct(pass: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { pass as f64 / total as f64 * 100.0 }
}

fn render(compilers: &[&str], rows: &[Row], totals: &[(&str, usize, usize)]) -> String {
    let mut h = String::with_capacity(1 << 20);
    h.push_str("<!doctype html><html lang=en><head><meta charset=utf-8>");
    h.push_str("<title>retrochains coverage</title>\n");
    h.push_str(STYLE);
    h.push_str("</head><body>\n");
    let _ = write!(h, "<h1>retrochains — cross-compiler coverage</h1>\n<p class=meta>generated {} · {} fixtures · {} compilers</p>\n",
        now_utc(), rows.len(), compilers.len());

    // Overall scorecard.
    h.push_str("<h2>Overall</h2>\n<table class=summary><tr><th>compiler<th>fixtures<th>pass<th>fail<th>pass %</tr>\n");
    for &(c, pass, total) in totals {
        let fail = total - pass;
        let _ = write!(h, "<tr><td>{c}<td class=num>{total}<td class=num>{pass}<td class=num>{fail}<td class=num>{:.2}%</tr>\n", pct(pass, total));
    }
    h.push_str("</table>\n");

    // Per-feature breakdown, one column-group per compiler.
    h.push_str("<h2>By feature</h2>\n<table class=summary><tr><th>category");
    for c in compilers {
        let _ = write!(h, "<th>{c} (pass/total)");
    }
    h.push_str("</tr>\n");
    let cats = ordered_categories(rows);
    for cat in &cats {
        let _ = write!(h, "<tr><td>{}", esc(cat));
        for (i, _) in compilers.iter().enumerate() {
            let (mut p, mut t) = (0usize, 0usize);
            for r in rows.iter().filter(|r| r.category == *cat) {
                match r.statuses[i] {
                    Status::Pass => { p += 1; t += 1; }
                    Status::Fail | Status::Error => t += 1,
                    Status::NotApplicable => {}
                }
            }
            let cls = if t > 0 && p == 0 { " class=\"num zero\"" } else { " class=num" };
            if t == 0 {
                let _ = write!(h, "<td class=num>—");
            } else {
                let _ = write!(h, "<td{cls}>{p}/{t} <span class=dim>({:.0}%)</span>", pct(p, t));
            }
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</table>\n");

    // Fixture × compiler matrix.
    h.push_str("<h2>Fixtures</h2>\n");
    h.push_str("<div class=controls><input id=filter placeholder='filter by name…' oninput=applyFilter()>");
    h.push_str(" <label><input type=checkbox id=failonly onchange=applyFilter()> failures only</label>");
    h.push_str(" <span id=count class=dim></span></div>\n");
    h.push_str("<table id=matrix><thead><tr>");
    h.push_str("<th onclick='sortBy(0)'>fixture<th onclick='sortBy(1)'>category");
    for (i, c) in compilers.iter().enumerate() {
        let _ = write!(h, "<th class=cc onclick='sortBy({})'>{c}", i + 2);
    }
    h.push_str("</tr></thead><tbody>\n");
    for r in rows {
        let has_fail = r.statuses.iter().any(|s| matches!(s, Status::Fail | Status::Error));
        let _ = write!(h, "<tr data-fail={}><td class=fx>{}<td class=cat>{}",
            u8::from(has_fail), esc(&r.name), esc(r.category));
        for s in &r.statuses {
            let _ = write!(h, "<td class='c {}' data-r={}>{}", s.css(), s.rank(), s.glyph());
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody></table>\n");
    h.push_str(SCRIPT);
    h.push_str("</body></html>\n");
    h
}

/// Categories present, ordered by descending pass-gap of the first compiler
/// (so the biggest opportunity sits at the top).
fn ordered_categories(rows: &[Row]) -> Vec<&'static str> {
    use std::collections::BTreeMap;
    let mut gap: BTreeMap<&'static str, i64> = BTreeMap::new();
    for r in rows {
        let e = gap.entry(r.category).or_insert(0);
        // gap measured on the first compiler's perspective is misleading when
        // a fixture is N/A there; count any non-pass across all compilers.
        if r.statuses.iter().any(|s| matches!(s, Status::Fail | Status::Error)) {
            *e += 1;
        }
    }
    let mut cats: Vec<&'static str> = gap.keys().copied().collect();
    cats.sort_by(|a, b| gap[b].cmp(&gap[a]).then(a.cmp(b)));
    cats
}

fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // civil-from-days (Howard Hinnant's algorithm), UTC.
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

const STYLE: &str = "<style>\
body{font:14px/1.5 system-ui,sans-serif;margin:2rem;color:#1a1a1a}\
h1{font-size:1.4rem}h2{font-size:1.1rem;margin-top:2rem}\
.meta,.dim{color:#888}.dim{font-size:.85em}\
table{border-collapse:collapse;margin:.5rem 0}\
.summary td,.summary th{border:1px solid #ddd;padding:.25rem .6rem;text-align:left}\
.summary th{background:#f4f4f4}.num{text-align:right;font-variant-numeric:tabular-nums}\
.zero{color:#b00;font-weight:600}\
.controls{margin:.5rem 0}#filter{padding:.2rem .4rem;width:16rem}\
#matrix{border-collapse:collapse;font-size:13px}\
#matrix th{position:sticky;top:0;background:#f4f4f4;padding:.2rem .5rem;cursor:pointer;border-bottom:2px solid #ccc;text-align:left}\
#matrix th.cc{text-align:center}\
#matrix td{padding:.1rem .5rem;border-bottom:1px solid #f0f0f0}\
.fx{font-family:ui-monospace,monospace}.cat{color:#666}\
td.c{text-align:center;font-weight:600}\
.pass{color:#1a7f37}.fail{color:#cf222e;background:#ffebe9}.err{color:#9a6700;background:#fff8c5}.na{color:#ccc}\
</style>\n";

const SCRIPT: &str = "<script>\n\
const tb=document.querySelector('#matrix tbody');\n\
const rows=[...tb.rows];\n\
let sortCol=0,sortAsc=true;\n\
function sortBy(c){sortAsc=(c===sortCol)?!sortAsc:true;sortCol=c;\n\
 rows.sort((a,b)=>{let x,y;\n\
  if(c<2){x=a.cells[c].textContent;y=b.cells[c].textContent;return sortAsc?x.localeCompare(y):y.localeCompare(x);}\n\
  x=+a.cells[c].dataset.r;y=+b.cells[c].dataset.r;return sortAsc?x-y:y-x;});\n\
 rows.forEach(r=>tb.appendChild(r));applyFilter();}\n\
function applyFilter(){const q=document.getElementById('filter').value.toLowerCase();\n\
 const fo=document.getElementById('failonly').checked;let n=0;\n\
 for(const r of rows){const nm=r.cells[0].textContent.toLowerCase();\n\
  const show=nm.includes(q)&&(!fo||r.dataset.fail==='1');\n\
  r.style.display=show?'':'none';if(show)n++;}\n\
 document.getElementById('count').textContent=n+' shown';}\n\
applyFilter();\n\
</script>\n";
