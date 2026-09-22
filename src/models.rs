use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Decision,
    Task,
    Invariant,
    Blocker,
    Ephemeral,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Decision => "decision",
            MemoryKind::Task => "task",
            MemoryKind::Invariant => "invariant",
            MemoryKind::Blocker => "blocker",
            MemoryKind::Ephemeral => "ephemeral",
        }
    }

    pub fn prefix(&self) -> &'static str {
        match self {
            MemoryKind::Decision => "DEC",
            MemoryKind::Task => "TSK",
            MemoryKind::Invariant => "INV",
            MemoryKind::Blocker => "BLK",
            MemoryKind::Ephemeral => "EPH",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().trim() {
            "decision" => Ok(MemoryKind::Decision),
            "task" => Ok(MemoryKind::Task),
            "invariant" => Ok(MemoryKind::Invariant),
            "blocker" => Ok(MemoryKind::Blocker),
            "ephemeral" => Ok(MemoryKind::Ephemeral),
            other => Err(format!(
                "Invalid memory kind: {}. Must be one of: decision, task, invariant, blocker, ephemeral",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Resolved,
    Ephemeral,
}

impl MemoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Superseded => "superseded",
            MemoryStatus::Resolved => "resolved",
            MemoryStatus::Ephemeral => "ephemeral",
        }
    }
}

impl fmt::Display for MemoryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for MemoryStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().trim() {
            "active" => Ok(MemoryStatus::Active),
            "superseded" => Ok(MemoryStatus::Superseded),
            "resolved" => Ok(MemoryStatus::Resolved),
            "ephemeral" => Ok(MemoryStatus::Ephemeral),
            other => Err(format!(
                "Invalid memory status: {}. Must be one of: active, superseded, resolved, ephemeral",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    Supersedes,
    DependsOn,
    Blocks,
    References,
}

impl RelationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationType::Supersedes => "supersedes",
            RelationType::DependsOn => "depends_on",
            RelationType::Blocks => "blocks",
            RelationType::References => "references",
        }
    }

    pub fn inverse(&self) -> &'static str {
        match self {
            RelationType::Supersedes => "superseded_by",
            RelationType::DependsOn => "depended_on_by",
            RelationType::Blocks => "blocked_by",
            RelationType::References => "referenced_by",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    #[default]
    Outgoing,
    Incoming,
}

impl fmt::Display for RelationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RelationType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().trim() {
            "supersedes" => Ok(RelationType::Supersedes),
            "depends_on" | "dependson" | "depends" => Ok(RelationType::DependsOn),
            "blocks" | "block" => Ok(RelationType::Blocks),
            "references" | "reference" => Ok(RelationType::References),
            other => Err(format!(
                "Invalid relation type: {}. Must be one of: supersedes, depends_on, blocks, references",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub tags: Option<String>,
    pub status: MemoryStatus,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedEntity {
    pub relation_type: RelationType,
    pub target_id: String,
    pub target_title: Option<String>,
    pub target_kind: Option<MemoryKind>,
    pub target_status: Option<MemoryStatus>,
    #[serde(default)]
    pub direction: EdgeDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub rank: f64,
    pub relations: Vec<RelatedEntity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLearning {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default = "default_learning_kind")]
    pub kind: MemoryKind,
}

fn default_learning_kind() -> MemoryKind {
    MemoryKind::Decision
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub source_id: String,
    pub target_id: String,
    pub relation_type: RelationType,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportData {
    pub memories: Vec<Memory>,
    pub relations: Vec<MemoryRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseStats {
    pub total_memories: i64,
    pub active_memories: i64,
    pub superseded_memories: i64,
    pub resolved_memories: i64,
    pub ephemeral_memories: i64,
    pub total_relations: i64,
    pub relations_by_type: Vec<(String, i64)>,
}
