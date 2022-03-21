use datasize::DataSize;
use serde::{Deserialize, Serialize};

use crate::types::{BlockHash, TimeDiff};

/// Maximum number of deploys to fetch in parallel, by default.
const DEFAULT_MAX_PARALLEL_DEPLOY_FETCHES: u32 = 20;
/// Maximum number of tries to fetch in parallel, by default.
const DEFAULT_MAX_PARALLEL_TRIE_FETCHES: u32 = 20;
const DEFAULT_RETRY_INTERVAL: &str = "100ms";

/// Node fast-sync configuration.
#[derive(DataSize, Debug, Deserialize, Serialize, Clone)]
// Disallow unknown fields to ensure config files and command-line overrides contain valid keys.
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Hash used as a trust anchor when joining, if any.
    pub trusted_hash: Option<BlockHash>,

    /// Maximum number of deploys to fetch in parallel.
    pub max_parallel_deploy_fetches: u32,

    /// Maximum number of trie nodes to fetch in parallel.
    pub max_parallel_trie_fetches: u32,

    /// The duration for which to pause between retry attempts while synchronising during joining.
    pub retry_interval: TimeDiff,

    /// Whether to run in archival-sync mode. Archival-sync mode captures all data (blocks, deploys
    /// and global state) back to genesis.
    pub archival_sync: bool,
}

impl Default for NodeConfig {
    fn default() -> NodeConfig {
        NodeConfig {
            trusted_hash: None,
            max_parallel_deploy_fetches: DEFAULT_MAX_PARALLEL_DEPLOY_FETCHES,
            max_parallel_trie_fetches: DEFAULT_MAX_PARALLEL_TRIE_FETCHES,
            // NOTE: Safe to unwrap as DEFAULT_RETRY_INTERVAL is correct
            retry_interval: DEFAULT_RETRY_INTERVAL.parse().unwrap(), //?unwrap
            archival_sync: false,
        }
    }
}
