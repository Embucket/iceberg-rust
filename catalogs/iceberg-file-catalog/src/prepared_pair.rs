//! Explicit local two-head append publication. SQL/cache snapshot integration is separate.

use super::*;
use uuid::Uuid;

/// Whether a failed publication may have made a durable commit decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairOutcome {
    NotCommitted,
    DecisionUnknown,
    CommittedNeedsRecovery,
}

#[derive(Debug, thiserror::Error)]
#[error("prepared append pair {outcome:?}: {source}")]
pub struct PairPublishError {
    pub outcome: PairOutcome,
    #[source]
    pub source: IcebergError,
}

/// An opaque candidate: constructing a native CommitTable alone is not publication authority.
pub struct PreparedAppendPair {
    _private: (),
}

/// Exact published metadata images, before an embedding engine refreshes its own providers.
#[derive(Debug)]
pub struct CommittedPairReceipt {
    pub operation_id: Uuid,
    pub tables: [(Identifier, TableMetadata); 2],
}

/// One cooperating reader's catalog snapshot exclusion, never a SQL statement/cache guard.
pub struct FileCatalogReadScope {
    catalog: Arc<FileCatalog>,
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl FileCatalogReadScope {
    pub async fn load(&self, identifier: &Identifier) -> Result<Tabular, IcebergError> {
        Arc::clone(&self.catalog).load_tabular(identifier).await
    }
}

impl FileCatalog {
    pub async fn read_scope(self: &Arc<Self>) -> Result<FileCatalogReadScope, IcebergError> {
        Ok(FileCatalogReadScope {
            catalog: Arc::clone(self),
            _guard: self.catalog_mutation_lock().read_owned().await,
        })
    }

    pub async fn prepare_append_pair(
        self: &Arc<Self>,
        _commits: [CommitTable; 2],
    ) -> Result<PreparedAppendPair, IcebergError> {
        Err(IcebergError::NotSupported(
            "prepared append pair is not implemented".to_owned(),
        ))
    }
}

impl PreparedAppendPair {
    pub async fn publish(self) -> Result<CommittedPairReceipt, PairPublishError> {
        Err(PairPublishError {
            outcome: PairOutcome::NotCommitted,
            source: IcebergError::NotSupported(
                "prepared append pair is not implemented".to_owned(),
            ),
        })
    }
}

#[cfg(test)]
#[path = "prepared_pair_tests.rs"]
mod tests;
