//! Borrowed reads; no mutation, trait recursion or interpretation of SQL markers.

use super::*;

impl FileCatalogReadScope {
    /// Read one exact plain local endpoint while retaining this reader.
    ///
    /// `None` requires positive no-follow physical endpoint absence under a live,
    /// non-symlink namespace. Every existing endpoint must have a regular metadata
    /// directory/head and pass the ordinary full native loader; missing/corrupt
    /// metadata never becomes `None`. Head selection remains the native catalog's
    /// existing rule. This bounded API requires Unix filesystem object identities.
    /// It shares the native cooperating-writer contract, not hostile filesystem
    /// write protection, SQL/cache lifetime exclusion or authorization.
    pub async fn load_if_present(
        &self,
        identifier: &Identifier,
    ) -> Result<Option<Tabular>, IcebergError> {
        #[cfg(unix)]
        {
            self.load_if_present_unix(identifier).await
        }
        #[cfg(not(unix))]
        {
            let _ = identifier;
            Err(refusal(
                "scoped optional reads require Unix filesystem identities",
            ))
        }
    }

    #[cfg(unix)]
    async fn load_if_present_unix(
        &self,
        identifier: &Identifier,
    ) -> Result<Option<Tabular>, IcebergError> {
        self.catalog.check_pair_health()?;
        if !self
            .catalog
            .renamed_tables
            .read()
            .map_err(|_| refusal("scoped optional read alias lock is poisoned"))?
            .is_empty()
        {
            return Err(refusal(
                "scoped optional read refuses active legacy rename aliases",
            ));
        }
        if identifier.namespace().len() != 1
            || !plain_component(&identifier.namespace()[0])
            || !plain_component(identifier.name())
        {
            return Err(refusal(
                "scoped optional read requires one exact plain local endpoint",
            ));
        }
        let root = self.catalog.pair_root()?;
        let namespace = root.path.join(&identifier.namespace()[0]);
        let namespace_before = directory(&namespace)?;
        let endpoint = namespace.join(identifier.name());
        let endpoint_before = match std::fs::symlink_metadata(&endpoint) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // ENOENT is not enough: reprove the same actual live parent and
                // original configured root before admitting an absent leaf.
                same(namespace_before, directory(&namespace)?)?;
                same_root(&self.catalog, &root)?;
                self.catalog.check_pair_health()?;
                return Ok(None);
            }
            Err(error) => return Err(io(error)),
            Ok(_) => directory(&endpoint)?,
        };
        let metadata = endpoint.join("metadata");
        let metadata_before = directory(&metadata)?;
        // CatalogNotFound here is an incoherent EXISTING endpoint, never None.
        let location = self.catalog.metadata_location(identifier).await?;
        let native = root.native(&location)?;
        if native
            .parent()
            .ok_or_else(|| refusal("scoped metadata parent is missing"))?
            .canonicalize()
            .map_err(io)?
            != metadata
        {
            return Err(refusal(
                "scoped metadata head escaped its exact physical endpoint",
            ));
        }
        let head = metadata.join(
            native
                .file_name()
                .ok_or_else(|| refusal("scoped metadata head is missing"))?,
        );
        let head_before = regular(&head)?;
        let tabular = self.load(identifier).await?;
        if self.catalog.metadata_location(identifier).await? != location {
            return Err(refusal("scoped native head changed during its full load"));
        }
        same(namespace_before, directory(&namespace)?)?;
        same(endpoint_before, directory(&endpoint)?)?;
        same(metadata_before, directory(&metadata)?)?;
        same(head_before, regular(&head)?)?;
        same_root(&self.catalog, &root)?;
        self.catalog.check_pair_health()?;
        Ok(Some(tabular))
    }
}

#[cfg(unix)]
#[derive(PartialEq, Eq)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> ObjectIdentity {
    use std::os::unix::fs::MetadataExt;
    ObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        length: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}
#[cfg(unix)]
fn directory(path: &NativePath) -> Result<ObjectIdentity, IcebergError> {
    let metadata = std::fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || path.canonicalize().map_err(io)? != path
    {
        return Err(refusal(
            "scoped native directory is missing, redirected or not a directory",
        ));
    }
    Ok(identity(&metadata))
}
#[cfg(unix)]
fn regular(path: &NativePath) -> Result<ObjectIdentity, IcebergError> {
    let metadata = std::fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || path.canonicalize().map_err(io)? != path
    {
        return Err(refusal("scoped native head is not an exact regular file"));
    }
    Ok(identity(&metadata))
}
#[cfg(unix)]
fn same(before: ObjectIdentity, after: ObjectIdentity) -> Result<(), IcebergError> {
    if before != after {
        return Err(refusal("scoped native object changed during its read"));
    }
    Ok(())
}
#[cfg(unix)]
fn same_root(catalog: &FileCatalog, before: &NativeRoot) -> Result<(), IcebergError> {
    let after = catalog.pair_root()?;
    if before.key != after.key || before.path != after.path || before.location != after.location {
        return Err(refusal("scoped native root changed during its read"));
    }
    Ok(())
}
