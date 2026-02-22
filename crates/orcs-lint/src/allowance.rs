use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowState {
    Denied,
    Allowed,
}

impl AllowState {
    #[must_use]
    pub fn is_allowed(self) -> bool {
        self == Self::Allowed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowCheck {
    Denied,
    Allowed { reason: Option<String> },
}

impl AllowCheck {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allowed { reason } => reason.as_deref(),
            Self::Denied => None,
        }
    }
}

/// `// orcs-lint: allow(rule) reason="..."` コメントを解析する。
#[must_use]
pub fn check_allow_with_reason(content: &str, line: usize, rule_name: &str) -> AllowCheck {
    let lines: Vec<&str> = content.lines().collect();

    for check_line in [line.saturating_sub(1), line] {
        if check_line == 0 || check_line > lines.len() {
            continue;
        }

        let line_content = lines[check_line - 1];
        if let Some(directive) = parse_allow_directive(line_content) {
            if directive.rules.contains(rule_name) || directive.rules.contains("all") {
                return AllowCheck::Allowed {
                    reason: directive.reason,
                };
            }
        }
    }

    AllowCheck::Denied
}

struct AllowDirective {
    rules: HashSet<String>,
    reason: Option<String>,
}

fn parse_allow_directive(line: &str) -> Option<AllowDirective> {
    let line = line.trim();

    let comment_content = if let Some(rest) = line.strip_prefix("///") {
        rest.trim()
    } else if let Some(rest) = line.strip_prefix("//") {
        rest.trim()
    } else {
        return None;
    };

    let directive = comment_content.strip_prefix("orcs-lint:")?.trim();
    let allow_content = directive.strip_prefix("allow(")?.trim();

    let paren_end = allow_content.find(')')?;
    let rules_str = &allow_content[..paren_end];

    let rules: HashSet<String> = rules_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if rules.is_empty() {
        return None;
    }

    let rest = allow_content[paren_end + 1..].trim();
    let reason = if let Some(reason_part) = rest.strip_prefix("reason=") {
        let reason_part = reason_part.trim();
        if reason_part.starts_with('"') && reason_part.len() > 1 {
            let end = reason_part[1..].find('"').map(|i| i + 1)?;
            Some(reason_part[1..end].to_string())
        } else {
            None
        }
    } else {
        None
    };

    Some(AllowDirective { rules, reason })
}

/// `#[cfg(test)]` or `#[cfg(any(test, ...))]` 属性の検出。
pub fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        // #[cfg(test)]
        if attr
            .parse_args::<syn::Ident>()
            .is_ok_and(|ident| ident == "test")
        {
            return true;
        }
        // #[cfg(any(test, ...))] — normalized token check
        let Ok(meta_list) = attr.meta.require_list() else {
            return false;
        };
        let normalized: String = meta_list
            .tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        normalized.starts_with("any(")
            && (normalized.contains("(test,")
                || normalized.contains(",test,")
                || normalized.contains(",test)")
                || normalized == "any(test)")
    })
}

/// `#[test]` / `#[tokio::test]` 属性の検出。
pub fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.is_ident("test") {
            return true;
        }
        // tokio::test — 2セグメントパスは is_ident ではマッチしない
        let segments: Vec<_> = path.segments.iter().collect();
        segments.len() == 2 && segments[0].ident == "tokio" && segments[1].ident == "test"
    })
}

/// `#[allow(...)]` 属性の検出。
/// `tokens.to_string()` はスペースが入る場合があるため正規化して比較する。
pub fn has_allow_attr(attrs: &[syn::Attribute], targets: &[&str]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("allow") {
            return false;
        }
        let Ok(meta_list) = attr.meta.require_list() else {
            return false;
        };
        let tokens: String = meta_list
            .tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        targets.iter().any(|t| tokens.contains(t))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_directive_basic() {
        let content = "fn foo() {\n    // orcs-lint: allow(no-unwrap)\n    v.unwrap();\n}";
        let result = check_allow_with_reason(content, 3, "no-unwrap");
        assert!(result.is_allowed());
        assert!(result.reason().is_none());
    }

    #[test]
    fn allow_directive_with_reason() {
        let content =
            "fn foo() {\n    // orcs-lint: allow(no-unwrap) reason=\"guaranteed safe\"\n    v.unwrap();\n}";
        let result = check_allow_with_reason(content, 3, "no-unwrap");
        assert!(result.is_allowed());
        assert_eq!(result.reason(), Some("guaranteed safe"));
    }

    #[test]
    fn allow_directive_wrong_rule() {
        let content = "fn foo() {\n    // orcs-lint: allow(no-panic)\n    v.unwrap();\n}";
        let result = check_allow_with_reason(content, 3, "no-unwrap");
        assert!(!result.is_allowed());
    }

    #[test]
    fn allow_all() {
        let content = "fn foo() {\n    // orcs-lint: allow(all)\n    v.unwrap();\n}";
        let result = check_allow_with_reason(content, 3, "no-unwrap");
        assert!(result.is_allowed());
    }

    #[test]
    fn no_directive() {
        let content = "fn foo() {\n    v.unwrap();\n}";
        let result = check_allow_with_reason(content, 2, "no-unwrap");
        assert!(!result.is_allowed());
    }

    #[test]
    fn has_test_attr_detects_plain_test() {
        let item: syn::ItemFn =
            syn::parse_str("#[test]\nfn t() {}").expect("should parse #[test] fn");
        assert!(has_test_attr(&item.attrs));
    }

    #[test]
    fn has_test_attr_detects_tokio_test() {
        let item: syn::ItemFn = syn::parse_str("#[tokio::test]\nasync fn t() {}")
            .expect("should parse #[tokio::test] fn");
        assert!(has_test_attr(&item.attrs));
    }

    #[test]
    fn has_test_attr_rejects_non_test() {
        let item: syn::ItemFn =
            syn::parse_str("#[inline]\nfn t() {}").expect("should parse #[inline] fn");
        assert!(!has_test_attr(&item.attrs));
    }
}
