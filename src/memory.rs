//! Memory storage and retrieval system.

pub mod analyzer;
pub mod consolidation;
pub mod embedding;
pub mod lance;
pub mod maintenance;
pub mod render;
pub mod search;
pub mod sqlite_index;
pub mod store;
pub mod types;
pub mod working;

pub use embedding::EmbeddingModel;
pub use lance::{ChronicleEmbeddingTable, ChronicleHit, EmbeddingTable};
pub use search::{MemorySearch, SearchConfig, SearchMode, SearchSort, curate_results};
pub use store::MemoryStore;
pub use types::{Association, Memory, MemoryType, RelationType};
pub use working::{WorkingMemoryEventType, WorkingMemoryStore};
