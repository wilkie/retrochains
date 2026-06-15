# artifacts/ — the fixture golden cache

This directory holds the **gitignored, reproducible byte cache** for the fixture
corpus: the exact output each oracle compiler produced for a fixture, plus its
captured `stdout`/`stderr`. Everything here except this README is ignored by git.

## Why it exists

The byte-exact contract the harness gates on is the **`sha256`** recorded for
every output in each fixture's tracked manifest
(`fixtures/<path>/expected/<compiler>/<RELEASE>.toml`). The multi-megabyte bytes
themselves are *not* tracked — they're a cache that can be regenerated from the
oracle at any time. Keeping them out of the `fixtures/` tree (where they were
interleaved with tracked files across ~16k directories) lets git prune the whole
cache in a single step, so `git status` doesn't walk tens of thousands of
ignored files.

## Layout

The cache mirrors the fixtures tree, then keys by compiler **family** and oracle
**release**:

```
artifacts/<fixture-path>/<family>/<release>/<output files + stdout + stderr>

e.g.  artifacts/c/aggregates/bitfields/1691-bitfield-struct-obj/bcc/BC2/HELLO.OBJ
      artifacts/c/aggregates/bitfields/1691-bitfield-struct-obj/bcc/BC2/HELLO.ASM
      artifacts/c/aggregates/bitfields/1691-bitfield-struct-obj/bcc/BC2/stdout
```

- `<fixture-path>` mirrors the path under `fixtures/`.
- `<family>` is the compiler family (`bcc`, `msc`) — same as `--compiler` and
  the `oracles/<family>/` directory.
- `<release>` is the specific oracle release whose bytes these are (`BC2`,
  `MSC500`) — matching `oracles/<family>/<release>.toml`. Output bytes are
  release-specific, so a fixture captured against several releases gets sibling
  release directories.

## How it is populated

Nothing here is committed; regenerate on demand by driving the provisioned
oracle:

```sh
xfix materialize <fixture>   # re-emit one fixture's bytes, asserting they match
                             # the recorded hashes in its <RELEASE>.toml manifest
xfix capture <fixture>       # (re)record a fixture: writes the manifest AND this cache
```

`xfix verify-all` needs nothing here — it gates purely on the recorded hashes.
You only need to materialize when you want a byte-level diff for debugging. See
[`../specs/FIXTURES.md`](../specs/FIXTURES.md) and
[`../specs/PROVISIONING.md`](../specs/PROVISIONING.md).
