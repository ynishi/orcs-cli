use crate::allowance::{check_allow_with_reason, has_cfg_test, has_test_attr};
use crate::context::FileCtx;
use crate::types::{Location, Severity, Suggestion, Violation};
use syn::visit::Visit;
use syn::{ItemFn, ItemMod, Macro};

pub const NAME: &str = "no-panic";

const PANIC_MACROS: &[&str] = &["panic", "todo", "unimplemented", "unreachable"];

pub struct NoPanicInLib {
    pub allow_in_tests: bool,
    pub severity: Severity,
}

impl Default for NoPanicInLib {
    fn default() -> Self {
        Self {
            allow_in_tests: true,
            severity: Severity::Error,
        }
    }
}

impl NoPanicInLib {
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

    fn visit_macro(&mut self, node: &'ast Macro) {
        if self.allow_in_tests && self.in_test_context {
            syn::visit::visit_macro(self, node);
            return;
        }

        let macro_name = node.path.segments.last().map(|s| s.ident.to_string());

        let Some(name) = macro_name else {
            syn::visit::visit_macro(self, node);
            return;
        };

        if !PANIC_MACROS.contains(&name.as_str()) {
            syn::visit::visit_macro(self, node);
            return;
        }

        let span = node
            .path
            .segments
            .last()
            .map_or_else(proc_macro2::Span::call_site, |s| s.ident.span());
        let start = span.start();

        let allow = check_allow_with_reason(self.ctx.content, start.line, NAME);
        if allow.is_allowed() {
            syn::visit::visit_macro(self, node);
            return;
        }

        let location = Location::new(self.ctx.relative_path.clone(), start.line, start.column + 1);

        self.violations.push(
            Violation::new(
                NAME,
                self.severity,
                location,
                format!("`{name}!` is forbidden in library code"),
            )
            .with_suggestion(Suggestion::new(
                "Return a Result with a descriptive error instead",
            )),
        );

        syn::visit::visit_macro(self, node);
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
        NoPanicInLib::default().check(&ctx, &ast)
    }

    #[test]
    fn detects_panic() {
        let v = check_code("fn foo() { panic!(\"boom\"); }");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, NAME);
    }

    #[test]
    fn detects_todo() {
        let v = check_code("fn foo() { todo!(); }");
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn detects_unimplemented() {
        let v = check_code("fn foo() { unimplemented!(); }");
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn allows_in_test() {
        let v = check_code("#[test]\nfn test_foo() { panic!(\"ok\"); }");
        assert!(v.is_empty());
    }

    #[test]
    fn allows_in_cfg_test() {
        let v = check_code("#[cfg(test)]\nmod tests {\n    fn h() { panic!(\"ok\"); }\n}");
        assert!(v.is_empty());
    }

    #[test]
    fn allows_with_comment() {
        let v = check_code(
            "fn foo() {\n    // orcs-lint: allow(no-panic) reason=\"invariant\"\n    panic!(\"x\");\n}",
        );
        assert!(v.is_empty());
    }

    #[test]
    fn clean_code_no_violations() {
        let v = check_code("fn foo() -> Result<(), String> { Ok(()) }");
        assert!(v.is_empty());
    }
}
