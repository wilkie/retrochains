#!/usr/bin/env python3
"""Print every method of the FunctionEmitter impl, one per line, sorted by
size descending, with its assigned module. For tuning the split rules."""
import sys
want = sys.argv[1] if len(sys.argv) > 1 else None
sys.argv = sys.argv[:1]  # keep split's arg parsing quiet
import importlib.util
spec = importlib.util.spec_from_file_location("split", "scripts/codegen_split/split.py")
split = importlib.util.module_from_spec(spec)
# prevent split.main() autorun
import builtins
spec.loader.exec_module(split)

from pathlib import Path
lines = Path("crates/bcc/src/codegen/mod.rs").read_text().splitlines()
impl_start = next(i for i, l in enumerate(lines) if l.rstrip() == split.TARGET_IMPL)
impl_end = split.span_from(lines, impl_start)
import re
method_re = re.compile(r'^    (pub(\([^)]*\))? )?(unsafe )?fn (\w+)')
ms = []
i = impl_start + 1
while i < impl_end:
    m = method_re.match(lines[i])
    if m:
        e = split.span_from(lines, i)
        ms.append((m.group(4), e - i + 1, split.categorize(m.group(4))))
        i = e + 1
    else:
        i += 1

for name, size, mod in sorted(ms, key=lambda t: -t[1]):
    if want and mod != want:
        continue
    print(f"{size:5d}  {mod:12s}  {name}")
