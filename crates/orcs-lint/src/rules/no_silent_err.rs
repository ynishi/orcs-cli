use crate::allowance::{check_allow_with_reason, has_cfg_test, has_test_attr};
use crate::context::FileCtx;
use crate::types::{Location, Severity, Suggestion, Violation};
use syn::visit::Visit;
use syn::{Expr, ExprMatch, ItemFn, ItemMod, Pat, PatIdent, PatTupleStruct, PatWild};

pub const NAME: &str = "no-silent-err";

/// OL004: Detect `Err(_) => None` patterns that silently discard errors.
///
/// # Rationale
///
/// Matching `Err(_) => None` (or `Err(_e) => None`) converts a `Result` error
/// into `Option::None`, discarding the error information.  This violates the
/// Rust error propagation convention: callers should receive the error via `?`
/// or explicit handling, not have it silently swallowed.
///
/// ## What triggers this rule
///
/// ```rust,ignore
/// match result {
///     Ok(v) => Some(v),
///     Err(_) => None,       // OL004
/// }
///
/// match result {
///     Ok(v) => Some(v),
///     Err(_e) => None,      // OL004 (named but unused)
/// }
/// ```
///
/// ## How to fix
///
/// - Propagate the error: `result?` or `result.map(Some)?`
/// - Handle explicitly: `Err(e) => { tracing::warn!(...); None }`
/// - If intentional, add an allow comment:
///   `// orcs-lint: allow(no-silent-err) reason="key absence is expected"`
pub struct NoSilentErr {
    pub allow_in_tests: bool,
    pub severity: Severity,
}

impl Default for NoSilentErr {
    fn default() -> Self {
        Self {
            allow_in_tests: true,
            severity: Severity::Warning,
        }
    }
}

impl NoSilentErr {
    pub fn check<'a>(&self, ctx: &'a FileCtx<'a>, ast: &syn::File) -> Vec<Violation> {
        if self.allow_in_tests && ctx.is_test {
            return Vec::new();
        }

        let mut visitor = Visitor {
            ctx,
            allow_in_tests: self.allow_in_tests,
            severity: self.severity,
            violations: Vec::new(),
            in_test_context: false,
        };
        visitor.visit_file(ast);
        visitor.violations
    }
}

struct Visitor<'a> {
    ctx: &'a FileCtx<'a>,
    allow_in_tests: bool,
    severity: Severity,
    violations: Vec<Violation>,
    in_test_context: bool,
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let prev = self.in_test_context;
        if has_cfg_test(&node.attrs) {
            self.in_test_context = true;
        }
        syn::visit::visit_item_mod(self, node);
        self.in_test_context = prev;
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let prev = self.in_test_context;
        if has_test_attr(&node.attrs) {
            self.in_test_context = true;
        }
        syn::visit::visit_item_fn(self, node);
        self.in_test_context = prev;
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        if self.allow_in_tests && self.in_test_context {
            syn::visit::visit_expr_match(self, node);
            return;
        }

        for arm in &node.arms {
            if is_err_wildcard_pattern(&arm.pat) && is_none_body(&arm.body) {
                let span = arm_span(&arm.pat);
                let start = span.start();

                let allow = check_allow_with_reason(self.ctx.content, start.line, NAME);
                if allow.is_allowed() {
                    continue;
                }

                let location =
                    Location::new(self.ctx.relative_path.clone(), start.line, start.column + 1);

                self.violations.push(
                    Violation::new(
                        NAME,
                        self.severity,
                        location,
                        "`Err(_) => None` silently discards the error".to_string(),
                    )
                    .with_suggestion(Suggestion::new(
                        "Propagate via `?`, log the error, or add: // orcs-lint: allow(no-silent-err) reason=\"...\"",
                    )),
                );
            }
        }

        // Continue visiting nested expressions
        syn::visit::visit_expr_match(self, node);
    }
}

/// Check if a pattern matches `Err(_)` or `Err(_name)`.
fn is_err_wildcard_pattern(pat: &Pat) -> bool {
    let Pat::TupleStruct(PatTupleStruct { path, elems, .. }) = pat else {
        return false;
    };

    if !path.is_ident("Err") {
        return false;
    }

    if elems.len() != 1 {
        return false;
    }

    match &elems[0] {
        // Err(_)
        Pat::Wild(PatWild { .. }) => true,
        // Err(_name) — prefixed with underscore, likely unused
        Pat::Ident(PatIdent { ident, .. }) => ident.to_string().starts_with('_'),
        _ => false,
    }
}

/// Check if an expression is `None`.
fn is_none_body(expr: &Expr) -> bool {
    matches!(expr, Expr::Path(ep) if ep.path.is_ident("None"))
}

/// Get the span of the first token in a pattern for error reporting.
fn arm_span(pat: &Pat) -> proc_macro2::Span {
    match pat {
        Pat::TupleStruct(p) => p
            .path
            .segments
            .first()
            .map_or_else(proc_macro2::Span::call_site, |s| s.ident.span()),
        _ => proc_macro2::Span::call_site(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn check_code(code: &str) -> Vec<Violation> {
        let ast = syn::parse_file(code).expect("parse failed");
        let ctx = FileCtx {
            path: Path::new("test.rs"),
            content: code,
            is_test: false,
            layer: None,
            module_path: vec![],
            relative_path: PathBuf::from("test.rs"),
        };
        NoSilentErr::default().check(&ctx, &ast)
    }

    #[test]
    fn detects_err_wildcard_to_none() {
        let v = check_code(
            r#"
fn foo() -> Option<i32> {
    let r: Result<i32, String> = Ok(1);
    match r {
        Ok(v) => Some(v),
        Err(_) => None,
    }
}
"#,
        );
        assert_eq!(v.len(), 1, "should detect Err(_) => None");
        assert_eq!(v[0].rule, NAME);
        assert!(v[0].message.contains("silently discards"));
    }

    #[test]
    fn detects_err_named_underscore_to_none() {
        let v = check_code(
            r#"
fn foo() -> Option<i32> {
    let r: Result<i32, String> = Ok(1);
    match r {
        Ok(v) => Some(v),
        Err(_e) => None,
    }
}
"#,
        );
        assert_eq!(v.len(), 1, "should detect Err(_e) => None");
    }

    #[test]
    fn allows_err_with_logging() {
        let v = check_code(
            r#"
fn foo() -> Option<i32> {
    let r: Result<i32, String> = Ok(1);
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("error: {}", e);
            None
        }
    }
}
"#,
        );
        assert!(
            v.is_empty(),
            "named Err(e) with block body should not trigger"
        );
    }

    #[test]
    fn allows_err_propagation() {
        let v = check_code(
            r#"
fn foo() -> Result<i32, String> {
    let r: Result<i32, String> = Ok(1);
    let val = r?;
    Ok(val)
}
"#,
        );
        assert!(v.is_empty(), "? propagation should not trigger");
    }

    #[test]
    fn allows_in_test_context() {
        let v = check_code(
            r#"
#[cfg(test)]
mod tests {
    fn helper() -> Option<i32> {
        let r: Result<i32, String> = Ok(1);
        match r {
            Ok(v) => Some(v),
            Err(_) => None,
        }
    }
}
"#,
        );
        assert!(v.is_empty(), "should allow in test module");
    }

    #[test]
    fn allows_with_orcs_lint_comment() {
        let v = check_code(
            r#"
fn foo() -> Option<i32> {
    let r: Result<i32, String> = Ok(1);
    match r {
        Ok(v) => Some(v),
        // orcs-lint: allow(no-silent-err) reason="key absence is expected"
        Err(_) => None,
    }
}
"#,
        );
        assert!(v.is_empty(), "should allow with orcs-lint comment");
    }

    #[test]
    fn clean_code_no_violations() {
        let v = check_code("fn foo() -> Result<(), String> { Ok(()) }");
        assert!(v.is_empty());
    }

    #[test]
    fn does_not_trigger_on_ok_wildcard() {
        let v = check_code(
            r#"
fn foo() -> Option<String> {
    let r: Result<i32, String> = Err("x".into());
    match r {
        Ok(_) => None,
        Err(e) => Some(e),
    }
}
"#,
        );
        assert!(v.is_empty(), "Ok(_) => None should not trigger");
    }
}
