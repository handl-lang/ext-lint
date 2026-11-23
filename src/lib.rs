//! `hnl-ext-lint` — the `lint` extension for `hnl` (ADR-0149).
//!
//! The linter logic lives in `handl-plugin-ext-lint` (capability adapter over
//! the data-driven `LintEngine` in `hnl-engine-lint`, ADR-0097); this crate
//! is the *packaging*: it exports the ADR-0149 ABI (`handl_ext_manifest` /
//! `handl_ext_dispatch` / `handl_ext_free`) and implements the
//! `hnl lint [--fix] [--fail-on-warn] [--prelude <path>] [--language <name>]
//! [paths...]` command surface — byte-for-byte the semantics the built-in
//! `lint` arm had before the host went dumb.
//!
//! Internally the extension is a *host of plugins* like any other
//! (ADR-0149 §Consequences): a `DefaultHost` running the `lint` capability of
//! [`LintPlugin`], with the canonical kit's language + `.hnl`-rule resolver
//! injected (ADR-0128 §"The 0.8 seam").

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

    let mut out = Out::default();

    if paths.is_empty() {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            return err("lint: failed to read stdin\n");
        }
        let code = lint_one(
            &mut host,
            &src,
            "<stdin>",
            fix,
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
        let code = lint_one(
            &mut host,
            &src,
            path,
            fix,
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

/// Write `content` to `target` atomically (temp file + rename).
fn write_atomic(target: &Path, content: &str) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".hnl-fmt-tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// Lint a single source. Returns 2 on parse error; sets `out.had_failure`
/// for Error diagnostics (or Warning when `fail_on_warn`).
#[allow(clippy::too_many_arguments)]
fn lint_one(
    host: &mut DefaultHost,
    src: &str,
    path: &str,
    fix: bool,
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
    let mut seeded = prelude.clone();
    seeded.extend(spliced_import_names(src, &bases, &deps));
    let prelude: Vec<String> = seeded.into_iter().collect();

    // Same capability path a third-party consumer uses (ADR-0085 step 4). The
    // caller's `language` rides in the payload (ADR-0124) so a capability lint
    // matches a direct call.
    let mut payload = json!({ "source": src, "prelude": prelude });
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

    if fix {
        // `fix` runs through the plugin's injected lint context — see
        // LintPlugin.
        let fix_request = CapabilityRequest {
            capability: "lint".into(),
            payload: json!({ "source": src, "fix": true, "prelude": prelude }),
        };
        match host.run(&fix_request) {
            Ok(r) if r.0.get("status").and_then(|v| v.as_str()) == Some("fixed") => {
                let fixed =
                    r.0.get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                if let Err(e) = write_atomic(Path::new(path), fixed) {
                    out.stderr
                        .push_str(&format!("lint: failed to write {path}: {e}\n"));
                    return 2;
                }
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
    let joined = match rest {
        Some(r) if !r.is_empty() => dep_dir.join(r),
        _ => dep_dir.clone(),
    };
    file_candidate(&joined, spec)
}

/// Resolve `<spec>` to the file `ModuleLoader` would load: dependency-name
/// resolution first, then `bases` (same order as [`spec_resolves_to_file`]).
fn resolve_spec_path(
    spec: &str,
    bases: &[PathBuf],
    deps: &std::collections::HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    if Path::new(spec).is_absolute() {
        return file_candidate(Path::new(spec), spec);
    }
    resolve_dep_spec_path(spec, deps).or_else(|| {
        bases
            .iter()
            .find_map(|base| file_candidate(&base.join(spec), spec))
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
        names.extend(handl_runtime::module_prep::module_export_names(&mod_src));
    }
    names
}

fn file_candidate(joined: &Path, spec: &str) -> Option<PathBuf> {
    if joined.is_dir() {
        let index = joined.join("index.hnl");
        if index.is_file() {
            return Some(index);
        }
        if let Some(name) = joined.file_name().and_then(|s| s.to_str()) {
            let main = joined.join(format!("{}.hnl", name));
            if main.is_file() {
                return Some(main);
            }
        }
    }
    if !spec.ends_with(".hnl") {
        let with_ext = joined.with_extension("hnl");
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }
    if joined.is_file() {
        Some(joined.to_path_buf())
    } else {
        None
    }
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
