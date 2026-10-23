use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex as StdMutex, OnceLock, RwLock, Weak},
};

use async_trait::async_trait;
use futures::TryStreamExt;
use iceberg_rust::{
    catalog::{
        commit::{
            apply_table_updates, apply_view_updates, check_table_requirements,
            check_view_requirements, CommitTable, CommitView,
        },
        create::{CreateMaterializedView, CreateTable, CreateView},
        identifier::Identifier,
        namespace::Namespace,
        tabular::Tabular,
        Catalog, CatalogList,
    },
    error::Error as IcebergError,
    materialized_view::MaterializedView,
    object_store::{store::IcebergStore, Bucket, ObjectStoreBuilder},
    spec::{
        materialized_view_metadata::MaterializedViewMetadata,
        table_metadata::{new_metadata_location, TableMetadata},
        tabular::{TabularMetadata, TabularMetadataRef},
        util::strip_prefix,
        view_metadata::ViewMetadata,
    },
    table::Table,
    view::View,
};
use object_store::{path::Path as ObjectPath, ObjectStore, ObjectStoreExt, PutMode, PutOptions};
use percent_encoding::percent_decode_str;

use crate::error::Error;

#[derive(Debug)]
pub struct FileCatalog {
    path: String,
    object_store: ObjectStoreBuilder,
    cache: Arc<RwLock<HashMap<Identifier, (String, TabularMetadata)>>>,
    renamed_tables: Arc<RwLock<HashMap<Identifier, Identifier>>>,
    #[cfg(test)]
    update_before_publish_pause: StdMutex<
        Option<(
            futures::channel::oneshot::Sender<()>,
            futures::channel::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    create_after_write_pause: StdMutex<
        Option<(
            futures::channel::oneshot::Sender<()>,
            futures::channel::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    drop_lock_reached: std::sync::atomic::AtomicBool,
}

type MutationLock = tokio::sync::Mutex<()>;
type MutationLockKey = (String, Identifier);
static TABLE_MUTATION_LOCKS: OnceLock<StdMutex<HashMap<MutationLockKey, Weak<MutationLock>>>> =
    OnceLock::new();
type CatalogMutationLock = tokio::sync::RwLock<()>;
static CATALOG_MUTATION_LOCKS: OnceLock<StdMutex<HashMap<String, Weak<CatalogMutationLock>>>> =
    OnceLock::new();

enum MutationScopeGuard {
    Read {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Write {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

async fn create_metadata(
    store: &dyn ObjectStore,
    location: &str,
    metadata: TabularMetadataRef<'_>,
) -> Result<(), IcebergError> {
    store
        .put_opts(
            &strip_prefix(location).into(),
            serde_json::to_vec(&metadata)?.into(),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| match error {
            object_store::Error::AlreadyExists { .. } => {
                IcebergError::CommitConflict(location.to_owned())
            }
            error => IcebergError::from(error),
        })?;
    Ok(())
}

async fn create_table_metadata(
    store: &dyn ObjectStore,
    metadata_location: &str,
    metadata: &TableMetadata,
) -> Result<(), IcebergError> {
    create_metadata(store, metadata_location, metadata.as_ref()).await?;
    if let Err(error) = store
        .put(
            &table_identity_path(&metadata.location),
            metadata.table_uuid.as_bytes().to_vec().into(),
        )
        .await
    {
        let _ = store.delete(&strip_prefix(metadata_location).into()).await;
        return Err(error.into());
    }
    Ok(())
}

fn table_identity_path(location: &str) -> ObjectPath {
    ObjectPath::from(strip_prefix(&format!("{location}/metadata/table-uuid")))
}

pub mod error;

impl FileCatalog {
    fn catalog_mutation_lock(&self) -> Arc<CatalogMutationLock> {
        let registry = CATALOG_MUTATION_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
        let mut locks = registry.lock().unwrap();
        if locks.len() > 4096 {
            locks.retain(|_, lock| lock.strong_count() > 0);
        }
        let key = self.path.trim_end_matches('/').to_owned();
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(CatalogMutationLock::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn mutation_scope(&self) -> MutationScopeGuard {
        let lock = self.catalog_mutation_lock();
        if matches!(self.object_store, ObjectStoreBuilder::Memory(_)) {
            MutationScopeGuard::Write {
                _guard: lock.write_owned().await,
            }
        } else {
            MutationScopeGuard::Read {
                _guard: lock.read_owned().await,
            }
        }
    }

    fn mutation_lock(&self, identifier: &Identifier) -> Arc<MutationLock> {
        let registry = TABLE_MUTATION_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
        let mut locks = registry.lock().unwrap();
        if locks.len() > 4096 {
            locks.retain(|_, lock| lock.strong_count() > 0);
        }
        let key = (
            self.path.trim_end_matches('/').to_owned(),
            identifier.clone(),
        );
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(MutationLock::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub async fn new(path: &str, object_store: ObjectStoreBuilder) -> Result<Self, Error> {
        Ok(FileCatalog {
            path: path.to_owned(),
            object_store,
            cache: Arc::new(RwLock::new(HashMap::new())),
            renamed_tables: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            update_before_publish_pause: StdMutex::new(None),
            #[cfg(test)]
            create_after_write_pause: StdMutex::new(None),
            #[cfg(test)]
            drop_lock_reached: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn catalog_list(&self) -> Arc<FileCatalogList> {
        Arc::new(FileCatalogList {
            path: self.path.clone(),
            object_store: self.object_store.clone(),
        })
    }
}

#[async_trait]
impl Catalog for FileCatalog {
    /// Catalog name
    fn name(&self) -> &str {
        self.path.trim_end_matches('/').split("/").last().unwrap()
    }
    /// Create a namespace in the catalog
    async fn create_namespace(
        &self,
        namespace: &Namespace,
        _properties: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, String>, IcebergError> {
        if let ObjectStoreBuilder::Filesystem(store) = &self.object_store {
            if let Some(name) = durable_local_namespace_name(namespace) {
                let directory = store
                    .path_to_filesystem(&self.catalog_root_path().join(name))
                    .map_err(IcebergError::from)?;
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(directory))
                    .await
                    .map_err(|error| IcebergError::External(Box::new(error)))?
                    .map_err(|error| IcebergError::External(Box::new(error)))?;
            }
        }
        Ok(HashMap::new())
    }
    /// Drop a namespace in the catalog
    async fn drop_namespace(&self, namespace: &Namespace) -> Result<(), IcebergError> {
        let catalog_lock = self.catalog_mutation_lock();
        let _catalog_guard = catalog_lock.write().await;
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let local_durable = matches!(self.object_store, ObjectStoreBuilder::Filesystem(_))
            && durable_local_namespace_name(namespace).is_some();
        let prefix = if local_durable {
            self.catalog_root_path().join(namespace[0].as_str())
        } else {
            ObjectPath::from(strip_prefix(&self.namespace_path(&namespace[0])))
        };
        let paths: Vec<_> = object_store
            .list(Some(&prefix))
            .map_ok(|x| x.location)
            .try_collect()
            .await
            .map_err(IcebergError::from)?;

        for path in paths {
            object_store
                .delete(&path)
                .await
                .map_err(IcebergError::from)?;
        }

        if local_durable {
            if let ObjectStoreBuilder::Filesystem(store) = &self.object_store {
                let directory = store
                    .path_to_filesystem(&prefix)
                    .map_err(IcebergError::from)?;
                tokio::task::spawn_blocking(move || remove_empty_directory_tree(&directory))
                    .await
                    .map_err(|error| IcebergError::External(Box::new(error)))?
                    .map_err(|error| IcebergError::External(Box::new(error)))?;
            }
        }

        let target = namespace[0].clone();
        self.cache
            .write()
            .unwrap()
            .retain(|id, _| id.namespace().first() != Some(&target));

        Ok(())
    }
    /// Load the namespace properties from the catalog
    async fn load_namespace(
        &self,
        namespace: &Namespace,
    ) -> Result<HashMap<String, String>, IcebergError> {
        if self.namespace_exists(namespace).await? {
            Ok(HashMap::new())
        } else {
            Err(IcebergError::CatalogNotFound)
        }
    }
    /// Update the namespace properties in the catalog. The filesystem catalog
    /// has no place to persist namespace properties, so this is a no-op.
    async fn update_namespace(
        &self,
        _namespace: &Namespace,
        _updates: Option<HashMap<String, String>>,
        _removals: Option<Vec<String>>,
    ) -> Result<(), IcebergError> {
        Ok(())
    }
    /// Check if a namespace exists, including an empty local filesystem directory.
    async fn namespace_exists(&self, namespace: &Namespace) -> Result<bool, IcebergError> {
        let target = &namespace[0];
        Ok(self
            .list_namespaces(None)
            .await?
            .iter()
            .any(|n| n.first() == Some(target)))
    }
    async fn list_tabulars(&self, namespace: &Namespace) -> Result<Vec<Identifier>, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let mut identifiers: HashSet<Identifier> = object_store
            .list(Some(
                &strip_prefix(&self.namespace_path(&namespace[0])).into(),
            ))
            .map_err(IcebergError::from)
            .map_ok(|x| {
                let path = x.location.as_ref();
                self.identifier(path)
            })
            .try_collect::<HashSet<_>>()
            .await?;

        let renamed_tables = self.renamed_tables.read().unwrap();
        for (destination, source) in renamed_tables.iter() {
            if destination != source {
                identifiers.remove(source);
            }
            if destination.namespace() == namespace {
                identifiers.insert(destination.clone());
            }
        }
        drop(renamed_tables);

        identifiers.extend(
            self.cache
                .read()
                .unwrap()
                .keys()
                .filter(|identifier| identifier.namespace() == namespace)
                .cloned(),
        );

        let mut identifiers = identifiers.into_iter().collect::<Vec<_>>();
        identifiers.sort_by_key(ToString::to_string);
        Ok(identifiers)
    }
    async fn list_namespaces(&self, _parent: Option<&str>) -> Result<Vec<Namespace>, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;
        object_store
            .list_with_delimiter(Some(
                &strip_prefix(self.path.trim_start_matches('/')).into(),
            ))
            .await
            .map_err(IcebergError::from)?
            .common_prefixes
            .into_iter()
            .map(|x| self.namespace(x.as_ref()))
            .collect::<Result<_, IcebergError>>()
    }
    async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, IcebergError> {
        match self.metadata_location(identifier).await {
            Ok(_) => Ok(true),
            Err(
                IcebergError::CatalogNotFound
                | IcebergError::NotFound(_)
                | IcebergError::ObjectStore(object_store::Error::NotFound { .. }),
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn drop_table(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.drop_tabular(identifier).await
    }
    async fn rename_table(
        &self,
        source: &Identifier,
        destination: &Identifier,
    ) -> Result<(), IcebergError> {
        if !matches!(self.object_store, ObjectStoreBuilder::Memory(_)) {
            return Err(IcebergError::NotSupported(
                "rename table in persistent file catalogs".to_string(),
            ));
        }
        if source == destination {
            return Ok(());
        }
        let _catalog_guard = self.mutation_scope().await;
        if self.tabular_exists(destination).await? {
            return Err(IcebergError::InvalidFormat(format!(
                "Destination table {destination} already exists"
            )));
        }

        let metadata_location = self.metadata_location(source).await?;
        let cached_metadata = {
            let cache = self.cache.read().unwrap();
            cache.get(source).map(|(_, metadata)| metadata.clone())
        };
        let metadata = if let Some(metadata) = cached_metadata {
            metadata
        } else {
            let bucket = Bucket::from_path(&self.path)?;
            let object_store = self.object_store.build(bucket)?;
            object_store
                .get_metadata(&metadata_location)
                .await
                .map_err(|error| match error {
                    IcebergError::ObjectStore(object_store::Error::NotFound { .. }) => {
                        IcebergError::CatalogNotFound
                    }
                    other => other,
                })?
        };
        if !matches!(metadata, TabularMetadata::Table(_)) {
            return Err(IcebergError::InvalidFormat(format!(
                "Source {source} is not a table"
            )));
        }

        let mut cache = self.cache.write().unwrap();
        cache.remove(source);
        cache.insert(destination.clone(), (metadata_location, metadata));
        drop(cache);

        let mut renamed_tables = self.renamed_tables.write().unwrap();
        let physical_source = renamed_tables
            .remove(source)
            .unwrap_or_else(|| source.clone());
        renamed_tables.insert(destination.clone(), physical_source);
        Ok(())
    }
    async fn drop_view(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.drop_tabular(identifier).await
    }
    async fn drop_materialized_view(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        let storage_table = match self.cache.read().unwrap().get(identifier).cloned() {
            Some((_, TabularMetadata::MaterializedView(metadata))) => {
                Some(metadata.current_version(None)?.storage_table().clone())
            }
            _ => None,
        };
        if let Some(storage_table) = storage_table {
            self.drop_tabular(&storage_table).await?;
        }
        self.drop_tabular(identifier).await
    }
    async fn load_tabular(
        self: Arc<Self>,
        identifier: &Identifier,
    ) -> Result<Tabular, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let metadata_location = self.metadata_location(identifier).await?;

        let metadata = object_store
            .get_metadata(&metadata_location)
            .await
            .map_err(|error| match error {
                IcebergError::ObjectStore(object_store::Error::NotFound { .. }) => {
                    IcebergError::CatalogNotFound
                }
                other => other,
            })?;

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone()),
        );

        match metadata {
            TabularMetadata::Table(metadata) => Ok(Tabular::Table(
                Table::new(
                    identifier.clone(),
                    self.clone(),
                    object_store.clone(),
                    metadata,
                )
                .await?,
            )),
            TabularMetadata::View(metadata) => Ok(Tabular::View(
                View::new(identifier.clone(), self.clone(), metadata).await?,
            )),
            TabularMetadata::MaterializedView(metadata) => Ok(Tabular::MaterializedView(
                MaterializedView::new(identifier.clone(), self.clone(), metadata).await?,
            )),
        }
    }

    async fn create_table(
        self: Arc<Self>,
        identifier: Identifier,
        mut create_table: CreateTable,
    ) -> Result<Table, IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(&identifier);
        let _mutation_guard = mutation_lock.lock().await;
        if self.tabular_exists(&identifier).await? {
            return Err(IcebergError::InvalidFormat(
                "Table already exists. Path".to_owned(),
            ));
        }
        create_table.location =
            Some(self.tabular_path(&identifier.namespace()[0], identifier.name()));
        let metadata: TableMetadata = create_table.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = format!("{location}/metadata/v0.metadata.json");

        create_table_metadata(object_store.as_ref(), &metadata_location, &metadata).await?;

        #[cfg(test)]
        let pause = { self.create_after_write_pause.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((reached, release)) = pause {
            let _ = reached.send(());
            let _ = release.await;
        }

        object_store.put_version_hint(&metadata_location).await.ok();

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );
        Ok(Table::new(
            identifier.clone(),
            self.clone(),
            object_store.clone(),
            metadata,
        )
        .await?)
    }

    async fn create_view(
        self: Arc<Self>,
        identifier: Identifier,
        mut create_view: CreateView<Option<()>>,
    ) -> Result<View, IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(&identifier);
        let _mutation_guard = mutation_lock.lock().await;
        if self.tabular_exists(&identifier).await? {
            return Err(IcebergError::InvalidFormat(
                "View already exists. Path".to_owned(),
            ));
        }

        create_view.location =
            Some(self.tabular_path(&identifier.namespace()[0], identifier.name()));
        let metadata: ViewMetadata = create_view.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = location + "/metadata/v0.metadata.json";

        create_metadata(object_store.as_ref(), &metadata_location, metadata.as_ref()).await?;

        object_store.put_version_hint(&metadata_location).await.ok();

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );
        Ok(View::new(identifier.clone(), self.clone(), metadata).await?)
    }

    async fn create_materialized_view(
        self: Arc<Self>,
        identifier: Identifier,
        create_view: CreateMaterializedView,
    ) -> Result<MaterializedView, IcebergError> {
        let catalog_lock = self.catalog_mutation_lock();
        let _catalog_guard = catalog_lock.write().await;
        let mutation_lock = self.mutation_lock(&identifier);
        let _mutation_guard = mutation_lock.lock().await;
        if self.tabular_exists(&identifier).await? {
            return Err(IcebergError::InvalidFormat(
                "View already exists. Path".to_owned(),
            ));
        }

        let (mut create_view, mut create_table) = create_view.into();

        create_view.location =
            Some(self.tabular_path(&identifier.namespace()[0], identifier.name()));
        let metadata: MaterializedViewMetadata = create_view.try_into()?;
        let table_identifier = metadata.current_version(None)?.storage_table();

        create_table.location =
            Some(self.tabular_path(&table_identifier.namespace()[0], table_identifier.name()));
        let table_metadata: TableMetadata = create_table.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = location + "/metadata/v0.metadata.json";

        let table_metadata_location =
            table_metadata.location.clone() + "/metadata/v0.metadata.json";

        create_table_metadata(
            object_store.as_ref(),
            &table_metadata_location,
            &table_metadata,
        )
        .await?;
        if let Err(error) =
            create_metadata(object_store.as_ref(), &metadata_location, metadata.as_ref()).await
        {
            let _ = object_store
                .delete(&table_identity_path(&table_metadata.location))
                .await;
            let _ = object_store
                .delete(&strip_prefix(&table_metadata_location).into())
                .await;
            return Err(error);
        }
        object_store
            .put_version_hint(&table_metadata_location)
            .await
            .ok();
        object_store.put_version_hint(&metadata_location).await.ok();

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );
        Ok(MaterializedView::new(identifier.clone(), self.clone(), metadata).await?)
    }

    async fn update_table(self: Arc<Self>, commit: CommitTable) -> Result<Table, IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(&commit.identifier);
        let _mutation_guard = mutation_lock.lock().await;
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(IcebergError::CommitConflict(identifier.to_string()));
        };
        let (previous_metadata_location, metadata) = entry;

        let TabularMetadata::Table(mut metadata) = metadata else {
            return Err(IcebergError::InvalidFormat(
                "Table update on entity that is not a table".to_owned(),
            ));
        };
        // The marker fences a dropped/recreated table without re-reading a
        // potentially large metadata JSON on every local commit. Legacy
        // tables without the marker still verify their metadata UUID.
        let identity = match object_store
            .get(&table_identity_path(&metadata.location))
            .await
        {
            Ok(identity) => Some(identity.bytes().await?),
            Err(object_store::Error::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let same_table = if let Some(identity) = identity {
            identity.as_ref() == metadata.table_uuid.as_bytes()
        } else {
            match object_store.get_metadata(&previous_metadata_location).await {
                Ok(TabularMetadata::Table(current)) => current.table_uuid == metadata.table_uuid,
                Err(
                    IcebergError::ObjectStore(object_store::Error::NotFound { .. })
                    | IcebergError::NotFound(_)
                    | IcebergError::CatalogNotFound,
                ) => false,
                Err(error) => return Err(error),
                Ok(_) => false,
            }
        };
        if !same_table {
            return Err(IcebergError::CommitConflict(identifier.to_string()));
        }
        if !check_table_requirements(&commit.requirements, &metadata) {
            return Err(IcebergError::CommitConflict(identifier.to_string()));
        }
        #[cfg(test)]
        let pause = { self.update_before_publish_pause.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((reached, release)) = pause {
            let _ = reached.send(());
            let _ = release.await;
        }
        apply_table_updates(&mut metadata, commit.updates)?;
        let temp_metadata_location = new_metadata_location(&metadata);

        object_store
            .put_metadata(&temp_metadata_location, metadata.as_ref())
            .await?;

        let metadata_location = new_filesystem_metadata_location(
            &metadata.location,
            &previous_metadata_location,
            temp_metadata_location.ends_with(".gz.metadata.json"),
        )?;

        object_store
            .copy_if_not_exists(
                &strip_prefix(&temp_metadata_location).into(),
                &strip_prefix(&metadata_location).into(),
            )
            .await
            .map_err(|error| match error {
                object_store::Error::AlreadyExists { .. } => {
                    IcebergError::CommitConflict(identifier.to_string())
                }
                error => error.into(),
            })?;

        object_store.put_version_hint(&metadata_location).await.ok();

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );

        Ok(Table::new(
            identifier.clone(),
            self.clone(),
            object_store.clone(),
            metadata,
        )
        .await?)
    }

    async fn update_view(
        self: Arc<Self>,
        commit: CommitView<Option<()>>,
    ) -> Result<View, IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(&commit.identifier);
        let _mutation_guard = mutation_lock.lock().await;
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(IcebergError::InvalidFormat(
                "Create table assertion".to_owned(),
            ));
        };
        let (previous_metadata_location, mut metadata) = entry;
        let metadata_location = match &mut metadata {
            TabularMetadata::View(metadata) => {
                if !check_view_requirements(&commit.requirements, metadata) {
                    return Err(IcebergError::InvalidFormat(
                        "View requirements not valid".to_owned(),
                    ));
                }
                apply_view_updates(metadata, commit.updates)?;
                let temp_metadata_location = new_metadata_location(&*metadata);

                object_store
                    .put_metadata(&temp_metadata_location, metadata.as_ref())
                    .await?;

                let metadata_location = new_filesystem_metadata_location(
                    &metadata.location,
                    &previous_metadata_location,
                    temp_metadata_location.ends_with(".gz.metadata.json"),
                )?;

                object_store
                    .copy_if_not_exists(
                        &strip_prefix(&temp_metadata_location).into(),
                        &strip_prefix(&metadata_location).into(),
                    )
                    .await?;

                object_store.put_version_hint(&metadata_location).await.ok();

                Ok(metadata_location)
            }
            _ => Err(IcebergError::InvalidFormat(
                "View update on entity that is not a view".to_owned(),
            )),
        }?;

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone()),
        );
        if let TabularMetadata::View(metadata) = metadata {
            Ok(View::new(identifier.clone(), self.clone(), metadata).await?)
        } else {
            Err(IcebergError::InvalidFormat(
                "Entity is not a view".to_owned(),
            ))
        }
    }
    async fn update_materialized_view(
        self: Arc<Self>,
        commit: CommitView<Identifier>,
    ) -> Result<MaterializedView, IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(&commit.identifier);
        let _mutation_guard = mutation_lock.lock().await;
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(IcebergError::InvalidFormat(
                "Create table assertion".to_owned(),
            ));
        };
        let (previous_metadata_location, mut metadata) = entry;
        let metadata_location = match &mut metadata {
            TabularMetadata::MaterializedView(metadata) => {
                if !check_view_requirements(&commit.requirements, metadata) {
                    return Err(IcebergError::InvalidFormat(
                        "Materialized view requirements not valid".to_owned(),
                    ));
                }
                apply_view_updates(metadata, commit.updates)?;
                let temp_metadata_location = new_metadata_location(&*metadata);

                object_store
                    .put_metadata(&temp_metadata_location, metadata.as_ref())
                    .await?;

                let metadata_location = new_filesystem_metadata_location(
                    &metadata.location,
                    &previous_metadata_location,
                    temp_metadata_location.ends_with(".gz.metadata.json"),
                )?;

                object_store
                    .copy_if_not_exists(
                        &strip_prefix(&temp_metadata_location).into(),
                        &strip_prefix(&metadata_location).into(),
                    )
                    .await?;

                object_store.put_version_hint(&metadata_location).await.ok();

                Ok(metadata_location)
            }
            _ => Err(IcebergError::InvalidFormat(
                "Materialized view update on entity that is not a materialized view".to_owned(),
            )),
        }?;

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone()),
        );
        if let TabularMetadata::MaterializedView(metadata) = metadata {
            Ok(MaterializedView::new(identifier.clone(), self.clone(), metadata).await?)
        } else {
            Err(IcebergError::InvalidFormat(
                "Entity is not a materialized view".to_owned(),
            ))
        }
    }

    async fn register_table(
        self: Arc<Self>,
        _identifier: Identifier,
        _metadata_location: &str,
    ) -> Result<Table, IcebergError> {
        unimplemented!()
    }
}

impl FileCatalog {
    fn default_object_store(&self, bucket: Bucket) -> Arc<dyn object_store::ObjectStore> {
        Arc::new(self.object_store.build(bucket).unwrap())
    }
    fn namespace_path(&self, namespace: &str) -> String {
        self.path.as_str().trim_end_matches('/').to_owned() + "/" + namespace
    }

    fn catalog_root_path(&self) -> ObjectPath {
        ObjectPath::from(strip_prefix(&self.path))
    }

    fn tabular_path(&self, namespace: &str, name: &str) -> String {
        self.path.as_str().trim_end_matches('/').to_owned() + "/" + namespace + "/" + name
    }

    async fn drop_tabular(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        let _catalog_guard = self.mutation_scope().await;
        let mutation_lock = self.mutation_lock(identifier);
        let _mutation_guard = mutation_lock.lock().await;
        #[cfg(test)]
        self.drop_lock_reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let physical_identifier = self
            .renamed_tables
            .read()
            .unwrap()
            .get(identifier)
            .cloned()
            .unwrap_or_else(|| identifier.clone());
        let tabular_path = self.tabular_path(
            &physical_identifier.namespace()[0],
            physical_identifier.name(),
        );
        let prefix = strip_prefix(&tabular_path);
        let paths: Vec<_> = object_store
            .list(Some(&prefix.as_str().into()))
            .map_ok(|x| x.location)
            .try_collect()
            .await
            .map_err(IcebergError::from)?;

        let identity_path = table_identity_path(&tabular_path);
        if paths.contains(&identity_path) {
            object_store
                .delete(&identity_path)
                .await
                .map_err(IcebergError::from)?;
        }

        for path in paths {
            if path == identity_path {
                continue;
            }
            object_store
                .delete(&path)
                .await
                .map_err(IcebergError::from)?;
        }

        self.cache.write().unwrap().remove(identifier);
        self.renamed_tables.write().unwrap().remove(identifier);
        Ok(())
    }

    async fn metadata_location(&self, identifier: &Identifier) -> Result<String, IcebergError> {
        let physical_identifier = {
            let renamed_tables = self.renamed_tables.read().unwrap();
            if renamed_tables.values().any(|source| source == identifier)
                && !renamed_tables.contains_key(identifier)
            {
                return Err(IcebergError::CatalogNotFound);
            }
            renamed_tables
                .get(identifier)
                .cloned()
                .unwrap_or_else(|| identifier.clone())
        };

        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let path = self.tabular_path(
            &physical_identifier.namespace()[0],
            physical_identifier.name(),
        ) + "/metadata";
        let metadata_dir = ObjectPath::from(strip_prefix(&path));
        let latest = object_store
            .list(Some(&metadata_dir))
            .try_fold(None, |latest: Option<(u64, String)>, object| {
                let direct_child = object
                    .location
                    .parent()
                    .as_ref()
                    .is_some_and(|parent| parent == &metadata_dir);
                let path = object.location.to_string();
                let version = direct_child.then(|| parse_version(&path).ok()).flatten();
                async move {
                    Ok::<_, object_store::Error>(match version {
                        Some(version)
                            if latest
                                .as_ref()
                                .is_none_or(|(current, _)| version >= *current) =>
                        {
                            Some((version, path))
                        }
                        _ => latest,
                    })
                }
            })
            .await
            .map_err(IcebergError::from)?;
        latest
            .map(|(_, path)| path)
            .ok_or(IcebergError::CatalogNotFound)
    }

    fn identifier(&self, path: &str) -> Identifier {
        let relative = self.relative_to_root(path);
        let parts: Vec<&str> = relative.split('/').take(2).collect();
        let namespace = decode_path_component(parts[0]);
        let name = decode_path_component(parts[1]);
        Identifier::new(&[namespace], &name)
    }

    fn namespace(&self, path: &str) -> Result<Namespace, IcebergError> {
        let relative = self.relative_to_root(path);
        let part = relative
            .split('/')
            .next()
            .ok_or(IcebergError::InvalidFormat("Namespace in path".to_owned()))?;
        Namespace::try_new(&[decode_path_component(part)]).map_err(IcebergError::from)
    }

    // Project `path` (a value coming back from `ObjectStore::list*`, which has
    // the scheme + authority + leading slash already stripped) into the catalog
    // root, returning the suffix as a `<namespace>/<name>/...` slice. Both
    // `path` and `self.path` are routed through `strip_prefix` so a `file://`,
    // `s3://`, `abfs://` etc. catalog root reduces to the same shape as the
    // raw object_store path before the prefix subtraction.
    fn relative_to_root(&self, path: &str) -> String {
        let path = strip_prefix(path);
        let path = path.trim_start_matches('/');
        let root = ObjectPath::from(strip_prefix(&self.path));
        let root = root.as_ref();
        path.strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/')
            .to_owned()
    }
}

fn remove_empty_directory_tree(directory: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Namespace path is not a directory",
        ));
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_directory_tree(&entry.path())?;
        }
    }
    match std::fs::remove_dir(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn durable_local_namespace_name(namespace: &Namespace) -> Option<&str> {
    let [name] = &namespace[..] else {
        return None;
    };
    let mut characters = name.chars();
    let first = characters.next()?;
    if (first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        Some(name)
    } else {
        None
    }
}

fn decode_path_component(component: &str) -> String {
    percent_decode_str(component)
        .decode_utf8()
        .map_or_else(|_| component.to_owned(), |decoded| decoded.into_owned())
}

fn parse_version(path: &str) -> Result<u64, IcebergError> {
    let filename = path
        .split('/')
        .next_back()
        .ok_or(IcebergError::InvalidFormat("Metadata location".to_owned()))?;
    let stem = filename
        .strip_suffix(".metadata.json")
        .ok_or_else(|| IcebergError::InvalidFormat(format!("Metadata version in {path}")))?;
    let stem = stem.strip_suffix(".gz").unwrap_or(stem);
    stem.strip_prefix('v')
        .ok_or_else(|| IcebergError::InvalidFormat(format!("Metadata version in {path}")))?
        .parse()
        .map_err(IcebergError::from)
}

fn new_filesystem_metadata_location(
    metadata_location: &str,
    previous_metadata_location: &str,
    gzipped: bool,
) -> Result<String, IcebergError> {
    let current_version = parse_version(previous_metadata_location)? + 1;
    // The name carries the encoding: the reader decides whether to decompress
    // from the suffix alone, so a gzipped file must be named `.gz.metadata.json`
    // even though this catalog numbers versions rather than using the metastore
    // name. Kept in sync with the temp file actually written, not re-derived
    // from properties, so the two can never disagree.
    let suffix = if gzipped {
        "gz.metadata.json"
    } else {
        "metadata.json"
    };
    Ok(format!(
        "{}/metadata/v{}.{}",
        metadata_location, current_version, suffix
    ))
}

#[cfg(test)]
mod metadata_naming_tests {
    use super::*;

    /// The version number must be recovered from both the plain and the
    /// gzipped name, since a gzipped table's previous location carries the
    /// `.gz.metadata.json` suffix and drives the next version number.
    #[test]
    fn parse_version_reads_plain_and_gzipped_names() {
        assert_eq!(
            parse_version("/wh/ns/t/metadata/v7.metadata.json").unwrap(),
            7
        );
        assert_eq!(
            parse_version("/wh/ns/t/metadata/v7.gz.metadata.json").unwrap(),
            7
        );
        for filename in [
            "v7metadata.json",
            "vv7.metadata.json",
            "v7.other.metadata.json",
        ] {
            assert!(parse_version(filename).is_err(), "accepted {filename}");
        }
    }

    /// The final versioned name must carry the same encoding as the file that
    /// was actually written; otherwise the reader, which decides purely from
    /// the suffix, would read gzip bytes as plain JSON.
    #[test]
    fn the_versioned_name_carries_the_gzip_suffix() {
        let plain = new_filesystem_metadata_location(
            "/wh/ns/t",
            "/wh/ns/t/metadata/v3.metadata.json",
            false,
        )
        .unwrap();
        assert_eq!(plain, "/wh/ns/t/metadata/v4.metadata.json");

        let gzipped = new_filesystem_metadata_location(
            "/wh/ns/t",
            "/wh/ns/t/metadata/v3.gz.metadata.json",
            true,
        )
        .unwrap();
        assert_eq!(gzipped, "/wh/ns/t/metadata/v4.gz.metadata.json");
    }

    /// The encoding may be toggled on an existing table, so the previous name's
    /// suffix and the requested encoding are independent: whichever the caller
    /// asks for wins, and the version still advances.
    #[test]
    fn the_encoding_can_change_between_commits() {
        // Was plain, now gzipped.
        assert_eq!(
            new_filesystem_metadata_location(
                "/wh/ns/t",
                "/wh/ns/t/metadata/v1.metadata.json",
                true
            )
            .unwrap(),
            "/wh/ns/t/metadata/v2.gz.metadata.json"
        );
        // Was gzipped, now plain.
        assert_eq!(
            new_filesystem_metadata_location(
                "/wh/ns/t",
                "/wh/ns/t/metadata/v1.gz.metadata.json",
                false
            )
            .unwrap(),
            "/wh/ns/t/metadata/v2.metadata.json"
        );
    }
}

#[derive(Debug)]
pub struct FileCatalogList {
    path: String,
    object_store: ObjectStoreBuilder,
}

impl FileCatalogList {
    pub async fn new(path: &str, object_store: ObjectStoreBuilder) -> Result<Self, Error> {
        Ok(FileCatalogList {
            path: path.to_owned(),
            object_store,
        })
    }

    fn parse_catalog(&self, path: &str) -> Result<String, IcebergError> {
        let path = strip_prefix(path);
        let path = path.trim_start_matches('/');
        let root = strip_prefix(&self.path);
        let root = root.trim_start_matches('/');
        path.strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/')
            .split('/')
            .next()
            .ok_or(IcebergError::InvalidFormat("Catalog in path".to_owned()))
            .map(ToOwned::to_owned)
    }
}

#[async_trait]
impl CatalogList for FileCatalogList {
    fn catalog(&self, name: &str) -> Option<Arc<dyn Catalog>> {
        Some(Arc::new(FileCatalog {
            path: self.path.clone() + "/" + name,
            object_store: self.object_store.clone(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            renamed_tables: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            update_before_publish_pause: StdMutex::new(None),
            #[cfg(test)]
            create_after_write_pause: StdMutex::new(None),
            #[cfg(test)]
            drop_lock_reached: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    async fn list_catalogs(&self) -> Vec<String> {
        let bucket = Bucket::from_path(&self.path).unwrap();
        let object_store = self.object_store.build(bucket).unwrap();

        object_store
            .list_with_delimiter(Some(&strip_prefix(&self.path).into()))
            .await
            .map_err(IcebergError::from)
            .unwrap()
            .common_prefixes
            .into_iter()
            .map(|x| self.parse_catalog(x.as_ref()))
            .collect::<Result<_, IcebergError>>()
            .unwrap()
    }
}

#[cfg(test)]
pub mod tests {
    use datafusion::{
        arrow::array::{Float64Array, Int64Array},
        common::{
            config::ConfigNonZeroUsize,
            tree_node::{TransformedResult, TreeNode},
        },
        execution::SessionStateBuilder,
        prelude::{SessionConfig, SessionContext},
    };
    use datafusion_iceberg::{
        catalog::catalog::IcebergCatalog,
        planner::{iceberg_transform, IcebergQueryPlanner},
    };
    use futures::StreamExt;
    use iceberg_rust::{
        catalog::{
            commit::{CommitTable, TableRequirement, TableUpdate},
            create::CreateTableBuilder,
            namespace::Namespace,
            tabular::Tabular,
            Catalog,
        },
        error::Error as IcebergError,
        materialized_view::MaterializedView,
        object_store::{Bucket, ObjectStoreBuilder},
        spec::{
            schema::Schema,
            types::{PrimitiveType, StructField, Type},
            util::strip_prefix,
            view_metadata::{Version, ViewRepresentation},
        },
    };
    use object_store::{path::Path as ObjectPath, ObjectStoreExt};
    use std::{collections::HashMap, sync::Arc, time::Duration};
    use testcontainers::{core::ExecCommand, runners::AsyncRunner, ImageExt};
    use testcontainers_modules::localstack::LocalStack;
    use tokio::time::sleep;
    // use testcontainers::{core::ExecCommand, runners::AsyncRunner, ImageExt};
    // use testcontainers_modules::localstack::LocalStack;

    use crate::FileCatalog;

    fn simple_table_schema() -> Schema {
        Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn local_empty_directory_is_listed_as_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let path = format!("file://{}/catalog", temp.path().display());
        std::fs::create_dir_all(temp.path().join("catalog/APP")).unwrap();
        let catalog = FileCatalog::new(&path, ObjectStoreBuilder::filesystem("/"))
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["APP".to_owned()]).unwrap();
        assert!(catalog
            .list_namespaces(None)
            .await
            .unwrap()
            .contains(&namespace));
    }

    #[tokio::test]
    async fn empty_namespace_drop_removes_local_filesystem_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = format!("file://{}/catalog", temp.path().display());
        let store = ObjectStoreBuilder::filesystem("/");
        let catalog = FileCatalog::new(&path, store.clone()).await.unwrap();
        let namespace = Namespace::try_new(&["EMPTY".to_owned()]).unwrap();

        catalog.create_namespace(&namespace, None).await.unwrap();
        catalog.create_namespace(&namespace, None).await.unwrap();
        let reopened = Arc::new(FileCatalog::new(&path, store.clone()).await.unwrap());
        assert!(reopened.namespace_exists(&namespace).await.unwrap());
        assert!(reopened.list_tabulars(&namespace).await.unwrap().is_empty());

        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let mut builder = CreateTableBuilder::default();
        builder
            .with_name("ITEMS")
            .with_schema(schema)
            .build(&namespace, reopened.clone())
            .await
            .unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "ITEMS");
        reopened.drop_table(&identifier).await.unwrap();

        reopened.drop_namespace(&namespace).await.unwrap();
        let reopened = FileCatalog::new(&path, store).await.unwrap();
        assert!(!reopened.namespace_exists(&namespace).await.unwrap());
        assert!(!temp.path().join("catalog/EMPTY").exists());
    }

    #[tokio::test]
    async fn fresh_catalog_loads_encoded_metadata_paths() {
        let catalog_path = "/dev/embucket_日";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(catalog_path, store.clone()).await.unwrap());
        let namespace = Namespace::try_new(&["public_日".to_string()]).unwrap();
        for name in ["unicode_日😀", "percent_50%"] {
            let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, name);
            let schema = Schema::builder()
                .with_struct_field(StructField {
                    id: 1,
                    name: "id".to_string(),
                    required: true,
                    field_type: Type::Primitive(PrimitiveType::Long),
                    doc: None,
                    initial_default: None,
                    write_default: None,
                })
                .build()
                .unwrap();
            let mut builder = CreateTableBuilder::default();
            builder
                .with_name(identifier.name())
                .with_schema(schema)
                .build(&namespace, catalog.clone())
                .await
                .unwrap();

            let bucket = Bucket::from_path(catalog_path).unwrap();
            let object_store = store.build(bucket).unwrap();
            for suffix in ["vbroken.metadata.json", "v_archive/v99.metadata.json"] {
                let location = format!(
                    "{}/metadata/{suffix}",
                    catalog.tabular_path(&namespace[0], identifier.name())
                );
                object_store
                    .put(
                        &ObjectPath::from(strip_prefix(&location)),
                        b"malformed".to_vec().into(),
                    )
                    .await
                    .unwrap();
            }

            let fresh = Arc::new(FileCatalog::new(catalog_path, store.clone()).await.unwrap());
            assert!(fresh
                .list_namespaces(None)
                .await
                .unwrap()
                .contains(&namespace));
            let listed = fresh.list_tabulars(&namespace).await.unwrap();
            assert!(listed.contains(&identifier), "missing {name} in {listed:?}");
            let listed_identifier = listed.into_iter().find(|id| id == &identifier).unwrap();
            let Tabular::Table(table) = fresh.load_tabular(&listed_identifier).await.unwrap()
            else {
                panic!("expected Iceberg table");
            };
            assert_eq!(table.identifier(), &identifier);
        }
    }

    #[tokio::test]
    async fn stale_handle_cannot_commit_after_drop_and_recreate() {
        let store = ObjectStoreBuilder::memory();
        let writer = Arc::new(
            FileCatalog::new("/dev/recreated", store.clone())
                .await
                .unwrap(),
        );
        let stale_catalog = Arc::new(FileCatalog::new("/dev/recreated", store).await.unwrap());
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let mut builder = CreateTableBuilder::default();
        let original = builder
            .with_name(identifier.name())
            .with_schema(schema.clone())
            .build(&namespace, writer.clone())
            .await
            .unwrap();
        let Tabular::Table(mut stale) = stale_catalog
            .clone()
            .load_tabular(&identifier)
            .await
            .unwrap()
        else {
            panic!("expected original table");
        };
        writer.drop_table(&identifier).await.unwrap();
        let missing_result = stale
            .new_transaction(None)
            .update_properties(vec![("stale".to_owned(), "true".to_owned())])
            .commit()
            .await;
        assert!(
            matches!(missing_result, Err(IcebergError::CommitConflict(_))),
            "{missing_result:?}"
        );
        let mut builder = CreateTableBuilder::default();
        let replacement = builder
            .with_name(identifier.name())
            .with_schema(schema)
            .build(&namespace, writer.clone())
            .await
            .unwrap();
        assert_ne!(
            original.metadata().table_uuid,
            replacement.metadata().table_uuid
        );

        let direct_result = stale_catalog
            .clone()
            .update_table(CommitTable {
                identifier: identifier.clone(),
                requirements: vec![],
                updates: vec![TableUpdate::SetProperties {
                    updates: HashMap::from([("stale".to_owned(), "true".to_owned())]),
                }],
            })
            .await;
        assert!(
            matches!(direct_result, Err(IcebergError::CommitConflict(_))),
            "{direct_result:?}"
        );

        let result = stale
            .new_transaction(None)
            .update_properties(vec![("stale".to_owned(), "true".to_owned())])
            .commit()
            .await;
        assert!(
            matches!(result, Err(IcebergError::CommitConflict(_))),
            "{result:?}"
        );
        let Tabular::Table(current) = writer.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("expected replacement table");
        };
        assert_eq!(
            current.metadata().table_uuid,
            replacement.metadata().table_uuid
        );
        assert!(!current.metadata().properties.contains_key("stale"));
    }

    #[tokio::test]
    async fn legacy_table_without_identity_marker_keeps_writes_and_missing_cache_conflicts() {
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(
            FileCatalog::new("/dev/legacy", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let mut builder = CreateTableBuilder::default();
        let mut table = builder
            .with_name(identifier.name())
            .with_schema(simple_table_schema())
            .build(&namespace, catalog.clone())
            .await
            .unwrap();
        let object_store = store
            .build(Bucket::from_path("/dev/legacy").unwrap())
            .unwrap();
        object_store
            .delete(&super::table_identity_path(&table.metadata().location))
            .await
            .unwrap();

        table
            .new_transaction(None)
            .update_properties(vec![("legacy".to_owned(), "ok".to_owned())])
            .commit()
            .await
            .unwrap();
        let Tabular::Table(reloaded) = catalog.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("expected legacy table");
        };
        assert_eq!(
            reloaded
                .metadata()
                .properties
                .get("legacy")
                .map(String::as_str),
            Some("ok")
        );

        catalog.drop_table(&identifier).await.unwrap();
        let result = catalog
            .update_table(CommitTable {
                identifier,
                requirements: vec![],
                updates: vec![],
            })
            .await;
        assert!(
            matches!(result, Err(IcebergError::CommitConflict(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn drop_waits_for_an_inflight_table_update_across_catalog_instances() {
        let store = ObjectStoreBuilder::memory();
        let writer = Arc::new(
            FileCatalog::new("/dev/update_drop", store.clone())
                .await
                .unwrap(),
        );
        let dropper = Arc::new(
            FileCatalog::new("/dev/update_drop/", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let mut builder = CreateTableBuilder::default();
        builder
            .with_name(identifier.name())
            .with_schema(simple_table_schema())
            .build(&namespace, writer.clone())
            .await
            .unwrap();
        let Tabular::Table(mut table) = writer.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("expected table");
        };
        let (reached, reached_rx) = futures::channel::oneshot::channel();
        let (release_tx, release) = futures::channel::oneshot::channel();
        *writer.update_before_publish_pause.lock().unwrap() = Some((reached, release));
        let update = tokio::spawn(async move {
            table
                .new_transaction(None)
                .update_properties(vec![("raced".to_owned(), "true".to_owned())])
                .commit()
                .await
        });
        reached_rx.await.unwrap();

        let mut drop = Box::pin(dropper.drop_table(&identifier));
        assert!(matches!(
            futures::poll!(drop.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(!dropper
            .drop_lock_reached
            .load(std::sync::atomic::Ordering::SeqCst));
        release_tx.send(()).unwrap();
        update.await.unwrap().unwrap();
        drop.await.unwrap();
        assert!(dropper
            .drop_lock_reached
            .load(std::sync::atomic::Ordering::SeqCst));

        let mut builder = CreateTableBuilder::default();
        let replacement = builder
            .with_name(identifier.name())
            .with_schema(simple_table_schema())
            .build(&namespace, dropper)
            .await
            .unwrap();
        let fresh = Arc::new(FileCatalog::new("/dev/update_drop", store).await.unwrap());
        let Tabular::Table(current) = fresh.load_tabular(&identifier).await.unwrap() else {
            panic!("expected replacement table");
        };
        assert_eq!(
            current.metadata().table_uuid,
            replacement.metadata().table_uuid
        );
        assert!(!current.metadata().properties.contains_key("raced"));
    }

    #[tokio::test]
    async fn drop_waits_for_an_inflight_table_create_across_catalog_instances() {
        let store = ObjectStoreBuilder::memory();
        let creator = Arc::new(
            FileCatalog::new("/dev/create_drop", store.clone())
                .await
                .unwrap(),
        );
        let dropper = Arc::new(
            FileCatalog::new("/dev/create_drop", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let (reached, reached_rx) = futures::channel::oneshot::channel();
        let (release_tx, release) = futures::channel::oneshot::channel();
        *creator.create_after_write_pause.lock().unwrap() = Some((reached, release));
        let create = {
            let creator = Arc::clone(&creator);
            let namespace = namespace.clone();
            let identifier = identifier.clone();
            tokio::spawn(async move {
                let mut builder = CreateTableBuilder::default();
                builder
                    .with_name(identifier.name())
                    .with_schema(simple_table_schema())
                    .build(&namespace, creator)
                    .await
            })
        };
        reached_rx.await.unwrap();

        let mut drop = Box::pin(dropper.drop_table(&identifier));
        assert!(matches!(
            futures::poll!(drop.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(!dropper
            .drop_lock_reached
            .load(std::sync::atomic::Ordering::SeqCst));
        release_tx.send(()).unwrap();
        create.await.unwrap().unwrap();
        drop.await.unwrap();
        assert!(dropper
            .drop_lock_reached
            .load(std::sync::atomic::Ordering::SeqCst));

        let fresh = Arc::new(FileCatalog::new("/dev/create_drop", store).await.unwrap());
        assert!(!fresh.tabular_exists(&identifier).await.unwrap());
    }

    #[tokio::test]
    async fn namespace_drop_waits_for_an_inflight_table_update() {
        let store = ObjectStoreBuilder::memory();
        let writer = Arc::new(
            FileCatalog::new("/dev/namespace_drop", store.clone())
                .await
                .unwrap(),
        );
        let dropper = Arc::new(
            FileCatalog::new("/dev/namespace_drop", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let mut builder = CreateTableBuilder::default();
        let mut table = builder
            .with_name(identifier.name())
            .with_schema(simple_table_schema())
            .build(&namespace, writer.clone())
            .await
            .unwrap();
        let (reached, reached_rx) = futures::channel::oneshot::channel();
        let (release_tx, release) = futures::channel::oneshot::channel();
        *writer.update_before_publish_pause.lock().unwrap() = Some((reached, release));
        let update = tokio::spawn(async move {
            table
                .new_transaction(None)
                .update_properties(vec![("raced".to_owned(), "true".to_owned())])
                .commit()
                .await
        });
        reached_rx.await.unwrap();

        let mut drop = Box::pin(dropper.drop_namespace(&namespace));
        assert!(matches!(
            futures::poll!(drop.as_mut()),
            std::task::Poll::Pending
        ));
        release_tx.send(()).unwrap();
        update.await.unwrap().unwrap();
        drop.await.unwrap();

        let fresh = Arc::new(
            FileCatalog::new("/dev/namespace_drop", store)
                .await
                .unwrap(),
        );
        assert!(!fresh.tabular_exists(&identifier).await.unwrap());
    }

    #[tokio::test]
    async fn renamed_table_update_serializes_with_drop_by_original_name() {
        let store = ObjectStoreBuilder::memory();
        let writer = Arc::new(
            FileCatalog::new("/dev/rename_drop", store.clone())
                .await
                .unwrap(),
        );
        let dropper = Arc::new(
            FileCatalog::new("/dev/rename_drop", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let source = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "original");
        let destination = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "renamed");
        let mut builder = CreateTableBuilder::default();
        builder
            .with_name(source.name())
            .with_schema(simple_table_schema())
            .build(&namespace, writer.clone())
            .await
            .unwrap();
        writer.rename_table(&source, &destination).await.unwrap();
        let Tabular::Table(mut table) = writer.clone().load_tabular(&destination).await.unwrap()
        else {
            panic!("expected renamed table");
        };
        let (reached, reached_rx) = futures::channel::oneshot::channel();
        let (release_tx, release) = futures::channel::oneshot::channel();
        *writer.update_before_publish_pause.lock().unwrap() = Some((reached, release));
        let update = tokio::spawn(async move {
            table
                .new_transaction(None)
                .update_properties(vec![("raced".to_owned(), "true".to_owned())])
                .commit()
                .await
        });
        reached_rx.await.unwrap();

        let mut drop = Box::pin(dropper.drop_table(&source));
        assert!(matches!(
            futures::poll!(drop.as_mut()),
            std::task::Poll::Pending
        ));
        release_tx.send(()).unwrap();
        update.await.unwrap().unwrap();
        drop.await.unwrap();

        let fresh = Arc::new(FileCatalog::new("/dev/rename_drop", store).await.unwrap());
        assert!(!fresh.tabular_exists(&source).await.unwrap());
    }

    #[tokio::test]
    async fn materialized_view_storage_collision_does_not_publish_view() {
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(
            FileCatalog::new("/dev/mv_collision", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let view_identifier =
            iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items");
        let storage_identifier =
            iceberg_rust::catalog::identifier::Identifier::new(&namespace, "items__storage");
        let mut builder = CreateTableBuilder::default();
        builder
            .with_name(storage_identifier.name())
            .with_schema(simple_table_schema())
            .build(&namespace, catalog.clone())
            .await
            .unwrap();

        let mut view_builder = MaterializedView::builder();
        let result = view_builder
            .with_name(view_identifier.name())
            .with_schema(simple_table_schema())
            .with_view_version(
                Version::builder()
                    .with_representation(ViewRepresentation::sql("select id from items", None))
                    .build()
                    .unwrap(),
            )
            .build(&namespace, catalog.clone())
            .await;
        assert!(result.is_err(), "storage-table collision must fail");
        assert!(!catalog.tabular_exists(&view_identifier).await.unwrap());
        assert!(catalog.tabular_exists(&storage_identifier).await.unwrap());

        catalog.drop_table(&storage_identifier).await.unwrap();
        let mut view_builder = MaterializedView::builder();
        view_builder
            .with_name(view_identifier.name())
            .with_schema(simple_table_schema())
            .with_view_version(
                Version::builder()
                    .with_representation(ViewRepresentation::sql("select id from items", None))
                    .build()
                    .unwrap(),
            )
            .build(&namespace, catalog.clone())
            .await
            .unwrap();
        assert!(catalog.tabular_exists(&view_identifier).await.unwrap());
        let Tabular::Table(storage_table) = catalog
            .clone()
            .load_tabular(&storage_identifier)
            .await
            .unwrap()
        else {
            panic!("expected storage table");
        };
        let object_store = store
            .build(Bucket::from_path("/dev/mv_collision").unwrap())
            .unwrap();
        let marker = object_store
            .get(&super::table_identity_path(
                &storage_table.metadata().location,
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            marker.as_ref(),
            storage_table.metadata().table_uuid.as_bytes()
        );
    }

    #[tokio::test]
    #[ignore = "run manually when changing the file-catalog commit path"]
    async fn bench_local_file_catalog_metadata_commits() {
        let temp = tempfile::tempdir().unwrap();
        let path = format!("file://{}/catalog", temp.path().display());
        let catalog = Arc::new(
            FileCatalog::new(&path, ObjectStoreBuilder::filesystem("/"))
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let mut builder = CreateTableBuilder::default();
        let mut table = builder
            .with_name("items")
            .with_schema(schema)
            .build(&namespace, catalog)
            .await
            .unwrap();

        let property_count = std::env::var("ICEBERG_BENCH_PROPERTY_COUNT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if property_count > 0 {
            let properties = (0..property_count)
                .map(|index| (format!("property_{index}"), "x".repeat(512)))
                .collect();
            table
                .new_transaction(None)
                .update_properties(properties)
                .commit()
                .await
                .unwrap();
        }

        let start = std::time::Instant::now();
        for value in 0..100 {
            table
                .new_transaction(None)
                .update_properties(vec![("iteration".to_owned(), value.to_string())])
                .commit()
                .await
                .unwrap();
        }
        eprintln!("100 local metadata commits: {:?}", start.elapsed());
    }

    #[tokio::test]
    async fn rename_table_in_memory_moves_catalog_identity() {
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(
            FileCatalog::new("/dev/embucket", store.clone())
                .await
                .unwrap(),
        );
        let namespace = Namespace::try_new(&["public".to_string()]).unwrap();
        let source = iceberg_rust::catalog::identifier::Identifier::new(&namespace, "source_table");
        let destination =
            iceberg_rust::catalog::identifier::Identifier::new(&namespace, "destination_table");
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_string(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let mut builder = CreateTableBuilder::default();
        builder
            .with_name(source.name())
            .with_schema(schema)
            .build(&namespace, catalog.clone())
            .await
            .unwrap();

        let catalog = Arc::new(FileCatalog::new("/dev/embucket", store).await.unwrap());
        catalog.rename_table(&source, &destination).await.unwrap();

        assert!(!catalog.tabular_exists(&source).await.unwrap());
        assert!(catalog.tabular_exists(&destination).await.unwrap());
        assert_eq!(
            catalog.list_tabulars(&namespace).await.unwrap(),
            vec![destination.clone()]
        );
        let tabular = catalog.clone().load_tabular(&destination).await.unwrap();
        assert_eq!(tabular.identifier(), &destination);
    }

    #[tokio::test]
    async fn conflicting_table_commits_report_commit_conflict() {
        let store = ObjectStoreBuilder::memory();
        let first_catalog = Arc::new(
            FileCatalog::new("/dev/embucket", store.clone())
                .await
                .unwrap(),
        );
        let second_catalog = Arc::new(FileCatalog::new("/dev/embucket", store).await.unwrap());
        let namespace = Namespace::try_new(&["public".to_owned()]).unwrap();
        let identifier =
            iceberg_rust::catalog::identifier::Identifier::new(&namespace, "conflicting_table");
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let mut builder = CreateTableBuilder::default();
        let mut first = builder
            .with_name(identifier.name())
            .with_schema(schema)
            .build(&namespace, first_catalog.clone())
            .await
            .unwrap();
        let Tabular::Table(mut stale) = second_catalog
            .clone()
            .load_tabular(&identifier)
            .await
            .unwrap()
        else {
            panic!("expected table");
        };

        let rejected_requirement = first_catalog
            .clone()
            .update_table(CommitTable {
                identifier: identifier.clone(),
                requirements: vec![TableRequirement::AssertRefSnapshotId {
                    r#ref: "main".to_owned(),
                    snapshot_id: Some(123),
                }],
                updates: vec![],
            })
            .await;
        assert!(matches!(
            rejected_requirement,
            Err(IcebergError::CommitConflict(_))
        ));

        first
            .new_transaction(None)
            .update_properties(vec![("writer".to_owned(), "first".to_owned())])
            .commit()
            .await
            .unwrap();
        let stale_version = stale
            .new_transaction(None)
            .update_properties(vec![("writer".to_owned(), "second".to_owned())])
            .commit()
            .await;
        assert!(matches!(
            stale_version,
            Err(IcebergError::CommitConflict(_))
        ));
    }

    #[tokio::test]
    async fn test_create_update_drop_table() {
        let localstack = LocalStack::default()
            .with_env_var("SERVICES", "s3")
            .with_env_var("AWS_ACCESS_KEY_ID", "user")
            .with_env_var("AWS_SECRET_ACCESS_KEY", "password")
            .start()
            .await
            .unwrap();

        let command = localstack
            .exec(ExecCommand::new(vec![
                "awslocal",
                "s3api",
                "create-bucket",
                "--bucket",
                "warehouse",
            ]))
            .await
            .unwrap();

        while command.exit_code().await.unwrap().is_none() {
            sleep(Duration::from_millis(100)).await;
        }

        let localstack_host = localstack.get_host().await.unwrap();
        let localstack_port = localstack.get_host_port_ipv4(4566).await.unwrap();

        let object_store = ObjectStoreBuilder::s3()
            .with_config("aws_access_key_id", "user")
            .unwrap()
            .with_config("aws_secret_access_key", "password")
            .unwrap()
            .with_config(
                "endpoint",
                format!("http://{localstack_host}:{localstack_port}"),
            )
            .unwrap()
            .with_config("region", "us-east-1")
            .unwrap()
            .with_config("allow_http", "true")
            .unwrap();

        // Wait for bucket to be ready
        iceberg_rust::test_utils::wait_for_s3_bucket(&object_store, "s3://warehouse", None).await;
        // let object_store = ObjectStoreBuilder::memory();

        let iceberg_catalog: Arc<dyn Catalog> = Arc::new(
            FileCatalog::new("s3://warehouse", object_store.clone())
                .await
                .unwrap(),
        );

        let catalog = Arc::new(
            IcebergCatalog::new(iceberg_catalog.clone(), None)
                .await
                .unwrap(),
        );

        let mut config = SessionConfig::default();

        config.options_mut().execution.minimum_parallel_output_files =
            ConfigNonZeroUsize::try_new(1).unwrap();
        config
            .options_mut()
            .execution
            .parquet
            .maximum_parallel_row_group_writers = 4;

        let state = SessionStateBuilder::new()
            .with_config(config)
            .with_default_features()
            .with_query_planner(Arc::new(IcebergQueryPlanner::new()))
            .build();

        let ctx = SessionContext::new_with_state(state);

        ctx.register_catalog("warehouse", catalog);

        let sql = &"CREATE SCHEMA warehouse.tpch;".to_string();

        let plan = ctx.state().create_logical_plan(sql).await.unwrap();

        let transformed = plan.transform(iceberg_transform).data().unwrap();

        ctx.execute_logical_plan(transformed)
            .await
            .unwrap()
            .collect()
            .await
            .expect("Failed to execute query plan.");

        let sql = "CREATE EXTERNAL TABLE lineitem ( 
    L_ORDERKEY BIGINT NOT NULL, 
    L_PARTKEY BIGINT NOT NULL, 
    L_SUPPKEY BIGINT NOT NULL, 
    L_LINENUMBER INT NOT NULL, 
    L_QUANTITY DOUBLE NOT NULL, 
    L_EXTENDED_PRICE DOUBLE NOT NULL, 
    L_DISCOUNT DOUBLE NOT NULL, 
    L_TAX DOUBLE NOT NULL, 
    L_RETURNFLAG CHAR NOT NULL, 
    L_LINESTATUS CHAR NOT NULL, 
    L_SHIPDATE DATE NOT NULL, 
    L_COMMITDATE DATE NOT NULL, 
    L_RECEIPTDATE DATE NOT NULL, 
    L_SHIPINSTRUCT VARCHAR NOT NULL, 
    L_SHIPMODE VARCHAR NOT NULL, 
    L_COMMENT VARCHAR NOT NULL ) STORED AS CSV LOCATION '../../datafusion_iceberg/testdata/tpch/lineitem.csv' OPTIONS ('has_header' 'false');";

        let plan = ctx.state().create_logical_plan(sql).await.unwrap();

        let transformed = plan.transform(iceberg_transform).data().unwrap();

        ctx.execute_logical_plan(transformed)
            .await
            .unwrap()
            .collect()
            .await
            .expect("Failed to execute query plan.");

        let sql = "CREATE EXTERNAL TABLE warehouse.tpch.lineitem ( 
    L_ORDERKEY BIGINT NOT NULL, 
    L_PARTKEY BIGINT NOT NULL, 
    L_SUPPKEY BIGINT NOT NULL, 
    L_LINENUMBER INT NOT NULL, 
    L_QUANTITY DOUBLE NOT NULL, 
    L_EXTENDED_PRICE DOUBLE NOT NULL, 
    L_DISCOUNT DOUBLE NOT NULL, 
    L_TAX DOUBLE NOT NULL, 
    L_RETURNFLAG CHAR NOT NULL, 
    L_LINESTATUS CHAR NOT NULL, 
    L_SHIPDATE DATE NOT NULL, 
    L_COMMITDATE DATE NOT NULL, 
    L_RECEIPTDATE DATE NOT NULL, 
    L_SHIPINSTRUCT VARCHAR NOT NULL, 
    L_SHIPMODE VARCHAR NOT NULL, 
    L_COMMENT VARCHAR NOT NULL ) STORED AS ICEBERG LOCATION 's3://warehouse/tpch/lineitem';";

        let plan = ctx.state().create_logical_plan(sql).await.unwrap();

        let transformed = plan.transform(iceberg_transform).data().unwrap();

        ctx.execute_logical_plan(transformed)
            .await
            .unwrap()
            .collect()
            .await
            .expect("Failed to execute query plan.");

        let tables = iceberg_catalog
            .clone()
            .list_tabulars(
                &Namespace::try_new(&["tpch".to_owned()]).expect("Failed to create namespace"),
            )
            .await
            .expect("Failed to list Tables");
        assert_eq!(tables[0].to_string(), "tpch.lineitem".to_owned());

        let sql = "insert into warehouse.tpch.lineitem select * from lineitem;";

        let plan = ctx.state().create_logical_plan(sql).await.unwrap();

        let transformed = plan.transform(iceberg_transform).data().unwrap();

        ctx.execute_logical_plan(transformed)
            .await
            .unwrap()
            .collect()
            .await
            .expect("Failed to execute query plan.");

        let batches = ctx
        .sql("select sum(L_QUANTITY), L_PARTKEY from warehouse.tpch.lineitem group by L_PARTKEY;")
        .await
        .expect("Failed to create plan for select")
        .collect()
        .await
        .expect("Failed to execute select query");

        let mut once = false;

        for batch in batches {
            if batch.num_rows() != 0 {
                let (amounts, product_ids) = (
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap(),
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                );
                for (product_id, amount) in product_ids.iter().zip(amounts) {
                    if product_id.unwrap() == 24027 {
                        assert_eq!(amount.unwrap(), 24.0)
                    } else if product_id.unwrap() == 63700 {
                        assert_eq!(amount.unwrap(), 23.0)
                    }
                }
                once = true
            }
        }

        assert!(once);

        let object_store = object_store
            .build(Bucket::from_path("s3://warehouse").unwrap())
            .unwrap();

        let version_hint = object_store
            .get(&strip_prefix("s3://warehouse/tpch/lineitem/metadata/version-hint.text").into())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();

        assert_eq!(std::str::from_utf8(&version_hint).unwrap(), "1");

        let files = object_store.list(None).collect::<Vec<_>>().await;

        assert_eq!(
            files
                .iter()
                .filter(|x| x
                    .as_ref()
                    .unwrap()
                    .location
                    .extension()
                    .is_some_and(|x| x == "parquet"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn test_namespace_path_normal_case() {
        let test_struct = FileCatalog::new("/base/path", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        assert_eq!(
            test_struct.namespace_path("test_namespace"),
            "/base/path/test_namespace"
        );
    }

    #[tokio::test]
    async fn test_namespace_path_s3() {
        let test_struct = FileCatalog::new("s3://base/path", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        assert_eq!(
            test_struct.namespace_path("test_namespace"),
            "s3://base/path/test_namespace"
        );
    }

    #[tokio::test]
    async fn test_identifier_normal_case() {
        let test_struct = FileCatalog::new("/base/path", ObjectStoreBuilder::memory())
            .await
            .unwrap();

        let result = test_struct.identifier("/base/path/test_namespace/test_table");
        assert_eq!(result.namespace()[0], "test_namespace");
        assert_eq!(result.name(), "test_table");
    }

    #[tokio::test]
    async fn test_namespace_normal_case() {
        let test_struct = FileCatalog::new("/base/path", ObjectStoreBuilder::memory())
            .await
            .unwrap();

        let result = test_struct.namespace("/base/path/test_namespace").unwrap();
        assert_eq!(result.as_ref(), &["test_namespace".to_string()]);
    }

    // The path that comes in from object_store::ObjectStore::list_with_delimiter
    // has the LocalFileSystem root stripped (no leading "/" and no "file://"
    // scheme). Reproduce the rustice flow where the catalog is rooted at a
    // deep file:// URL and we try to recover the namespace name from a listed
    // common-prefix.
    #[tokio::test]
    async fn test_namespace_file_url_deep_prefix() {
        let test_struct = FileCatalog::new(
            "file:///tmp/rustice_test_catalog/embucket",
            ObjectStoreBuilder::memory(),
        )
        .await
        .unwrap();

        // What LocalFileSystem actually hands back for a child of
        // file:///tmp/rustice_test_catalog/embucket/demo (object_store::Path
        // strips the leading slash and the URL scheme).
        let result = test_struct
            .namespace("tmp/rustice_test_catalog/embucket/demo")
            .unwrap();
        assert_eq!(result.as_ref(), &["demo".to_string()]);
    }

    #[tokio::test]
    async fn test_identifier_file_url_deep_prefix() {
        let test_struct = FileCatalog::new(
            "file:///tmp/rustice_test_catalog/embucket",
            ObjectStoreBuilder::memory(),
        )
        .await
        .unwrap();

        let result = test_struct
            .identifier("tmp/rustice_test_catalog/embucket/demo/t1/metadata/v0.metadata.json");
        assert_eq!(result.namespace()[0], "demo");
        assert_eq!(result.name(), "t1");
    }

    #[tokio::test]
    async fn test_namespace_s3_url_deep_prefix() {
        let test_struct = FileCatalog::new("s3://bucket/wh/embucket", ObjectStoreBuilder::memory())
            .await
            .unwrap();

        // S3 list yields keys without the bucket; the leading slash is stripped.
        let result = test_struct.namespace("wh/embucket/demo").unwrap();
        assert_eq!(result.as_ref(), &["demo".to_string()]);
    }
}
