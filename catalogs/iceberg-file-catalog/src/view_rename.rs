//! Test-first owned view rename API. Publication remains explicitly unsupported
//! until the bounded full-image namespace protocol is implemented and qualified.

use super::*;

#[derive(Debug, thiserror::Error)]
#[error("local view rename {outcome:?}: {source}")]
pub struct ViewRenameError {
    pub outcome: PairOutcome,
    #[source]
    pub source: IcebergError,
}

/// Opaque single-owner preparation. The initial API stub never constructs it.
#[derive(Debug)]
pub struct PreparedViewRename {
    _private: (),
    #[cfg(test)]
    hook: Option<Arc<TestHook>>,
}

impl FileCatalog {
    pub async fn prepare_view_rename(
        self: &Arc<Self>,
        _source: &Identifier,
        _destination: &Identifier,
    ) -> Result<PreparedViewRename, IcebergError> {
        Err(refusal("local view rename publisher is not implemented"))
    }
}

impl PreparedViewRename {
    pub async fn publish(self) -> Result<View, ViewRenameError> {
        #[cfg(test)]
        let _ = self.hook;
        Err(ViewRenameError {
            outcome: PairOutcome::NotCommitted,
            source: refusal("local view rename publisher is not implemented"),
        })
    }
}
