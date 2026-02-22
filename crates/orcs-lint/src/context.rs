use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// orcs-cli ワークスペースのレイヤー定義。
/// 数値が小さいほど下位（依存される側）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    Types = 0,
    Event = 1,
    Auth = 2,
    Component = 3,
    Runtime = 4,
    Plugin = 5,
    App = 6,
    Cli = 7,
    DevTool = 8,
}

impl Layer {
    /// crate 名からレイヤーを解決する。
    #[must_use]
    pub fn from_crate_name(name: &str) -> Option<Self> {
        match name {
            "orcs-types" => Some(Self::Types),
            "orcs-event" => Some(Self::Event),
            "orcs-auth" => Some(Self::Auth),
            "orcs-component" => Some(Self::Component),
            "orcs-runtime" => Some(Self::Runtime),
            "orcs-lua" => Some(Self::Plugin),
            "orcs-app" => Some(Self::App),
            "orcs-cli" => Some(Self::Cli),
            "orcs-lint" => Some(Self::DevTool),
            _ => None,
        }
    }

    /// 上位レイヤーが下位に依存してよいかチェック。
    /// `self` が `dep` に依存する場合、`self > dep` であるべき。
    #[must_use]
    pub fn can_depend_on(self, dep: Self) -> bool {
        // DevTool は何にでも依存可（ただし実際は非依存）
        if self == Self::DevTool {
            return true;
        }
        self > dep
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Types => "types",
            Self::Event => "event",
            Self::Auth => "auth",
            Self::Component => "component",
            Self::Runtime => "runtime",
            Self::Plugin => "plugin",
            Self::App => "app",
            Self::Cli => "cli",
            Self::DevTool => "devtool",
        };
        write!(f, "{name}")
    }
}

/// ファイル解析時のコンテキスト。
#[derive(Debug, Clone)]
pub struct FileCtx<'a> {
    pub path: &'a Path,
    pub content: &'a str,
    pub relative_path: PathBuf,
    pub is_test: bool,
    pub layer: Option<Layer>,
    pub module_path: Vec<String>,
}

impl<'a> FileCtx<'a> {
    #[must_use]
    pub fn new(path: &'a Path, content: &'a str, root: &Path) -> Self {
        let is_test = detect_test_file(path);
        let relative_path = path
            .strip_prefix(root)
            .map_or_else(|_| path.to_path_buf(), Path::to_path_buf);
        let layer = resolve_layer_from_path(&relative_path);
        let module_path = compute_module_path(&relative_path);

        Self {
            path,
            content,
            relative_path,
            is_test,
            layer,
            module_path,
        }
    }

    /// 指定行のバイトオフセットを計算する。
    #[must_use]
    pub fn offset_for(&self, line: usize, column: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut offset = 0;
        for (i, line_content) in self.content.lines().enumerate() {
            if i + 1 == line {
                return offset + column.saturating_sub(1);
            }
            offset += line_content.len() + 1;
        }
        offset
    }
}

/// Cargo.toml から抽出した crate メタ情報。
#[derive(Debug, Clone)]
pub struct CrateMeta {
    pub name: String,
    pub path: PathBuf,
    pub layer: Option<Layer>,
    pub dependencies: Vec<String>,
}

/// ワークスペース構造解析のコンテキスト。
#[derive(Debug, Clone)]
pub struct WorkspaceCtx {
    pub root: PathBuf,
    pub members: Vec<CrateMeta>,
    pub dep_graph: DepGraph,
}

/// crate 間依存グラフ。
#[derive(Debug, Clone, Default)]
pub struct DepGraph {
    edges: HashMap<String, Vec<String>>,
}

impl DepGraph {
    pub fn add_edge(&mut self, from: &str, to: &str) {
        self.edges
            .entry(from.to_string())
            .or_default()
            .push(to.to_string());
    }

    #[must_use]
    pub fn dependencies_of(&self, crate_name: &str) -> &[String] {
        self.edges.get(crate_name).map_or(&[], Vec::as_slice)
    }
}

fn detect_test_file(path: &Path) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(s) = component {
            let s = s.to_string_lossy();
            if s == "tests" || s == "test" || s == "benches" {
                return true;
            }
        }
    }
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        if file_name.ends_with("_test.rs")
            || file_name.ends_with("_tests.rs")
            || file_name.starts_with("test_")
            || file_name == "tests.rs"
            || file_name == "testing.rs"
        {
            return true;
        }
    }
    false
}

fn resolve_layer_from_path(relative_path: &Path) -> Option<Layer> {
    // crates/orcs-xxx/... → "orcs-xxx" を抽出
    let mut components = relative_path.components();
    let first = components.next()?;
    if first.as_os_str() != "crates" {
        return None;
    }
    let crate_dir = components.next()?;
    Layer::from_crate_name(crate_dir.as_os_str().to_str()?)
}

fn compute_module_path(relative_path: &Path) -> Vec<String> {
    let mut parts: Vec<String> = relative_path
        .with_extension("")
        .components()
        .filter_map(|c| {
            if let std::path::Component::Normal(s) = c {
                s.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();

    if let Some(last) = parts.last() {
        if last == "mod" || last == "lib" {
            parts.pop();
        }
    }

    if !parts.is_empty() {
        parts.insert(0, "crate".to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_ordering() {
        assert!(Layer::Runtime.can_depend_on(Layer::Types));
        assert!(Layer::App.can_depend_on(Layer::Runtime));
        assert!(!Layer::Types.can_depend_on(Layer::Runtime));
        assert!(!Layer::Runtime.can_depend_on(Layer::App));
    }

    #[test]
    fn layer_same_level_denied() {
        assert!(!Layer::Event.can_depend_on(Layer::Event));
    }

    #[test]
    fn devtool_can_depend_on_anything() {
        assert!(Layer::DevTool.can_depend_on(Layer::Types));
        assert!(Layer::DevTool.can_depend_on(Layer::Cli));
    }

    #[test]
    fn resolve_layer() {
        assert_eq!(
            resolve_layer_from_path(Path::new("crates/orcs-types/src/lib.rs")),
            Some(Layer::Types)
        );
        assert_eq!(
            resolve_layer_from_path(Path::new("crates/orcs-runtime/src/engine.rs")),
            Some(Layer::Runtime)
        );
        assert_eq!(resolve_layer_from_path(Path::new("src/main.rs")), None);
    }

    #[test]
    fn test_file_detection() {
        assert!(detect_test_file(Path::new(
            "crates/orcs-types/tests/foo.rs"
        )));
        assert!(detect_test_file(Path::new("src/foo_test.rs")));
        assert!(!detect_test_file(Path::new("src/lib.rs")));
    }

    #[test]
    fn from_crate_name_coverage() {
        assert_eq!(Layer::from_crate_name("orcs-types"), Some(Layer::Types));
        assert_eq!(Layer::from_crate_name("orcs-lua"), Some(Layer::Plugin));
        assert_eq!(Layer::from_crate_name("unknown"), None);
    }
}
