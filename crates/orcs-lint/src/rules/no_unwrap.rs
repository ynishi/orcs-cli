use crate::allowance::{check_allow_with_reason, has_allow_attr, has_cfg_test, has_test_attr};
use crate::context::FileCtx;
use crate::types::{Location, Severity, Suggestion, Violation};
use syn::visit::Visit;
use syn::{ExprMethodCall, ItemFn, ItemImpl, ItemMod};

pub const NAME: &str = "no-unwrap";

pub struct NoUnwrap {
    pub severity: Severity,
}

impl Default for NoUnwrap {
    fn default() -> Self {
        Self {
            severity: Severity::Error,
        }
    }
}

impl NoUnwrap {
    pub fn check<'a>(&self, ctx: &'a FileCtx<'a>, ast: &syn::File) -> Vec<Violation> {
        let mut visitor = Visitor {
            ctx,
            severity: self.severity,
            violations: Vec::new(),
            in_test_context: ctx.is_test,
            in_allowed_context: false,
            in_main_fn: false,
        };
        visitor.visit_file(ast);
        visitor.violations
    }
}

struct Visitor<'a> {
    ctx: &'a FileCtx<'a>,
    severity: Severity,
    violations: Vec<Violation>,
    in_test_context: bool,
    in_allowed_context: bool,
    in_main_fn: bool,
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        let prev_test = self.in_test_context;
        let prev_allow = self.in_allowed_context;

        if has_cfg_test(&node.attrs) {
            self.in_test_context = true;
        }
        syn::visit::visit_item_mod(self, node);

        self.in_test_context = prev_test;
        self.in_allowed_context = prev_allow;
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let prev_test = self.in_test_context;
        let prev_allow = self.in_allowed_context;
        let prev_main = self.in_main_fn;

        if has_test_attr(&node.attrs) {
            self.in_test_context = true;
        }
        if has_allow_attr(&node.attrs, &["clippy::unwrap_used", "clippy::expect_used"]) {
            self.in_allowed_context = true;
        }
        // fn main() in build scripts / binaries: allow .expect()
        if node.sig.ident == "main" {
            self.in_main_fn = true;
        }

        syn::visit::visit_item_fn(self, node);

        self.in_test_context = prev_test;
        self.in_allowed_context = prev_allow;
        self.in_main_fn = prev_main;
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let prev_allow = self.in_allowed_context;
        syn::visit::visit_item_impl(self, node);
        self.in_allowed_context = prev_allow;
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if self.in_allowed_context {
            syn::visit::visit_expr_method_call(self, node);
            return;
        }

        let method_name = node.method.to_string();
        let is_unwrap = method_name == "unwrap";
        let is_expect = method_name == "expect";

        if !is_unwrap && !is_expect {
            syn::visit::visit_expr_method_call(self, node);
            return;
        }

        // テストコンテキストでは expect() は許可、unwrap() のみ禁止
        if self.in_test_context && is_expect {
            syn::visit::visit_expr_method_call(self, node);
            return;
        }

        // fn main() (build.rs / binary entry) では expect() は許可
        if self.in_main_fn && is_expect {
            syn::visit::visit_expr_method_call(self, node);
            return;
        }

        let span = node.method.span();
        let start = span.start();

        let allow = check_allow_with_reason(self.ctx.content, start.line, NAME);
        if allow.is_allowed() {
            syn::visit::visit_expr_method_call(self, node);
            return;
        }

        let location = Location::new(self.ctx.relative_path.clone(), start.line, start.column + 1);

        let (message, suggestion) = if is_unwrap && self.in_test_context {
            (
                ".unwrap() is forbidden in test code. Use .expect(\"description\") instead",
                "Replace .unwrap() with .expect(\"reason for expectation\")",
            )
        } else if is_unwrap {
            (
                ".unwrap() is forbidden in production code",
                "Use `?` operator, `.ok_or(Error)?`, or pattern matching",
            )
        } else {
            (
                ".expect() is forbidden in production code",
                "Use `?` operator with `.context()` or custom error",
            )
        };

        self.violations.push(
            Violation::new(NAME, self.severity, location, message)
                .with_suggestion(Suggestion::new(suggestion)),
        );

        syn::visit::visit_expr_method_call(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn check_code(code: &str) -> Vec<Violation> {
        let ast = syn::parse_file(code).expect("parse failed");
        let ctx = FileCtx {
            path: Path::new("src/lib.rs"),
            content: code,
            is_test: false,
            layer: None,
            module_path: vec![],
            relative_path: PathBuf::from("src/lib.rs"),
        };
        NoUnwrap::default().check(&ctx, &ast)
    }

    fn check_test_code(code: &str) -> Vec<Violation> {
        let ast = syn::parse_file(code).expect("parse failed");
        let ctx = FileCtx {
            path: Path::new("tests/integration.rs"),
            content: code,
            is_test: true,
            layer: None,
            module_path: vec![],
            relative_path: PathBuf::from("tests/integration.rs"),
        };
        NoUnwrap::default().check(&ctx, &ast)
    }

    // --- production code ---

    #[test]
    fn detects_unwrap_in_prod() {
        let v = check_code("fn foo() { let x = Some(1).unwrap(); }");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, NAME);
        assert!(v[0].message.contains("production"));
    }

    #[test]
    fn detects_expect_in_prod() {
        let v = check_code("fn foo() { let x = Some(1).expect(\"msg\"); }");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("production"));
    }

    #[test]
    fn allows_with_clippy_attr() {
        let v = check_code("#[allow(clippy::unwrap_used)]\nfn foo() { Some(1).unwrap(); }");
        assert!(v.is_empty());
    }

    #[test]
    fn allows_with_comment() {
        let v = check_code(
            "fn foo() {\n    // orcs-lint: allow(no-unwrap) reason=\"safe\"\n    Some(1).unwrap();\n}",
        );
        assert!(v.is_empty());
    }

    #[test]
    fn clean_code_no_violations() {
        let v = check_code("fn foo() -> Option<i32> { Some(1) }");
        assert!(v.is_empty());
    }

    // --- test code: unwrap() 禁止, expect() 許可 ---

    #[test]
    fn test_code_unwrap_forbidden() {
        let v = check_test_code("fn test_foo() { Some(1).unwrap(); }");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("test code"));
        assert!(v[0].message.contains("expect"));
    }

    #[test]
    fn test_code_expect_allowed() {
        let v = check_test_code("fn test_foo() { Some(1).expect(\"should be Some\"); }");
        assert!(v.is_empty());
    }

    #[test]
    fn cfg_test_mod_unwrap_forbidden() {
        let v = check_code("#[cfg(test)]\nmod tests {\n    fn helper() { Some(1).unwrap(); }\n}");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("test code"));
    }

    #[test]
    fn cfg_test_mod_expect_allowed() {
        let v =
            check_code("#[cfg(test)]\nmod tests {\n    fn helper() { Some(1).expect(\"ok\"); }\n}");
        assert!(v.is_empty());
    }

    #[test]
    fn test_fn_unwrap_forbidden() {
        let v = check_code("#[test]\nfn test_foo() { Some(1).unwrap(); }");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("test code"));
    }

    #[test]
    fn test_fn_expect_allowed() {
        let v = check_code("#[test]\nfn test_foo() { Some(1).expect(\"ok\"); }");
        assert!(v.is_empty());
    }

    #[test]
    fn tokio_test_expect_allowed() {
        let v = check_code("#[tokio::test]\nasync fn test_async() { Some(1).expect(\"ok\"); }");
        assert!(v.is_empty());
    }

    #[test]
    fn tokio_test_unwrap_forbidden() {
        let v = check_code("#[tokio::test]\nasync fn test_async() { Some(1).unwrap(); }");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("test code"));
    }

    // --- fn main(): expect() 許可, unwrap() 禁止 ---

    #[test]
    fn main_fn_expect_allowed() {
        let v = check_code("fn main() { Some(1).expect(\"required\"); }");
        assert!(v.is_empty(), "expect() in fn main() should be allowed");
    }

    #[test]
    fn main_fn_unwrap_forbidden() {
        let v = check_code("fn main() { Some(1).unwrap(); }");
        assert_eq!(
            v.len(),
            1,
            "unwrap() in fn main() should still be forbidden"
        );
        assert!(v[0].message.contains("production"));
    }

    #[test]
    fn non_main_fn_expect_forbidden() {
        let v = check_code("fn setup() { Some(1).expect(\"msg\"); }");
        assert_eq!(v.len(), 1, "expect() in non-main fn should be forbidden");
    }
}
