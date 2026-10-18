# ext-lint — `hnl lint` as a dynamic `hnl` extension

The canonical linter packaged as a `libhnl_ext_lint.{so,dylib,dll}` shared
library for the dumb `hnl` host (ADR-0149). The host discovers it via
`--ext-dir`, `$HANDL_EXT_DIR`, `~/.handl/ext/`, or a dev `target/debug/` next
to the `hnl` binary.

## Commands

```text
hnl lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>] [paths...]
```

`--fix` rewrites files in place; `--fail-on-warn` promotes warnings to the
failure exit code; `--prelude` seeds the unbound-wire scope from a name list;
`--language canonical` is accepted for parity (other values error). Reads
stdin when no paths are given. Exit codes: `0` ok, `1` diagnostics at/above
the severity gate, `2` usage/read/parse errors.

## Dev

```sh
cargo build        # target/debug/libhnl_ext_lint.so
cargo test         # unit tests link the rlib statically
HANDL_EXT_DIR=$PWD/target/debug ../host-cli/target/debug/hnl lint x.hnl
```

Sibling-repo deps resolve from sibling checkouts via the `[patch]` tables
(ADR-0128 §Resolution); without them the `branch = "main"` git sources are used.
