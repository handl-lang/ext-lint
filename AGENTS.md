# ext-lint

`hnl-ext-lint` — `hnl lint` as a dynamic `hnl` extension: the canonical
linter packaged as a `libhnl_ext_lint.{so,dylib,dll}` cdylib the dumb
`hnl` host discovers at runtime.

## Layout

| path | role |
| --- | --- |
| `src/` | extension ABI exports + `lint` command wiring |
| `xtask/` | vendored gate crate (`fmt`, `clippy`, `test`, `doc`, `gate`) |
| `workflows/gate.yaml` | per-push gate — `handl-gates` rust campaign via bupha |

## Contract

- Commands: `hnl lint [--fix] [--fail-on-warn] [--prelude <path>]
  [--language <name>] [paths...]` — stdin when no paths given.
- `--fix` rewrites files in place; `--fail-on-warn` promotes warnings to
  the failure exit code; `--prelude` seeds the unbound-wire scope;
  `--language canonical` is accepted for parity (other values error).
- Exit codes: `0` ok, `1` diagnostics at/above the severity gate, `2`
  usage/read/parse errors.
- Builds `cdylib` + `rlib`; `cargo test` links the `rlib` statically.

## Verification

```bash
cargo run --manifest-path xtask/Cargo.toml -- gate   # fmt → clippy → test → doc
```

<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->
