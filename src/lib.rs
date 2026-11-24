//! `hnl-ext-lint` — the `lint` extension for `hnl` (ADR-0149).
//!
//! The linter logic lives in `handl-plugin-ext-lint` (capability adapter over
//! the data-driven `LintEngine` in `hnl-tool-lint`, ADR-0097); this crate
//! is the *packaging*: it exports the ADR-0149 ABI (`handl_ext_manifest` /
//! `handl_ext_dispatch` / `handl_ext_free`) and implements the
//! `hnl lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>]
//! [paths...]` command surface — byte-for-byte the semantics the built-in
//! `lint` arm had before the host went dumb.
//!
//! Internally the extension is a *host of plugins* like any other
//! (ADR-0149 §Consequences): a `DefaultHost` running the `lint` capability of
//! `handl_plugin_ext_lint::LintPlugin` (for stdin `--fix`; file `--fix`
//! goes straight to `fix_file_with_prelude`), with the canonical kit's
//! language + `.hnl`-rule resolver injected (ADR-0128 §"The 0.8 seam").

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use handl_ext::{ExtRequest, ExtResponse, ExtensionManifest};
use handl_kit_pkg::KitProjectExt;
use handl_kit_prelude::Kit;
use handl_language::Language;
use handl_platform::{CapabilityRequest, DefaultHost, PlatformError};
use handl_plugin_ext_detect::{Dialect, detect_dialect};
use handl_plugin_ext_diag::{Diagnostic, Severity, render_diagnostics};
use handl_plugin_ext_lint::LintContext;
use serde_json::json;

handl_ext::export!(
    ExtensionManifest::new("lint")
        .version(env!("CARGO_PKG_VERSION"))
        .command(
            "lint",
            "lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>] [paths...]",
        ),
    dispatch
);

/// ABI dispatch: only the `lint` command is declared.
fn dispatch(cmd: &str, req: &ExtRequest) -> ExtResponse {
    match cmd {
        "lint" => run(&req.args, Path::new(&req.cwd)),
        other => ExtResponse::error(2, format!("lint: unknown command `{other}`\n")),
    }
}

// ---------------------------------------------------------------------------
// `lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>] [paths...]`
// ---------------------------------------------------------------------------

/// Output accumulator: the extension renders one `ExtResponse` instead of
/// printing, so stdout/stderr collect here and the code is returned.
#[derive(Default)]
struct Out {
    stdout: String,
    stderr: String,
    had_failure: bool,
}

thread_local! {
    /// The canonical kit, memoized per thread for the tooling plugin.
    ///
    /// `Plugin: Send + Sync` forbids storing a `Kit` (`!Sync`) in a plugin —
    /// so per ADR-0128 §"The 0.8 seam" the host owns the kit cache here and
    /// injects *capture-less* closures that read it, keeping the plugin a
    /// pure adapter with no `handl-kit-canonical` in its signature.
    static TOOLING_KIT: RefCell<Option<Kit>> = const { RefCell::new(None) };
}

/// Run `f` with the thread-cached canonical kit, assembling on first use.
fn with_tooling_kit<R>(f: impl FnOnce(&Kit) -> R) -> R {
    TOOLING_KIT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            // Project-aware: discover `.handl.toml` from the CWD upward so
            // `import "dep/..."` resolves against `[dependencies]` (ADR-0128
            // Phase-5 name resolution).
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            *slot = Some(
                Kit::discover(&cwd)
                    .or_else(|_| Kit::new())
                    .expect("canonical kit must assemble"),
            );
        }
        f(slot.as_ref().expect("kit populated above"))
    })
}

fn load_prelude(path: &str) -> Result<HashSet<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read prelude file {path}: {e}"))?;
    Ok(text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn load_canonical_language() -> Result<Language, String> {
    Ok(handl_kit_data_prelude::assemble()
        .map_err(|e| format!("failed to assemble canonical language: {e}"))?
        .lang()
        .clone())
}

/// The process-wide unbound-wire prelude for `hnl lint`/`typecheck`: the names
/// the canonical kit's module chain supplies at eval time (ADR-0096).
fn typecheck_wire_prelude() -> &'static HashSet<String> {
    static PRELUDE: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    PRELUDE.get_or_init(handl_kit_prelude::prelude_wire_names)
}

/// Run the `lint` command, resolving relative paths against `cwd`.
fn run(args: &[String], cwd: &Path) -> ExtResponse {
    let mut fix = false;
    let mut fail_on_warn = false;
    let mut prelude_path: Option<String> = None;
    let mut language: Option<String> = None;
    let mut paths = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fix" => fix = true,
            "--fail-on-warn" => fail_on_warn = true,
            "--prelude" => {
                i += 1;
                if i >= args.len() {
                    return err("lint: --prelude requires a value\n");
                }
                prelude_path = Some(args[i].clone());
            }
            "--language" => {
                i += 1;
                if i >= args.len() {
                    return err("lint: --language requires a value\n");
                }
                language = Some(args[i].clone());
            }
            "--" => {}
            s if s.starts_with("--") => {
                return err(format!("lint: unknown flag `{s}`\n"));
            }
            s => paths.push(s.to_string()),
        }
        i += 1;
    }

    if let Some(ref lang) = language
        && lang != "canonical"
    {
        return err(
            "custom languages are not yet supported by the CLI; use the programmatic `_with_language` APIs\n",
        );
    }

    let use_with_language = prelude_path.is_some() || language.is_some();
    let prelude = match &prelude_path {
        Some(path) => match load_prelude(path) {
            Ok(set) => set,
            Err(msg) => return err(format!("lint: {msg}\n")),
        },
        None => typecheck_wire_prelude().clone(),
    };
    let lang = if use_with_language {
        match load_canonical_language() {
            Ok(l) => Some(l),
            Err(msg) => return err(format!("lint: {msg}\n")),
        }
    } else {
        None
    };

    // ADR-0124: serialize the language **once per command**, not per file.
    let lang_json: Option<serde_json::Value> = match &lang {
        Some(l) => match serde_json::to_value(l) {
            Ok(v) => Some(v),
            Err(e) => {
                return err(format!(
                    "lint: could not serialize the language for the capability: {e}\n"
                ));
            }
        },
        None => None,
    };

    let mut host = match tooling_host() {
        Ok(h) => h,
        Err(e) => return err(format!("lint: tooling host unavailable: {e}\n")),
    };

    // `--fix` runs through the codemod byte-edit path (PL-004 T-091):
    // `handl_plugin_ext_lint::fix_file_with_prelude` turns each
    // fixpoint round's `FixHit`s into `ByteEdit`s, plans them through
    // `CodemodEngine::plan_byte_edits`, and applies via
    // `Engine::apply` — journaled under `.handl/codegen/` and
    // revertable. Built once per command; the language override
    // (`--language`/`--prelude`) rides on `for_language` exactly like
    // the capability payload's `language` field (ADR-0124).
    let fix_ctx = if fix {
        let mut ctx = with_tooling_kit(|kit| {
            LintContext::with_resolver(kit.language().clone(), kit.handl_lint_adapter().resolver())
        });
        if let Some(l) = &lang {
            ctx = ctx.for_language(l.clone());
        }
        Some(ctx)
    } else {
        None
    };

    let mut out = Out::default();

    if paths.is_empty() {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            return err("lint: failed to read stdin\n");
        }
        // Stdin has no artifact — the fixed source goes to stdout
        // (there is nothing to journal).
        let fix_req = fix_ctx.as_ref().map(|ctx| FixRequest { ctx, target: None });
        let code = lint_one(
            &mut host,
            &src,
            "<stdin>",
            fix_req.as_ref(),
            fail_on_warn,
            &mut out,
            &prelude,
            lang_json.as_ref(),
        );
        if code == 2 {
            return ExtResponse {
                code: 2,
                stdout: out.stdout,
                stderr: out.stderr,
            };
        }
        return finish(out);
    }

    for path in &paths {
        let disk = resolve(cwd, path);
        let src = match std::fs::read_to_string(&disk) {
            Ok(s) => s,
            Err(e) => {
                out.stderr
                    .push_str(&format!("lint: failed to read {path}: {e}\n"));
                return ExtResponse {
                    code: 2,
                    stdout: out.stdout,
                    stderr: out.stderr,
                };
            }
        };
        // The codemod byte-edit target: `FsStore` root + artifact ref —
        // cwd when the file lives beneath it (journal at
        // `<cwd>/.handl/codegen/`), else the file's own directory.
        let fix_req = fix_ctx.as_ref().map(|ctx| {
            let (root, artifact) = store_location(cwd, &disk);
            FixRequest {
                ctx,
                target: Some((root, artifact)),
            }
        });
        let code = lint_one(
            &mut host,
            &src,
            path,
            fix_req.as_ref(),
            fail_on_warn,
            &mut out,
            &prelude,
            lang_json.as_ref(),
        );
        if code == 2 {
            return ExtResponse {
                code: 2,
                stdout: out.stdout,
                stderr: out.stderr,
            };
        }
    }

    finish(out)
}

/// A host with the `lint` plugin registered — the extension consumes the
/// same `DefaultHost`/`CapabilityRequest` machinery the built-in command
/// did; only the packaging changed (ADR-0149 §Consequences).
#[allow(clippy::result_large_err)]
fn tooling_host() -> Result<DefaultHost, PlatformError> {
    let mut host = DefaultHost::new();
    host.register_plugin(handl_plugin_ext_lint::LintPlugin::new(with_tooling_kit(
        |kit| {
            handl_plugin_ext_lint::LintContext::with_resolver(
                kit.language().clone(),
                kit.handl_lint_adapter().resolver(),
            )
        },
    )));
    host.load()?;
    host.resolve_composition()
        .map_err(|e| PlatformError::Plugin(format!("tooling composition: {e}")))?;
    Ok(host)
}

/// Resolve `path` (possibly relative) against the request's `cwd`.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// The `FsStore` root + artifact ref for `disk` under `cwd`: the cwd
/// when the file lives beneath it — so `fix_file`'s journal/manifest
/// land at `<cwd>/.handl/codegen/`, the bookkeeping root `hnl codemod`
/// uses — else the file's own directory (the artifact ref must be
/// store-relative).
fn store_location(cwd: &Path, disk: &Path) -> (PathBuf, String) {
    let canon_disk = disk.canonicalize().unwrap_or_else(|_| disk.to_path_buf());
    let canon_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if let Ok(rel) = canon_disk.strip_prefix(&canon_cwd)
        && !rel.as_os_str().is_empty()
    {
        return (cwd.to_path_buf(), rel.to_string_lossy().replace('\\', "/"));
    }
    let name = disk
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    (
        disk.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf()),
        name,
    )
}

/// The `--fix` request for one file: the lint context plus the codemod
/// byte-edit target — `Some((store_root, artifact))` for on-disk paths
/// (journaled `Engine::apply` under `<root>/.handl/codegen/`), `None`
/// for stdin (fixed source goes to stdout; there is no artifact).
struct FixRequest<'a> {
    ctx: &'a LintContext,
    target: Option<(PathBuf, String)>,
}

/// Lint a single source. Returns 2 on parse error; sets `out.had_failure`
/// for Error diagnostics (or Warning when `fail_on_warn`).
#[allow(clippy::too_many_arguments)]
fn lint_one(
    host: &mut DefaultHost,
    src: &str,
    path: &str,
    fix: Option<&FixRequest<'_>>,
    fail_on_warn: bool,
    out: &mut Out,
    prelude: &HashSet<String>,
    lang_json: Option<&serde_json::Value>,
) -> i32 {
    if detect_dialect(src) != Dialect::Canonical {
        out.stderr
            .push_str(&format!("skipping dialect file: {path}\n"));
        return 0;
    }

    // `unbound-wire` is delegated to the bootstrap typechecker's scope pass;
    // seed it with the wire prelude supplied by the host (canonical or user)
    // plus the names this file's splice-form imports (`import "<spec>"`,
    // `from "<spec>" import *`) bind into scope at eval time — same seeding
    // as `hnl run`/`hnl typecheck`.
    let bases = spec_resolution_bases(Path::new(path), None);
    let deps = handl_platform::discover_dependencies(Path::new(path)).unwrap_or_default();
    // P-013 T-150: dep scopes narrow the splice-import seeding to the
    // declared surface, matching `hnl run`/`hnl typecheck`.
    let dep_scopes = handl_platform::discover_dep_scopes(Path::new(path));
    let mut seeded = prelude.clone();
    seeded.extend(spliced_import_names(src, &bases, &deps, &dep_scopes));
    let prelude_names: Vec<String> = seeded.iter().cloned().collect();

    // Same capability path a third-party consumer uses (ADR-0085 step 4). The
    // caller's `language` rides in the payload (ADR-0124) so a capability lint
    // matches a direct call.
    let mut payload = json!({ "source": src, "prelude": prelude_names });
    if let Some(l) = lang_json {
        payload["language"] = l.clone();
    }

    let request = CapabilityRequest {
        capability: "lint".into(),
        payload,
    };
    let response = match host.run(&request) {
        Ok(r) => r,
        Err(e) => {
            out.stderr.push_str(&format!("lint: {path}: {e}\n"));
            return 2;
        }
    };
    let diags: Vec<Diagnostic> = match response.0.get("status").and_then(|v| v.as_str()) {
        Some("diagnostics") => response
            .0
            .get("diagnostics")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
        Some("error") => {
            let msg = response
                .0
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("lint failed");
            out.stderr.push_str(&format!("{path}: {msg}\n"));
            return 2;
        }
        other => {
            out.stderr.push_str(&format!(
                "lint: {path}: unexpected capability status {other:?}\n"
            ));
            return 2;
        }
    };

    let rendered = render_diagnostics(&diags, path);
    if !rendered.is_empty() {
        out.stdout.push_str(&format!("{rendered}\n"));
    }

    let has_error = diags.iter().any(|d| d.severity == Severity::Error);
    let has_warn = diags.iter().any(|d| d.severity == Severity::Warning);
    if has_error || (fail_on_warn && has_warn) {
        out.had_failure = true;
    }

    if let Some(fx) = fix {
        match &fx.target {
            // File path: the codemod byte-edit path — each fixpoint
            // round's `FixHit`s become `ByteEdit`s planned through
            // `CodemodEngine::plan_byte_edits` and applied via
            // `Engine::apply`, journaled under
            // `<root>/.handl/codegen/` and revertable (PL-004 T-091).
            // Replaces the old capability `fix` + `write_atomic`.
            Some((root, artifact)) => {
                if let Err(e) =
                    handl_plugin_ext_lint::fix_file_with_prelude(fx.ctx, root, artifact, &seeded)
                {
                    out.stderr.push_str(&format!("{path}: {e}\n"));
                    return 2;
                }
            }
            // Stdin: no artifact exists to journal — emit the fixed
            // source on stdout through the same capability the file
            // path used before (in-memory fixpoint only).
            None => {
                let fix_request = CapabilityRequest {
                    capability: "lint".into(),
                    payload: json!({ "source": src, "fix": true, "prelude": prelude_names.clone() }),
                };
                match host.run(&fix_request) {
                    Ok(r) if r.0.get("status").and_then(|v| v.as_str()) == Some("fixed") => {
                        let fixed =
                            r.0.get("output")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                        out.stdout.push_str(fixed);
                    }
                    Ok(r) => {
                        let msg =
                            r.0.get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("fix failed");
                        out.stderr.push_str(&format!("{path}: fix error: {msg}\n"));
                        return 2;
                    }
                    Err(e) => {
                        out.stderr.push_str(&format!("lint: {path}: {e}\n"));
                        return 2;
                    }
                }
            }
        }
    }

    0
}

// ---------------------------------------------------------------------------
// Import-specifier resolution (shared with `run`/`typecheck`)
// ---------------------------------------------------------------------------

/// Resolution bases for `import "<spec>"` specifiers in a file at `path`,
/// mirroring `ModuleLoader` order: the file's directory, the module root,
/// then the process working directory. Used by `run`, `typecheck`, and
/// `lint` alike so each command resolves imports the way eval will.
fn spec_resolution_bases(path: &Path, module_root: Option<&Path>) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => bases.push(dir.to_path_buf()),
        _ => bases.push(PathBuf::from(".")),
    }
    if let Some(root) = module_root
        && !bases.iter().any(|b| b.as_path() == root)
    {
        bases.push(root.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir()
        && !bases.contains(&cwd)
    {
        bases.push(cwd);
    }
    bases
}

/// Resolve a `[dependencies]` specifier: when the first `/`-separated
/// segment of `spec` names a declared dependency, the remainder resolves
/// inside that package's directory (`dep/lib/file` →
/// `<deps["dep"]>/lib/file`; a bare `dep` name resolves the package dir's
/// `index.hnl`/`<name>.hnl` main file). Returns `None` for non-dep specs.
fn resolve_dep_spec_path(
    spec: &str,
    deps: &std::collections::HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    let (first, rest) = match spec.split_once('/') {
        Some((f, r)) => (f, Some(r)),
        None => (spec, None),
    };
    let dep_dir = deps.get(first)?;
    // pkg.hnl `modules:`/`src/` layout first (dep names are package
    // abstractions), literal `<dep_dir>/<rest>` as fallback — same as
    // `ModuleLoader::resolve_path` via `handl_platform::dep_spec_file`.
    handl_platform::dep_spec_file(dep_dir, rest.unwrap_or(""))
}

/// Resolve `<spec>` to the file `ModuleLoader` would load: dependency-name
/// resolution first, then `bases` (same order as [`spec_resolves_to_file`]).
/// `pkg.hnl` package layouts apply exactly as in `ModuleLoader` —
/// [`handl_platform::relative_spec_file`] tries the literal candidate
/// first, then package prefixes of the spec.
fn resolve_spec_path(
    spec: &str,
    bases: &[PathBuf],
    deps: &std::collections::HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    if Path::new(spec).is_absolute() {
        return handl_platform::relative_spec_file(Path::new("/"), spec.trim_start_matches('/'));
    }
    resolve_dep_spec_path(spec, deps).or_else(|| {
        bases
            .iter()
            .find_map(|base| handl_platform::relative_spec_file(base, spec))
    })
}

/// The specifier of a *splice-form* import — `import "<spec>"` or
/// `from "<spec>" import *` with no alias — whose exports land directly in
/// the importer's scope at eval time. Named/aliased `from` forms bind
/// statically known names and need no spliced scope.
fn splice_import_spec(trimmed: &str) -> Option<String> {
    if let Some(rest) = trimmed.strip_prefix("import") {
        if !rest.starts_with(' ') && !rest.starts_with('\t') {
            return None;
        }
        let inner = rest.trim_start();
        if !inner.starts_with('"') && !inner.starts_with('\'') {
            return None;
        }
        return first_quoted_spec(inner).map(str::to_string);
    }
    let rest = trimmed.strip_prefix("from")?;
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    // `from "<spec>" <tail>` — splice iff the tail is `import *` exactly.
    let bytes = rest.as_bytes();
    let q = bytes.iter().position(|b| *b == b'"' || *b == b'\'')?;
    let quote = bytes[q] as char;
    let after_q = &rest[q + 1..];
    let end = after_q.find(quote)?;
    let spec = &after_q[..end];
    let tail = after_q[end + quote.len_utf8()..].trim_start();
    let tail = tail.strip_prefix("import")?.trim_start();
    let tail = tail
        .split("--")
        .next()
        .unwrap_or("")
        .trim_end()
        .trim_end_matches(';')
        .trim_end();
    (tail == "*").then(|| spec.to_string())
}

/// Names a file's splice-form imports bind into scope at eval time: for
/// each resolvable `import "<spec>"` / `from "<spec>" import *`, the
/// module's declared exports (collected without evaluating it). The
/// bootstrap typechecker's module scope is source-only, so `hnl run`,
/// `hnl typecheck`, and `hnl lint` seed these into their scope-check
/// prelude — otherwise every use of a spliced export is reported as
/// `unbound wire` even though eval binds it.
fn spliced_import_names(
    src: &str,
    bases: &[PathBuf],
    deps: &std::collections::HashMap<String, PathBuf>,
    scopes: &std::collections::HashMap<String, handl_platform::PkgDepScope>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        let Some(spec) = splice_import_spec(trimmed) else {
            continue;
        };
        let Some(path) = resolve_spec_path(&spec, bases, deps) else {
            continue;
        };
        let Ok(mod_src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut exported = handl_runtime::module_prep::module_export_names(&mod_src);
        // P-013 T-150: dep-scoped specifiers seed only declared atoms.
        if let Some((scope, _, module)) = handl_platform::dep_spec_scope(scopes, &spec) {
            if !scope.allows_module(&module) {
                continue;
            }
            exported.retain(|n| scope.allows_field(&module, n));
        }
        names.extend(exported);
    }
    names
}

/// Extract the first `'`- or `"`-quoted string literal's contents from a
/// source fragment (e.g. the specifier in `from "..." import ...`).
fn first_quoted_spec(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'"' || b == b'\'')?;
    let quote = bytes[start];
    let rest = &s[start + 1..];
    let end = rest.as_bytes().iter().position(|&b| b == quote)?;
    Some(&rest[..end])
}

// ---------------------------------------------------------------------------
// Response plumbing
// ---------------------------------------------------------------------------

fn err(msg: impl Into<String>) -> ExtResponse {
    ExtResponse::error(2, msg.into())
}

fn finish(out: Out) -> ExtResponse {
    ExtResponse {
        code: if out.had_failure { 1 } else { 0 },
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// A unique temp dir per test.
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hnl-ext-lint-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unknown_flag_is_usage_error() {
        let res = run(&["--bogus".into()], Path::new("."));
        assert_eq!(res.code, 2);
        assert!(res.stderr.contains("lint: unknown flag `--bogus`"));
    }

    #[test]
    fn non_canonical_language_is_usage_error() {
        let res = run(&["--language".into(), "other".into()], Path::new("."));
        assert_eq!(res.code, 2);
        assert!(
            res.stderr
                .contains("custom languages are not yet supported")
        );
    }

    #[test]
    fn prelude_flag_requires_a_value() {
        let res = run(&["--prelude".into()], Path::new("."));
        assert_eq!(res.code, 2);
        assert!(res.stderr.contains("lint: --prelude requires a value"));
    }

    #[test]
    fn missing_file_is_read_error() {
        let dir = tmpdir("missing");
        let res = run(&["nope.hnl".into()], &dir);
        assert_eq!(res.code, 2);
        assert!(res.stderr.contains("lint: failed to read"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--fix` on a file goes through `fix_file_with_prelude` — the
    /// codemod byte-edit path journaled under `.handl/codegen/`
    /// (PL-004), not the old capability + `write_atomic`.
    #[test]
    fn fix_rewrites_the_file_journaled() {
        let dir = tmpdir("fix");
        let file = dir.join("dirty.hnl");
        std::fs::write(&file, "x := (1)\n").unwrap();
        let res = run(&["--fix".into(), "dirty.hnl".into()], &dir);
        assert_eq!(res.code, 0, "stderr: {}", res.stderr);
        let fixed = std::fs::read_to_string(&file).unwrap();
        assert_ne!(fixed, "x := (1)\n", "expected a fix round to apply");
        assert!(
            dir.join(".handl/codegen/journal").exists()
                || dir
                    .join(".handl/codegen")
                    .read_dir()
                    .is_ok_and(|mut d| d.next().is_some()),
            "expected .handl/codegen bookkeeping under {}",
            dir.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The `export!`-generated manifest symbol, called directly.
    #[test]
    fn manifest_declares_lint() {
        let raw = handl_ext_manifest();
        assert!(!raw.is_null());
        let json = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { handl_ext_free(raw) };
        let manifest: ExtensionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest.name, "lint");
        assert_eq!(manifest.commands[0].name, "lint");
    }
}
