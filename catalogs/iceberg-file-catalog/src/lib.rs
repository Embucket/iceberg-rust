use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use futures::{future, stream, StreamExt, TryStreamExt};
use iceberg_rust::{
    catalog::{
        commit::{
            apply_table_updates, apply_view_updates, check_table_requirements,
            check_view_requirements, CommitTable, CommitView, TableRequirement,
        },
        create::{CreateMaterializedView, CreateTable, CreateView},
        identifier::Identifier,
        namespace::Namespace,
        tabular::{get_tabular_metadata, Tabular},
        Catalog, CatalogList,
    },
    error::Error as IcebergError,
    materialized_view::MaterializedView,
    object_store::{metadata_object_store_path, store::IcebergStore, Bucket, ObjectStoreBuilder},
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

use crate::error::Error;

#[derive(Debug)]
pub struct FileCatalog {
    path: String,
    object_store: ObjectStoreBuilder,
    cache: Arc<RwLock<HashMap<Identifier, (String, TabularMetadata)>>>,
}

pub mod error;

impl FileCatalog {
    pub async fn new(path: &str, object_store: ObjectStoreBuilder) -> Result<Self, Error> {
        Ok(FileCatalog {
            path: path.to_owned(),
            object_store,
            cache: Arc::new(RwLock::new(HashMap::new())),
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
        _namespace: &Namespace,
        _properties: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, String>, IcebergError> {
        Ok(HashMap::new())
    }
    /// Drop a namespace in the catalog
    async fn drop_namespace(&self, namespace: &Namespace) -> Result<(), IcebergError> {
        for identifier in self.list_tabulars(namespace).await? {
            self.drop_tabular(&identifier).await?;
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
    /// Check if a namespace exists. Namespaces are implicit on the filesystem
    /// — a namespace exists iff `list_namespaces` would report it (i.e., it
    /// has at least one tabular underneath).
    async fn namespace_exists(&self, namespace: &Namespace) -> Result<bool, IcebergError> {
        Ok(!self.list_tabulars(namespace).await?.is_empty())
    }
    async fn list_tabulars(&self, namespace: &Namespace) -> Result<Vec<Identifier>, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let mut entries = HashSet::new();
        for prefix in Self::candidate_paths(&self.namespace_path(&namespace[0])) {
            for identifier in self
                .list_identifiers_at_prefix(object_store.clone(), &prefix)
                .await?
            {
                if identifier.namespace() == namespace {
                    entries.insert(identifier);
                }
            }
        }
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by_key(ToString::to_string);
        Ok(entries)
    }
    async fn list_namespaces(&self, _parent: Option<&str>) -> Result<Vec<Namespace>, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let mut namespaces = HashSet::new();
        let unambiguous_catalog = Self::candidate_paths(&self.path).len() == 1;
        for prefix in Self::candidate_paths(self.path.trim_start_matches('/')) {
            let paths = object_store
                .list_with_delimiter(Some(&prefix))
                .await
                .map_err(IcebergError::from)?
                .common_prefixes;
            for path in paths {
                let relative = relative_path_from_root(path.as_ref(), &self.path);
                if unambiguous_catalog
                    && !relative.is_empty()
                    && !relative.contains('%')
                    && !relative.contains('/')
                {
                    let tables = object_store
                        .list_with_delimiter(Some(&path))
                        .await
                        .map_err(IcebergError::from)?
                        .common_prefixes;
                    for table in tables {
                        if Self::latest_metadata_under(
                            object_store.as_ref(),
                            &table.join("metadata"),
                        )
                        .await?
                        .is_some()
                        {
                            namespaces.insert(Namespace::try_new(&[relative])?);
                            break;
                        }
                    }
                    continue;
                }
                for identifier in self
                    .list_identifiers_at_prefix(object_store.clone(), &path)
                    .await?
                {
                    namespaces.insert(identifier.namespace().clone());
                }
            }
        }
        let mut namespaces: Vec<_> = namespaces.into_iter().collect();
        namespaces.sort_by_key(ToString::to_string);
        Ok(namespaces)
    }
    async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, IcebergError> {
        match self.metadata_location(identifier).await {
            Ok(_) => Ok(true),
            Err(IcebergError::CatalogNotFound) => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn drop_table(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.drop_tabular(identifier).await
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

        let metadata = get_tabular_metadata(&metadata_location, object_store.clone()).await?;

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

        let metadata_location = location + "/metadata/v0.metadata.json";

        Self::put_initial_metadata(object_store.as_ref(), &metadata_location, metadata.as_ref())
            .await?;

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

        Self::put_initial_metadata(object_store.as_ref(), &metadata_location, metadata.as_ref())
            .await?;

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

        match self.metadata_location(table_identifier).await {
            Ok(_) => {
                return Err(IcebergError::InvalidFormat(
                    "Storage table already exists".to_owned(),
                ));
            }
            Err(IcebergError::CatalogNotFound) => {}
            Err(error) => return Err(error),
        }

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

        Self::put_initial_metadata(
            object_store.as_ref(),
            &table_metadata_location,
            table_metadata.as_ref(),
        )
        .await?;
        if let Err(error) =
            Self::put_initial_metadata(object_store.as_ref(), &metadata_location, metadata.as_ref())
                .await
        {
            object_store
                .delete(&metadata_object_store_path(&table_metadata_location))
                .await?;
            return Err(error);
        }

        object_store.put_version_hint(&metadata_location).await.ok();

        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );

        Ok(MaterializedView::new(identifier.clone(), self.clone(), metadata).await?)
    }

    async fn update_table(self: Arc<Self>, commit: CommitTable) -> Result<Table, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            #[allow(clippy::if_same_then_else)]
            if !matches!(commit.requirements[0], TableRequirement::AssertCreate) {
                return Err(IcebergError::InvalidFormat(
                    "Create table assertion".to_owned(),
                ));
            } else {
                return Err(IcebergError::InvalidFormat(
                    "Create table assertion".to_owned(),
                ));
            }
        };
        let (previous_metadata_location, metadata) = entry;

        let TabularMetadata::Table(mut metadata) = metadata else {
            return Err(IcebergError::InvalidFormat(
                "Table update on entity that is not a table".to_owned(),
            ));
        };
        if !check_table_requirements(&commit.requirements, &metadata) {
            return Err(IcebergError::InvalidFormat(
                "Table requirements not valid".to_owned(),
            ));
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
                &metadata_object_store_path(&temp_metadata_location),
                &metadata_object_store_path(&metadata_location),
            )
            .await?;

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
                        &metadata_object_store_path(&temp_metadata_location),
                        &metadata_object_store_path(&metadata_location),
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
                        &metadata_object_store_path(&temp_metadata_location),
                        &metadata_object_store_path(&metadata_location),
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
    async fn put_initial_metadata(
        object_store: &dyn ObjectStore,
        location: &str,
        metadata: TabularMetadataRef<'_>,
    ) -> Result<(), IcebergError> {
        object_store
            .put_opts(
                &metadata_object_store_path(location),
                serde_json::to_vec(&metadata)?.into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    fn default_object_store(&self, bucket: Bucket) -> Arc<dyn object_store::ObjectStore> {
        Arc::new(self.object_store.build(bucket).unwrap())
    }
    fn namespace_path(&self, namespace: &str) -> String {
        self.path.as_str().trim_end_matches('/').to_owned() + "/" + namespace
    }

    fn tabular_path(&self, namespace: &str, name: &str) -> String {
        self.path.as_str().trim_end_matches('/').to_owned() + "/" + namespace + "/" + name
    }

    fn candidate_paths(location: &str) -> Vec<ObjectPath> {
        let canonical = metadata_object_store_path(location);
        let legacy = ObjectPath::from(strip_prefix(location));
        if canonical == legacy {
            vec![canonical]
        } else {
            vec![canonical, legacy]
        }
    }

    async fn metadata_files_under(
        object_store: &dyn ObjectStore,
        prefix: &ObjectPath,
    ) -> Result<Vec<(u64, String)>, IcebergError> {
        let version_prefix = format!("{prefix}/v");
        let files: Vec<String> = object_store
            .list(Some(prefix))
            .map_ok(|entry| entry.location.to_string())
            .try_filter(|location| {
                future::ready(
                    location.ends_with("metadata.json") && location.starts_with(&version_prefix),
                )
            })
            .try_collect()
            .await
            .map_err(IcebergError::from)?;
        let mut files: Vec<_> = files
            .into_iter()
            .filter_map(|location| parse_version(&location).ok().map(|v| (v, location)))
            .collect();
        files.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        Ok(files)
    }

    async fn latest_metadata_under(
        object_store: &dyn ObjectStore,
        prefix: &ObjectPath,
    ) -> Result<Option<(u64, String)>, IcebergError> {
        Ok(Self::metadata_files_under(object_store, prefix)
            .await?
            .into_iter()
            .next())
    }

    async fn list_identifiers_at_prefix(
        &self,
        object_store: Arc<dyn ObjectStore>,
        prefix: &ObjectPath,
    ) -> Result<Vec<Identifier>, IcebergError> {
        let tables = object_store
            .list_with_delimiter(Some(prefix))
            .await
            .map_err(IcebergError::from)?
            .common_prefixes;
        let unambiguous_catalog = Self::candidate_paths(&self.path).len() == 1;
        let catalog_prefix = format!("{}/", self.path.trim_end_matches('/'));
        let entries: Vec<Vec<Identifier>> = stream::iter(tables.into_iter().map(|table_path| {
            let object_store = object_store.clone();
            let catalog_prefix = &catalog_prefix;
            async move {
                let files = Self::metadata_files_under(
                    object_store.as_ref(),
                    &table_path.clone().join("metadata"),
                )
                .await?;
                if files.is_empty() {
                    return Ok::<Vec<Identifier>, IcebergError>(Vec::new());
                }
                let relative = relative_path_from_root(table_path.as_ref(), &self.path);
                if unambiguous_catalog && !relative.contains('%') {
                    let mut parts = relative.split('/');
                    let namespace = parts.next().unwrap_or_default();
                    let name = parts.next().unwrap_or_default();
                    if !namespace.is_empty() && !name.is_empty() {
                        return Ok(vec![Identifier::new(&[namespace.to_owned()], name)]);
                    }
                }
                let identifiers: Vec<Result<(Option<Identifier>, bool), IcebergError>> = stream::iter(files.into_iter().map(
                    |(_, location)| {
                        let object_store = object_store.clone();
                        async move {
                            match get_tabular_metadata(&location, object_store).await {
                                Ok(metadata) if metadata.as_ref().location().starts_with(catalog_prefix) => {
                                    match Self::identifier_from_metadata(&metadata) {
                                        Ok(identifier) => Ok((Some(identifier), false)),
                                        Err(error) => {
                                            tracing::warn!(%error, path = %location, "Invalid tabular identifier");
                                            Ok((None, true))
                                        }
                                    }
                                }
                                Ok(_) => Ok((None, false)),
                                Err(error @ (IcebergError::JSONSerde(_) | IcebergError::Decompress(_))) => {
                                    tracing::warn!(%error, path = %location, "Failed to read historical metadata");
                                    Ok((None, true))
                                }
                                Err(error) => Err(error),
                            }
                        }
                    },
                ))
                .buffer_unordered(8)
                .collect()
                .await;
                let identifiers: Vec<_> = identifiers.into_iter().collect::<Result<_, _>>()?;
                let has_corrupt_metadata = identifiers.iter().any(|(_, corrupt)| *corrupt);
                let identifiers: HashSet<_> = identifiers.into_iter().filter_map(|(id, _)| id).collect();
                if identifiers.is_empty() && has_corrupt_metadata {
                    return Err(IcebergError::InvalidFormat(
                        "No readable metadata for tabular prefix".to_owned(),
                    ));
                }
                Ok::<Vec<Identifier>, IcebergError>(identifiers.into_iter().collect())
            }
        }))
        .buffer_unordered(8)
        .try_collect()
        .await?;
        Ok(entries.into_iter().flatten().collect())
    }

    fn identifier_from_metadata(metadata: &TabularMetadata) -> Result<Identifier, IcebergError> {
        let metadata_ref = metadata.as_ref();
        let location = metadata_ref.location();
        let mut parts = location.trim_end_matches('/').rsplit('/');
        let name = parts.next().unwrap_or_default();
        let namespace = parts.next().unwrap_or_default();
        if namespace.is_empty() || name.is_empty() {
            return Err(IcebergError::InvalidFormat("Tabular location".to_owned()));
        }
        Ok(Identifier::new(&[namespace.to_owned()], name))
    }

    async fn drop_tabular(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let metadata_location = self.metadata_location(identifier).await?;
        let metadata = get_tabular_metadata(&metadata_location, object_store.clone()).await?;
        let table_path = self.tabular_path(&identifier.namespace()[0], identifier.name());
        let mut paths = HashSet::new();
        for prefix in Self::candidate_paths(&table_path) {
            let files =
                Self::metadata_files_under(object_store.as_ref(), &prefix.clone().join("metadata"))
                    .await?;
            let mut has_own_metadata = false;
            let mut has_foreign_metadata = false;
            for (_, location) in files {
                let candidate = get_tabular_metadata(&location, object_store.clone()).await?;
                if candidate.as_ref().location() == table_path
                    && candidate.as_ref().uuid() == metadata.as_ref().uuid()
                {
                    has_own_metadata = true;
                } else {
                    has_foreign_metadata = true;
                }
            }
            if has_own_metadata && has_foreign_metadata {
                return Err(IcebergError::InvalidFormat(
                    "Cannot drop a table whose object prefix contains another table".to_owned(),
                ));
            }
            if !has_own_metadata {
                continue;
            }
            let listed: Vec<_> = object_store
                .list(Some(&prefix))
                .map_ok(|entry| entry.location)
                .try_collect()
                .await
                .map_err(IcebergError::from)?;
            paths.extend(listed);
        }
        for path in paths {
            object_store
                .delete(&path)
                .await
                .map_err(IcebergError::from)?;
        }

        self.cache.write().unwrap().remove(identifier);
        Ok(())
    }

    async fn metadata_location(&self, identifier: &Identifier) -> Result<String, IcebergError> {
        let bucket = Bucket::from_path(&self.path)?;
        let object_store = self.object_store.build(bucket)?;

        let table_path = self.tabular_path(&identifier.namespace()[0], identifier.name());
        let path = format!("{table_path}/metadata");
        let candidates = Self::candidate_paths(&path);
        let mut latest: Option<(u64, String)> = None;
        for prefix in &candidates {
            for (version, location) in
                Self::metadata_files_under(object_store.as_ref(), prefix).await?
            {
                if candidates.len() > 1
                    && get_tabular_metadata(&location, object_store.clone())
                        .await?
                        .as_ref()
                        .location()
                        != table_path
                {
                    continue;
                }
                if latest
                    .as_ref()
                    .is_none_or(|(current, _)| version > *current)
                {
                    latest = Some((version, location));
                }
                break;
            }
        }
        latest
            .map(|(_, location)| location)
            .ok_or(IcebergError::CatalogNotFound)
    }
}

fn relative_path_from_root(path: &str, root: &str) -> String {
    let canonical_root = metadata_object_store_path(root).to_string();
    let legacy_root = ObjectPath::from(strip_prefix(root)).to_string();
    for prefix in [canonical_root, legacy_root] {
        if path == prefix {
            return String::new();
        }
        if let Some(relative) = path.strip_prefix(&format!("{prefix}/")) {
            return relative.to_owned();
        }
    }
    path.to_owned()
}

fn parse_version(path: &str) -> Result<u64, IcebergError> {
    path.split('/')
        .next_back()
        .ok_or(IcebergError::InvalidFormat("Metadata location".to_owned()))?
        .trim_start_matches('v')
        .trim_end_matches(".metadata.json")
        .trim_end_matches(".gz")
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
    use iceberg_rust::{
        catalog::create::{CreateMaterializedViewBuilder, CreateTableBuilder},
        spec::{
            schema::Schema,
            types::{PrimitiveType, StructField, StructType, Type},
            view_metadata::Version,
        },
    };

    #[test]
    fn listed_object_keys_are_not_decoded_twice() {
        let root = "s3://warehouse/catalog/name=%E6%97%A5";
        let canonical = metadata_object_store_path(root);
        let legacy = ObjectPath::from(strip_prefix(root));
        assert_ne!(canonical, legacy);
        assert_eq!(
            relative_path_from_root(&format!("{canonical}/ns/table"), root),
            "ns/table"
        );
        assert_eq!(
            relative_path_from_root(&format!("{legacy}/ns/table"), root),
            "ns/table"
        );
    }

    #[tokio::test]
    async fn encoded_catalog_names_round_trip_with_legacy_metadata() {
        let root = "memory:///warehouse/name=%E6%97%A5";
        let store = ObjectStoreBuilder::memory();
        let namespace = Namespace::try_new(&["é".to_owned()]).unwrap();
        let identifier = Identifier::new(&["é".to_owned()], "日");
        let other_namespace = Namespace::try_new(&["other".to_owned()]).unwrap();
        let other_identifier = Identifier::new(&["other".to_owned()], "plain");
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let create = CreateTableBuilder::default()
            .with_name("日")
            .with_schema(schema.clone())
            .create()
            .unwrap();
        let other_create = CreateTableBuilder::default()
            .with_name("plain")
            .with_schema(schema)
            .create()
            .unwrap();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        catalog
            .clone()
            .create_table(identifier.clone(), create)
            .await
            .unwrap();
        catalog
            .clone()
            .create_table(other_identifier.clone(), other_create)
            .await
            .unwrap();

        let fresh = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        assert!(matches!(
            fresh.clone().load_tabular(&identifier).await.unwrap(),
            Tabular::Table(_)
        ));
        assert_eq!(
            fresh.list_tabulars(&namespace).await.unwrap(),
            vec![identifier.clone()]
        );
        let namespaces = fresh.list_namespaces(None).await.unwrap();
        assert_eq!(namespaces.len(), 2);
        assert!(namespaces.contains(&namespace));
        assert!(namespaces.contains(&other_namespace));

        let metadata_location = format!("{root}/é/日/metadata/v0.metadata.json");
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let canonical = metadata_object_store_path(&metadata_location);
        let legacy = ObjectPath::from(strip_prefix(&metadata_location));
        assert_ne!(canonical, legacy);
        objects.copy(&canonical, &legacy).await.unwrap();
        objects.delete(&canonical).await.unwrap();
        objects
            .delete(&metadata_object_store_path(&format!(
                "{root}/é/日/metadata/version-hint.text"
            )))
            .await
            .unwrap();

        let reopened = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        assert!(matches!(
            reopened.clone().load_tabular(&identifier).await.unwrap(),
            Tabular::Table(_)
        ));
        assert_eq!(
            reopened.list_tabulars(&namespace).await.unwrap(),
            vec![identifier.clone()]
        );

        let canonical_v1 =
            metadata_object_store_path(&format!("{root}/é/日/metadata/v1.metadata.json"));
        objects.copy(&legacy, &canonical_v1).await.unwrap();
        let mixed = Arc::new(FileCatalog::new(root, store).await.unwrap());
        assert_eq!(
            mixed.metadata_location(&identifier).await.unwrap(),
            canonical_v1.to_string()
        );
        let legacy_v2 = ObjectPath::from(strip_prefix(&format!(
            "{root}/é/日/metadata/v2.metadata.json"
        )));
        objects.copy(&legacy, &legacy_v2).await.unwrap();
        assert_eq!(
            mixed.metadata_location(&identifier).await.unwrap(),
            legacy_v2.to_string()
        );
        mixed.drop_table(&identifier).await.unwrap();
        assert!(objects.get(&legacy).await.is_err());
        assert!(objects.get(&canonical_v1).await.is_err());
        assert!(objects.get(&legacy_v2).await.is_err());
        assert!(!mixed.tabular_exists(&identifier).await.unwrap());
        assert!(mixed.list_tabulars(&namespace).await.unwrap().is_empty());
        assert_eq!(
            mixed.list_tabulars(&other_namespace).await.unwrap(),
            vec![other_identifier]
        );
        assert_eq!(
            mixed.list_namespaces(None).await.unwrap(),
            vec![other_namespace]
        );
    }

    #[tokio::test]
    async fn dropping_namespace_does_not_delete_aliased_canonical_key() {
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new("memory:///alias", store).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let first = Identifier::new(&["a b".to_owned()], "t");
        let second = Identifier::new(&["a%2520b".to_owned()], "t");
        for identifier in [&first, &second] {
            let create = CreateTableBuilder::default()
                .with_name(identifier.name())
                .with_schema(schema.clone())
                .create()
                .unwrap();
            catalog
                .clone()
                .create_table(identifier.clone(), create)
                .await
                .unwrap();
        }
        catalog
            .drop_namespace(&Namespace::try_new(&["a b".to_owned()]).unwrap())
            .await
            .unwrap();
        assert!(!catalog.tabular_exists(&first).await.unwrap());
        assert!(catalog.tabular_exists(&second).await.unwrap());
        assert_eq!(
            catalog
                .list_tabulars(&Namespace::try_new(&["a%2520b".to_owned()]).unwrap())
                .await
                .unwrap(),
            vec![second]
        );
    }

    #[tokio::test]
    async fn initial_metadata_create_does_not_overwrite_aliased_legacy_key() {
        let root = "memory:///collision";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let first = Identifier::new(&["a b".to_owned()], "t");
        let create = CreateTableBuilder::default()
            .with_name("t")
            .with_schema(schema.clone())
            .create()
            .unwrap();
        catalog
            .clone()
            .create_table(first.clone(), create)
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let uri = format!("{root}/a b/t/metadata/v0.metadata.json");
        let canonical = metadata_object_store_path(&uri);
        let legacy = ObjectPath::from(strip_prefix(&uri));
        objects.copy(&canonical, &legacy).await.unwrap();
        objects.delete(&canonical).await.unwrap();
        let before = objects.get(&legacy).await.unwrap().bytes().await.unwrap();

        let second = Identifier::new(&["a%2520b".to_owned()], "t");
        let create = CreateTableBuilder::default()
            .with_name("t")
            .with_schema(schema)
            .create()
            .unwrap();
        assert!(catalog.clone().create_table(second, create).await.is_err());
        assert_eq!(
            objects.get(&legacy).await.unwrap().bytes().await.unwrap(),
            before
        );
        assert!(catalog.tabular_exists(&first).await.unwrap());
    }

    #[tokio::test]
    async fn shared_prefix_with_mixed_owners_cannot_be_deleted() {
        let root = "memory:///mixed_alias";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let first = Identifier::new(&["a b".to_owned()], "t");
        let second = Identifier::new(&["a%2520b".to_owned()], "t");
        for identifier in [&first, &second] {
            let create = CreateTableBuilder::default()
                .with_name("t")
                .with_schema(schema.clone())
                .create()
                .unwrap();
            catalog
                .clone()
                .create_table(identifier.clone(), create)
                .await
                .unwrap();
        }
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let first_uri = format!("{root}/a b/t/metadata/v0.metadata.json");
        let first_canonical = metadata_object_store_path(&first_uri);
        let first_legacy = ObjectPath::from(strip_prefix(&first_uri));
        let second_uri = format!("{root}/a%2520b/t/metadata/v0.metadata.json");
        let second_canonical = metadata_object_store_path(&second_uri);
        let second_legacy = ObjectPath::from(strip_prefix(&second_uri));
        assert_eq!(first_legacy, second_canonical);
        objects
            .copy(&second_canonical, &second_legacy)
            .await
            .unwrap();
        objects.delete(&second_canonical).await.unwrap();
        objects.copy(&first_canonical, &first_legacy).await.unwrap();
        objects.delete(&first_canonical).await.unwrap();
        let second_v1 =
            metadata_object_store_path(&format!("{root}/a%2520b/t/metadata/v1.metadata.json"));
        objects.copy(&second_legacy, &second_v1).await.unwrap();

        let fresh = Arc::new(FileCatalog::new(root, store).await.unwrap());
        assert!(matches!(
            fresh.clone().load_tabular(&first).await.unwrap(),
            Tabular::Table(_)
        ));
        assert!(matches!(
            fresh.clone().load_tabular(&second).await.unwrap(),
            Tabular::Table(_)
        ));
        assert!(fresh.drop_table(&second).await.is_err());
        assert!(objects.get(&first_legacy).await.is_ok());
        assert!(objects.get(&second_legacy).await.is_ok());
        assert!(objects.get(&second_v1).await.is_ok());
    }

    #[tokio::test]
    async fn materialized_view_create_preserves_existing_storage_table() {
        let root = "memory:///mv_conflict";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let storage = Identifier::new(&["ns".to_owned()], "mv__storage");
        catalog
            .clone()
            .create_table(
                storage.clone(),
                CreateTableBuilder::default()
                    .with_name("mv__storage")
                    .with_schema(schema.clone())
                    .create()
                    .unwrap(),
            )
            .await
            .unwrap();
        let view = Identifier::new(&["ns".to_owned()], "mv");
        let version = Version {
            version_id: 1,
            storage_table: storage.clone(),
            ..Default::default()
        };
        let create = CreateMaterializedViewBuilder::default()
            .with_name("mv")
            .with_schema(schema)
            .with_view_version(version)
            .create()
            .unwrap();
        assert!(catalog
            .clone()
            .create_materialized_view(view.clone(), create)
            .await
            .is_err());
        assert!(catalog.tabular_exists(&storage).await.unwrap());
        assert!(!catalog.tabular_exists(&view).await.unwrap());
    }

    #[tokio::test]
    async fn materialized_view_create_preserves_legacy_storage_table() {
        let root = "memory:///mv_legacy_conflict";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let storage = Identifier::new(&["ns".to_owned()], "mv storage");
        catalog
            .clone()
            .create_table(
                storage.clone(),
                CreateTableBuilder::default()
                    .with_name("mv storage")
                    .with_schema(schema.clone())
                    .create()
                    .unwrap(),
            )
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let storage_uri = format!("{root}/ns/mv storage/metadata/v0.metadata.json");
        let canonical = metadata_object_store_path(&storage_uri);
        let legacy = ObjectPath::from(strip_prefix(&storage_uri));
        objects.copy(&canonical, &legacy).await.unwrap();
        objects.delete(&canonical).await.unwrap();

        let view = Identifier::new(&["ns".to_owned()], "mv");
        let create = CreateMaterializedViewBuilder::default()
            .with_name("mv")
            .with_schema(schema)
            .with_view_version(Version {
                version_id: 1,
                storage_table: storage.clone(),
                ..Default::default()
            })
            .create()
            .unwrap();
        assert!(catalog
            .clone()
            .create_materialized_view(view.clone(), create)
            .await
            .is_err());
        assert!(objects.get(&legacy).await.is_ok());
        assert!(objects.get(&canonical).await.is_err());
        assert!(catalog.tabular_exists(&storage).await.unwrap());
        assert!(!catalog.tabular_exists(&view).await.unwrap());
    }

    #[tokio::test]
    async fn aliased_catalog_does_not_list_foreign_tables() {
        let store = ObjectStoreBuilder::memory();
        let foreign = Arc::new(
            FileCatalog::new("memory:///warehouse/cat%2520a", store.clone())
                .await
                .unwrap(),
        );
        let local = FileCatalog::new("memory:///warehouse/cat a", store)
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["ns".to_owned()]).unwrap();
        let identifier = Identifier::new(&["ns".to_owned()], "t");
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        foreign
            .clone()
            .create_table(
                identifier.clone(),
                CreateTableBuilder::default()
                    .with_name("t")
                    .with_schema(schema)
                    .create()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            foreign.list_tabulars(&namespace).await.unwrap(),
            vec![identifier]
        );
        assert!(local.list_tabulars(&namespace).await.unwrap().is_empty());
        assert!(local.list_namespaces(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupt_historical_version_does_not_hide_valid_encoded_table() {
        let root = "memory:///history";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let namespace = Namespace::try_new(&["ns".to_owned()]).unwrap();
        let identifier = Identifier::new(&["ns".to_owned()], "a b");
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        catalog
            .clone()
            .create_table(
                identifier.clone(),
                CreateTableBuilder::default()
                    .with_name("a b")
                    .with_schema(schema)
                    .create()
                    .unwrap(),
            )
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let v0 = metadata_object_store_path(&format!("{root}/ns/a b/metadata/v0.metadata.json"));
        let v1 = metadata_object_store_path(&format!("{root}/ns/a b/metadata/v1.metadata.json"));
        objects.copy(&v0, &v1).await.unwrap();
        objects.put(&v0, "not json".into()).await.unwrap();

        assert_eq!(
            catalog.list_tabulars(&namespace).await.unwrap(),
            vec![identifier]
        );
        assert_eq!(
            catalog.list_namespaces(None).await.unwrap(),
            vec![namespace]
        );
    }

    #[tokio::test]
    async fn unreadable_legacy_metadata_cannot_be_shadowed_by_create() {
        let root = "memory:///unreadable_legacy";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let identifier = Identifier::new(&["ns".to_owned()], "a b");
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let create = || {
            CreateTableBuilder::default()
                .with_name("a b")
                .with_schema(schema.clone())
                .create()
                .unwrap()
        };
        catalog
            .clone()
            .create_table(identifier.clone(), create())
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let uri = format!("{root}/ns/a b/metadata/v0.metadata.json");
        let canonical = metadata_object_store_path(&uri);
        let legacy = ObjectPath::from(strip_prefix(&uri));
        objects.copy(&canonical, &legacy).await.unwrap();
        objects.delete(&canonical).await.unwrap();
        objects.put(&legacy, "not json".into()).await.unwrap();

        assert!(catalog.tabular_exists(&identifier).await.is_err());
        assert!(catalog
            .clone()
            .create_table(identifier.clone(), create())
            .await
            .is_err());
        assert!(objects.get(&canonical).await.is_err());
        assert!(catalog
            .drop_namespace(&Namespace::try_new(&["ns".to_owned()]).unwrap())
            .await
            .is_err());
        assert!(objects.get(&legacy).await.is_ok());
    }

    #[tokio::test]
    async fn orphan_version_hint_does_not_report_namespace() {
        let root = "memory:///orphan";
        let store = ObjectStoreBuilder::memory();
        let catalog = Arc::new(FileCatalog::new(root, store.clone()).await.unwrap());
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let create = CreateTableBuilder::default()
            .with_name("t")
            .with_schema(schema)
            .create()
            .unwrap();
        let identifier = Identifier::new(&["plain".to_owned()], "t");
        catalog
            .clone()
            .create_table(identifier.clone(), create)
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        objects
            .delete(&metadata_object_store_path(
                "memory:///orphan/plain/t/metadata/v0.metadata.json",
            ))
            .await
            .unwrap();
        let fresh = FileCatalog::new(root, store).await.unwrap();
        assert!(!fresh.tabular_exists(&identifier).await.unwrap());
        assert!(!fresh
            .namespace_exists(&Namespace::try_new(&["plain".to_owned()]).unwrap())
            .await
            .unwrap());
        assert!(fresh.list_namespaces(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn catalog_list_reads_legacy_unicode_catalog_name() {
        let root = "memory:///catalogs";
        let store = ObjectStoreBuilder::memory();
        let list = FileCatalogList::new(root, store.clone()).await.unwrap();
        let catalog = list.catalog("é").unwrap();
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Int),
                None,
            )]),
            0,
            None,
        );
        let create = CreateTableBuilder::default()
            .with_name("t")
            .with_schema(schema.clone())
            .create()
            .unwrap();
        catalog
            .clone()
            .create_table(Identifier::new(&["ns".to_owned()], "t"), create)
            .await
            .unwrap();
        let objects = store.build(Bucket::from_path(root).unwrap()).unwrap();
        let uri = "memory:///catalogs/é/ns/t/metadata/v0.metadata.json";
        let canonical = metadata_object_store_path(uri);
        let legacy = ObjectPath::from(strip_prefix(uri));
        objects.copy(&canonical, &legacy).await.unwrap();
        objects.delete(&canonical).await.unwrap();
        objects
            .delete(&metadata_object_store_path(
                "memory:///catalogs/é/ns/t/metadata/version-hint.text",
            ))
            .await
            .unwrap();
        objects
            .put(
                &ObjectPath::parse("catalogs/%25C3%25A9/ns/bad/metadata/v0.metadata.json").unwrap(),
                b"invalid metadata".to_vec().into(),
            )
            .await
            .unwrap();
        let aliased_catalog = list.catalog("%25C3%25A9").unwrap();
        let aliased_create = CreateTableBuilder::default()
            .with_name("other")
            .with_schema(schema)
            .create()
            .unwrap();
        aliased_catalog
            .create_table(
                Identifier::new(&["other_ns".to_owned()], "other"),
                aliased_create,
            )
            .await
            .unwrap();
        assert_eq!(list.list_catalogs().await, vec!["%25C3%25A9", "é"]);
    }

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

    async fn parse_catalog(
        &self,
        path: &ObjectPath,
        object_store: Arc<dyn ObjectStore>,
    ) -> Vec<String> {
        let relative = relative_path_from_root(path.as_ref(), &self.path);
        let name = relative.split('/').next().unwrap_or_default();
        if !name.is_empty() && !name.contains('%') {
            return vec![name.to_owned()];
        }

        let namespaces = match object_store.list_with_delimiter(Some(path)).await {
            Ok(list) => list.common_prefixes,
            Err(error) => {
                tracing::warn!(%error, catalog_path = %path, "Failed to list namespaces");
                return Vec::new();
            }
        };
        let tables: Vec<Vec<ObjectPath>> = stream::iter(namespaces.into_iter().map(|namespace| {
            let object_store = object_store.clone();
            async move {
                match object_store.list_with_delimiter(Some(&namespace)).await {
                    Ok(list) => list.common_prefixes,
                    Err(error) => {
                        tracing::warn!(%error, path = %namespace, "Failed to list tables");
                        Vec::new()
                    }
                }
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;
        let files: Vec<Vec<(u64, String)>> =
            stream::iter(tables.into_iter().flatten().map(|table| {
                let object_store = object_store.clone();
                async move {
                    match FileCatalog::metadata_files_under(
                        object_store.as_ref(),
                        &table.clone().join("metadata"),
                    )
                    .await
                    {
                        Ok(files) => files,
                        Err(error) => {
                            tracing::warn!(%error, path = %table, "Failed to list metadata");
                            Vec::new()
                        }
                    }
                }
            }))
            .buffer_unordered(8)
            .collect()
            .await;
        let names: Vec<Option<String>> =
            stream::iter(files.into_iter().flatten().map(|(_, location)| {
                let object_store = object_store.clone();
                async move {
                    match get_tabular_metadata(&location, object_store).await {
                        Ok(metadata) => {
                            let metadata_ref = metadata.as_ref();
                            let name = metadata_ref
                                .location()
                                .trim_end_matches('/')
                                .rsplit('/')
                                .nth(2);
                            if name.is_none() {
                                tracing::warn!(path = %location, "Invalid catalog location");
                            }
                            name.map(ToOwned::to_owned)
                        }
                        Err(error) => {
                            tracing::warn!(%error, path = %location, "Failed to read metadata");
                            None
                        }
                    }
                }
            }))
            .buffer_unordered(8)
            .collect()
            .await;
        names
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }
}

#[async_trait]
impl CatalogList for FileCatalogList {
    fn catalog(&self, name: &str) -> Option<Arc<dyn Catalog>> {
        Some(Arc::new(FileCatalog {
            path: self.path.clone() + "/" + name,
            object_store: self.object_store.clone(),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }))
    }
    async fn list_catalogs(&self) -> Vec<String> {
        let bucket = match Bucket::from_path(&self.path) {
            Ok(bucket) => bucket,
            Err(error) => {
                tracing::warn!(%error, "Failed to resolve catalog bucket");
                return Vec::new();
            }
        };
        let object_store = match self.object_store.build(bucket) {
            Ok(object_store) => object_store,
            Err(error) => {
                tracing::warn!(%error, "Failed to build catalog object store");
                return Vec::new();
            }
        };

        let mut catalogs = HashSet::new();
        for prefix in FileCatalog::candidate_paths(&self.path) {
            let paths = match object_store.list_with_delimiter(Some(&prefix)).await {
                Ok(list) => list.common_prefixes,
                Err(error) => {
                    tracing::warn!(%error, path = %prefix, "Failed to list catalogs");
                    continue;
                }
            };
            for path in paths {
                for name in self.parse_catalog(&path, object_store.clone()).await {
                    catalogs.insert(name);
                }
            }
        }
        let mut catalogs: Vec<_> = catalogs.into_iter().collect();
        catalogs.sort();
        catalogs
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
        catalog::{namespace::Namespace, Catalog},
        object_store::{Bucket, ObjectStoreBuilder},
        spec::util::strip_prefix,
    };
    use object_store::ObjectStoreExt;
    use std::{sync::Arc, time::Duration};
    use testcontainers::{core::ExecCommand, runners::AsyncRunner, ImageExt};
    use testcontainers_modules::localstack::LocalStack;
    use tokio::time::sleep;
    // use testcontainers::{core::ExecCommand, runners::AsyncRunner, ImageExt};
    // use testcontainers_modules::localstack::LocalStack;

    use crate::FileCatalog;

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
