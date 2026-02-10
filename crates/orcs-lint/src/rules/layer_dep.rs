use crate::context::{Layer, WorkspaceCtx};
use crate::types::{Location, Severity, Suggestion, Violation};

pub const NAME: &str = "layer-dep";

/// Cargo.toml の依存方向がレイヤー順に違反していないかチェックする。
/// 上位レイヤーは下位に依存できるが、逆方向は violation。
pub fn check(ctx: &WorkspaceCtx) -> Vec<Violation> {
    let mut violations = Vec::new();

    for member in &ctx.members {
        let Some(from_layer) = member.layer else {
            continue;
        };

        for dep_name in &member.dependencies {
            let Some(to_layer) = Layer::from_crate_name(dep_name) else {
                continue;
            };

            if !from_layer.can_depend_on(to_layer) {
                violations.push(
                    Violation::new(
                        NAME,
                        Severity::Error,
                        Location::new(member.path.join("Cargo.toml"), 1, 1),
                        format!(
                            "Layer violation: `{}` ({}) depends on `{}` ({}). \
                             Upper layers must not be depended on by lower layers.",
                            member.name, from_layer, dep_name, to_layer
                        ),
                    )
                    .with_suggestion(Suggestion::new(
                        "Remove the dependency or introduce an abstraction trait in a lower layer",
                    )),
                );
            }
        }
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CrateMeta, DepGraph};
    use std::path::PathBuf;

    fn make_ctx(members: Vec<CrateMeta>) -> WorkspaceCtx {
        WorkspaceCtx {
            root: PathBuf::from("."),
            members,
            dep_graph: DepGraph::default(),
        }
    }

    #[test]
    fn valid_dependency_direction() {
        let ctx = make_ctx(vec![CrateMeta {
            name: "orcs-runtime".to_string(),
            path: PathBuf::from("crates/orcs-runtime"),
            layer: Some(Layer::Runtime),
            dependencies: vec!["orcs-types".to_string(), "orcs-event".to_string()],
        }]);

        let violations = check(&ctx);
        assert!(violations.is_empty());
    }

    #[test]
    fn invalid_dependency_direction() {
        let ctx = make_ctx(vec![CrateMeta {
            name: "orcs-types".to_string(),
            path: PathBuf::from("crates/orcs-types"),
            layer: Some(Layer::Types),
            dependencies: vec!["orcs-runtime".to_string()],
        }]);

        let violations = check(&ctx);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, NAME);
        assert!(violations[0].message.contains("Layer violation"));
    }

    #[test]
    fn same_layer_dependency_denied() {
        let ctx = make_ctx(vec![CrateMeta {
            name: "orcs-event".to_string(),
            path: PathBuf::from("crates/orcs-event"),
            layer: Some(Layer::Event),
            dependencies: vec!["orcs-event".to_string()],
        }]);

        let violations = check(&ctx);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn external_dependency_ignored() {
        let ctx = make_ctx(vec![CrateMeta {
            name: "orcs-types".to_string(),
            path: PathBuf::from("crates/orcs-types"),
            layer: Some(Layer::Types),
            dependencies: vec!["serde".to_string(), "tokio".to_string()],
        }]);

        let violations = check(&ctx);
        assert!(violations.is_empty());
    }
}
