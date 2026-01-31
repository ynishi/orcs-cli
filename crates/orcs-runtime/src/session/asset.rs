//! Session Asset - The accumulating value at the core of ORCS.
//!
//! ## Overview
//!
//! `SessionAsset` is designed to accumulate value over time:
//!
//! - **History**: Conversation history (searchable, referenceable)
//! - **Preferences**: Learned user preferences
//! - **ProjectContext**: Project-specific information (grows over time)
//! - **SkillConfigs**: Skill customization settings
//! - **ComponentSnapshots**: Saved component states for resume

use chrono::{DateTime, Utc};
use orcs_component::ComponentSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Session version for compatibility checking.
pub const SESSION_VERSION: u32 = 1;

/// SessionAsset - The accumulating value.
///
/// This is the core data structure that persists between sessions.
/// It contains all the context and history that makes the assistant
/// more useful over time.
///
/// # Example
///
/// ```
/// use orcs_runtime::session::SessionAsset;
///
/// let mut asset = SessionAsset::new();
/// asset.project_context.name = Some("my-project".into());
///
/// // Save to JSON
/// let json = asset.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAsset {
    /// Asset ID (UUID).
    pub id: String,

    /// Session version for compatibility.
    pub version: u32,

    /// Creation timestamp (milliseconds since epoch).
    pub created_at_ms: u64,

    /// Last update timestamp (milliseconds since epoch).
    pub updated_at_ms: u64,

    /// Conversation history.
    pub history: ConversationHistory,

    /// User preferences.
    pub preferences: UserPreferences,

    /// Project context.
    pub project_context: ProjectContext,

    /// Skill configurations.
    pub skill_configs: HashMap<String, SkillConfig>,

    /// Component snapshots for resume.
    #[serde(default)]
    pub component_snapshots: HashMap<String, ComponentSnapshot>,

    /// Arbitrary metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl SessionAsset {
    /// Creates a new session asset with a fresh UUID.
    #[must_use]
    pub fn new() -> Self {
        let now = current_time_ms();
        Self {
            id: Uuid::new_v4().to_string(),
            version: SESSION_VERSION,
            created_at_ms: now,
            updated_at_ms: now,
            history: ConversationHistory::new(),
            preferences: UserPreferences::default(),
            project_context: ProjectContext::default(),
            skill_configs: HashMap::new(),
            component_snapshots: HashMap::new(),
            metadata: HashMap::new(),
        }
    }

    /// Creates a session asset for a specific project.
    #[must_use]
    pub fn for_project(path: impl Into<PathBuf>) -> Self {
        let mut asset = Self::new();
        asset.project_context.root_path = Some(path.into());
        asset
    }

    /// Updates the modification timestamp.
    pub fn touch(&mut self) {
        self.updated_at_ms = current_time_ms();
    }

    /// Adds a conversation turn.
    pub fn add_turn(&mut self, turn: ConversationTurn) {
        self.history.turns.push(turn);
        self.touch();
    }

    /// Updates a skill configuration.
    pub fn set_skill_config(&mut self, skill_id: impl Into<String>, config: SkillConfig) {
        self.skill_configs.insert(skill_id.into(), config);
        self.touch();
    }

    /// Saves a component snapshot.
    pub fn save_snapshot(&mut self, snapshot: ComponentSnapshot) {
        self.component_snapshots
            .insert(snapshot.component_fqn.clone(), snapshot);
        self.touch();
    }

    /// Retrieves a component snapshot by FQN.
    #[must_use]
    pub fn get_snapshot(&self, fqn: &str) -> Option<&ComponentSnapshot> {
        self.component_snapshots.get(fqn)
    }

    /// Removes a component snapshot by FQN.
    pub fn remove_snapshot(&mut self, fqn: &str) -> Option<ComponentSnapshot> {
        let snapshot = self.component_snapshots.remove(fqn);
        if snapshot.is_some() {
            self.touch();
        }
        snapshot
    }

    /// Clears all component snapshots.
    pub fn clear_snapshots(&mut self) {
        if !self.component_snapshots.is_empty() {
            self.component_snapshots.clear();
            self.touch();
        }
    }

    /// Returns the number of saved snapshots.
    #[must_use]
    pub fn snapshot_count(&self) -> usize {
        self.component_snapshots.len()
    }

    /// Returns the creation time as a DateTime.
    #[must_use]
    pub fn created_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.created_at_ms as i64).unwrap_or_else(Utc::now)
    }

    /// Returns the update time as a DateTime.
    #[must_use]
    pub fn updated_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.updated_at_ms as i64).unwrap_or_else(Utc::now)
    }

    /// Serializes to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserializes from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Calculates a checksum for integrity verification.
    #[must_use]
    pub fn checksum(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut hasher);
        self.updated_at_ms.hash(&mut hasher);
        self.history.turns.len().hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

impl Default for SessionAsset {
    fn default() -> Self {
        Self::new()
    }
}

/// Conversation history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationHistory {
    /// Conversation turns.
    pub turns: Vec<ConversationTurn>,

    /// Compacted (summarized) history.
    pub compacted: Vec<CompactedTurn>,

    /// Maximum turns to keep before compacting.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

fn default_max_turns() -> usize {
    1000
}

impl ConversationHistory {
    /// Creates a new empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            compacted: Vec::new(),
            max_turns: 1000,
        }
    }

    /// Returns the number of turns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Returns `true` if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Returns the most recent N turns.
    #[must_use]
    pub fn recent(&self, n: usize) -> &[ConversationTurn] {
        let start = self.turns.len().saturating_sub(n);
        &self.turns[start..]
    }

    /// Searches turns by keyword.
    #[must_use]
    pub fn search(&self, keyword: &str) -> Vec<&ConversationTurn> {
        let keyword_lower = keyword.to_lowercase();
        self.turns
            .iter()
            .filter(|turn| {
                turn.content.to_lowercase().contains(&keyword_lower)
                    || turn
                        .tags
                        .iter()
                        .any(|t| t.to_lowercase().contains(&keyword_lower))
            })
            .collect()
    }

    /// Filters turns by time range.
    #[must_use]
    pub fn in_range(&self, start_ms: u64, end_ms: u64) -> Vec<&ConversationTurn> {
        self.turns
            .iter()
            .filter(|turn| turn.timestamp_ms >= start_ms && turn.timestamp_ms <= end_ms)
            .collect()
    }

    /// Compacts old turns using the provided summarizer.
    ///
    /// If the number of turns exceeds `max_turns`, older turns are
    /// summarized and moved to `compacted`.
    pub fn compact(&mut self, summarizer: impl Fn(&[ConversationTurn]) -> String) {
        if self.turns.len() > self.max_turns {
            let to_compact = self.turns.len() - self.max_turns;
            let old_turns: Vec<_> = self.turns.drain(..to_compact).collect();

            let summary = summarizer(&old_turns);
            self.compacted.push(CompactedTurn {
                summary,
                turn_count: old_turns.len(),
                start_time_ms: old_turns.first().map(|t| t.timestamp_ms).unwrap_or(0),
                end_time_ms: old_turns.last().map(|t| t.timestamp_ms).unwrap_or(0),
            });
        }
    }
}

/// A single conversation turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    /// Turn ID (UUID).
    pub id: String,

    /// Timestamp (milliseconds since epoch).
    pub timestamp_ms: u64,

    /// Role: "user", "assistant", or "system".
    pub role: String,

    /// Content text.
    pub content: String,

    /// Tool calls made in this turn.
    pub tool_calls: Vec<ToolCallRecord>,

    /// Tags for search.
    pub tags: Vec<String>,

    /// Arbitrary metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl ConversationTurn {
    /// Creates a user turn.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            role: "user".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Creates an assistant turn.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            role: "assistant".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Creates a system turn.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp_ms: current_time_ms(),
            role: "system".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Adds tags.
    #[must_use]
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Adds tool calls.
    #[must_use]
    pub fn with_tool_calls(mut self, calls: Vec<ToolCallRecord>) -> Self {
        self.tool_calls = calls;
        self
    }
}

/// Record of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool name.
    pub tool_name: String,

    /// Arguments passed.
    pub args: serde_json::Value,

    /// Result (if available).
    pub result: Option<serde_json::Value>,

    /// Whether the call succeeded.
    pub success: bool,

    /// Duration in milliseconds.
    pub duration_ms: u64,
}

/// Compacted (summarized) turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactedTurn {
    /// Summary text.
    pub summary: String,

    /// Number of turns summarized.
    pub turn_count: usize,

    /// Start time of the summarized range.
    pub start_time_ms: u64,

    /// End time of the summarized range.
    pub end_time_ms: u64,
}

/// User preferences learned over time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserPreferences {
    /// Coding style preferences.
    pub coding_style: CodingStylePrefs,

    /// Communication preferences.
    pub communication: CommunicationPrefs,

    /// Frequently used tools.
    pub frequent_tools: Vec<String>,

    /// Custom preferences.
    pub custom: HashMap<String, serde_json::Value>,
}

/// Coding style preferences.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodingStylePrefs {
    /// Preferred languages.
    pub preferred_languages: Vec<String>,

    /// Indentation type ("spaces" or "tabs").
    pub indent: Option<String>,

    /// Indentation size.
    pub indent_size: Option<u8>,

    /// Naming convention.
    pub naming_convention: Option<String>,

    /// Comment style.
    pub comment_style: Option<String>,
}

/// Communication preferences.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommunicationPrefs {
    /// Verbosity level ("concise", "balanced", "verbose").
    pub verbosity: Option<String>,

    /// Preferred language.
    pub language: Option<String>,

    /// Formality level ("casual", "professional").
    pub formality: Option<String>,

    /// Whether to include code examples.
    #[serde(default)]
    pub include_examples: bool,
}

/// Project-specific context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectContext {
    /// Project root path.
    pub root_path: Option<PathBuf>,

    /// Project name.
    pub name: Option<String>,

    /// Technologies used.
    pub technologies: Vec<String>,

    /// Key files.
    pub key_files: Vec<String>,

    /// Architecture notes.
    pub architecture_notes: Vec<String>,

    /// Learned facts.
    pub learned_facts: Vec<LearnedFact>,

    /// Persistent context injections.
    pub persistent_contexts: Vec<ContextInjection>,
}

impl ProjectContext {
    /// Adds a learned fact (deduplicates by key).
    pub fn learn(&mut self, fact: LearnedFact) {
        if !self.learned_facts.iter().any(|f| f.key == fact.key) {
            self.learned_facts.push(fact);
        }
    }

    /// Adds a persistent context.
    pub fn add_persistent_context(&mut self, ctx: ContextInjection) {
        self.persistent_contexts.push(ctx);
    }

    /// Finds facts matching a keyword.
    #[must_use]
    pub fn find_facts(&self, keyword: &str) -> Vec<&LearnedFact> {
        let keyword_lower = keyword.to_lowercase();
        self.learned_facts
            .iter()
            .filter(|f| {
                f.key.to_lowercase().contains(&keyword_lower)
                    || f.value.to_lowercase().contains(&keyword_lower)
            })
            .collect()
    }
}

/// A fact learned about the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedFact {
    /// Key for lookup.
    pub key: String,

    /// Value.
    pub value: String,

    /// Source of the fact.
    pub source: String,

    /// Confidence (0.0-1.0).
    pub confidence: f32,

    /// When the fact was learned.
    pub learned_at_ms: u64,
}

impl LearnedFact {
    /// Creates a new fact with full confidence.
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source: source.into(),
            confidence: 1.0,
            learned_at_ms: current_time_ms(),
        }
    }

    /// Sets the confidence level.
    #[must_use]
    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }
}

/// Context to inject into conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextInjection {
    /// Injection ID.
    pub id: String,

    /// When to inject ("always", "on_request", etc.).
    pub timing: String,

    /// Content to inject.
    pub content: String,
}

/// Skill configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConfig {
    /// Skill ID.
    pub skill_id: String,

    /// Whether the skill is enabled.
    pub enabled: bool,

    /// Auto-trigger configuration.
    pub auto_trigger: AutoTriggerConfig,

    /// Custom parameters.
    pub params: HashMap<String, serde_json::Value>,
}

impl SkillConfig {
    /// Creates a new enabled skill config.
    #[must_use]
    pub fn new(skill_id: impl Into<String>) -> Self {
        Self {
            skill_id: skill_id.into(),
            enabled: true,
            auto_trigger: AutoTriggerConfig::default(),
            params: HashMap::new(),
        }
    }

    /// Creates a disabled skill config.
    #[must_use]
    pub fn disabled(skill_id: impl Into<String>) -> Self {
        Self {
            enabled: false,
            ..Self::new(skill_id)
        }
    }
}

/// Auto-trigger configuration for skills.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoTriggerConfig {
    /// Whether auto-triggering is enabled.
    pub enabled: bool,

    /// Trigger conditions.
    pub triggers: Vec<TriggerCondition>,
}

/// Trigger condition for auto-firing skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// On file edit matching glob.
    OnFileEdit { glob: String },

    /// Before LLM request.
    OnLLMPreRequest,

    /// After LLM response.
    OnLLMPostResponse,

    /// On custom event.
    OnEvent { kind: String },

    /// Periodic interval.
    Interval { seconds: u64 },
}

// === Helper functions ===

fn current_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_asset_new() {
        let asset = SessionAsset::new();
        assert!(!asset.id.is_empty());
        assert_eq!(asset.version, SESSION_VERSION);
        assert!(asset.history.is_empty());
    }

    #[test]
    fn session_asset_for_project() {
        let asset = SessionAsset::for_project("/path/to/project");
        assert_eq!(
            asset.project_context.root_path,
            Some(PathBuf::from("/path/to/project"))
        );
    }

    #[test]
    fn session_asset_touch() {
        let mut asset = SessionAsset::new();
        let original = asset.updated_at_ms;
        std::thread::sleep(std::time::Duration::from_millis(10));
        asset.touch();
        assert!(asset.updated_at_ms >= original);
    }

    #[test]
    fn conversation_history_search() {
        let mut history = ConversationHistory::new();
        history.turns.push(ConversationTurn::user("Hello Rust"));
        history
            .turns
            .push(ConversationTurn::user("Python is great"));
        history
            .turns
            .push(ConversationTurn::user("rust programming"));

        let results = history.search("rust");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn conversation_history_recent() {
        let mut history = ConversationHistory::new();
        for i in 0..10 {
            history
                .turns
                .push(ConversationTurn::user(format!("Turn {i}")));
        }

        let recent = history.recent(3);
        assert_eq!(recent.len(), 3);
        assert!(recent[0].content.contains('7'));
    }

    #[test]
    fn project_context_learn() {
        let mut ctx = ProjectContext::default();
        ctx.learn(LearnedFact::new("db", "PostgreSQL", "user"));
        ctx.learn(LearnedFact::new("framework", "Actix", "codebase"));

        let facts = ctx.find_facts("postgre");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, "PostgreSQL");
    }

    #[test]
    fn session_asset_json_roundtrip() {
        let mut asset = SessionAsset::new();
        asset.add_turn(ConversationTurn::user("test"));
        asset.preferences.coding_style.preferred_languages = vec!["Rust".into()];

        let json = asset.to_json().unwrap();
        let restored = SessionAsset::from_json(&json).unwrap();

        assert_eq!(asset.id, restored.id);
        assert_eq!(restored.history.len(), 1);
        assert_eq!(
            restored.preferences.coding_style.preferred_languages,
            vec!["Rust".to_string()]
        );
    }

    #[test]
    fn skill_config_new() {
        let config = SkillConfig::new("lint");
        assert!(config.enabled);
        assert_eq!(config.skill_id, "lint");
    }

    #[test]
    fn checksum_changes_on_update() {
        let mut asset = SessionAsset::new();
        let checksum1 = asset.checksum();

        asset.add_turn(ConversationTurn::user("test"));
        let checksum2 = asset.checksum();

        assert_ne!(checksum1, checksum2);
    }
}
