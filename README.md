# ext-lint

`hnl-ext-lint` — `hnl lint` as a dynamic `hnl` extension.

An `ext-*` crate: the canonical linter packaged as a
`libhnl_ext_lint.{so,dylib,dll}` shared library for the dumb `hnl` host. The
host discovers it via `--ext-dir`, `$HANDL_EXT_DIR`, `~/.handl/ext/`, or a dev
`target/debug/` next to the `hnl` binary.

## Commands

```text
hnl lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>] [paths...]
```

`--fix` rewrites files in place; `--fail-on-warn` promotes warnings to the
failure exit code; `--prelude` seeds the unbound-wire scope from a name list;
`--language canonical` is accepted for parity (other values error). Reads
stdin when no paths are given. Exit codes: `0` ok, `1` diagnostics at/above
the severity gate, `2` usage/read/parse errors.

## Development

```bash
cargo xtask gate        # fmt --check → clippy -D warnings → test → doc
```

`cargo xtask` wraps the repo's `xtask/` crate (`fmt`, `clippy`, `test`, `doc`,
`check`, `gate`); `ci.sh` is the gate entry. The crate builds a `cdylib` +
`rlib`; `cargo test` links the `rlib` statically.

```sh
cargo build        # target/debug/libhnl_ext_lint.so
HANDL_EXT_DIR=$PWD/target/debug ../host-cli/target/debug/hnl lint x.hnl
```

Sibling-repo deps resolve from sibling checkouts via the `[patch]` tables;
without them the `branch = "main"` git sources are used.

## License

MIT.
