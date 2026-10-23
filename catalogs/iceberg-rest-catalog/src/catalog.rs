use crate::{
    apis::{
        self, catalog_api_api,
        configuration::{self, Configuration},
    },
    models::{self, StorageCredential},
};
use async_trait::async_trait;
use futures::{FutureExt, TryFutureExt};
use iceberg_rust::object_store::vended::{StoreClientConfig, VendedStoreCache};
/**
Iceberg rest catalog implementation
*/
use iceberg_rust::{
    catalog::{
        commit::CommitView,
        create::{CreateMaterializedView, CreateTable, CreateView},
        identifier::{self, Identifier},
        namespace::Namespace,
        tabular::Tabular,
        Catalog, CatalogList,
    },
    error::Error,
    materialized_view::MaterializedView,
    object_store::{Bucket, ObjectStoreBuilder},
    spec::{
        identifier::FullIdentifier,
        materialized_view_metadata::MaterializedViewMetadata,
        table_metadata::TableMetadata,
        tabular::TabularMetadata,
        view_metadata::{self, ViewMetadata},
    },
    table::Table,
    view::View,
};
use object_store::{aws::AmazonS3Builder, ObjectStore, ObjectStoreScheme};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
};
use url::Url;

/// Identity of the table a vended store serves: catalog name plus table identifier.
type VendedStoreKey = (Option<String>, Identifier);

#[derive(Debug)]
pub struct RestCatalog {
    name: Option<String>,
    configuration: Configuration,
    default_object_store_builder: Option<ObjectStoreBuilder>,
    ignore_storage_credentials: bool,
    cache: Arc<RwLock<HashMap<Identifier, Arc<dyn ObjectStore>>>>,
    vended_stores: Arc<VendedStoreCache<VendedStoreKey>>,
}

impl RestCatalog {
    pub fn new(
        name: Option<&str>,
        configuration: Configuration,
        default_object_store_builder: Option<ObjectStoreBuilder>,
        ignore_storage_credentials: bool,
    ) -> Self {
        RestCatalog {
            name: name.map(ToString::to_string),
            configuration,
            default_object_store_builder,
            ignore_storage_credentials,
            cache: Arc::new(RwLock::new(HashMap::new())),
            vended_stores: Arc::default(),
        }
    }

    /// Build stores from vended storage credentials with `client` (client options, retry policy,
    /// HTTP connector) instead of object_store's defaults.
    ///
    /// Without this call vended stores use object_store's default client settings. In both
    /// cases a reload of the same table at the same location keeps its store and only swaps the
    /// vended credentials.
    pub fn with_store_client_config(mut self, client: StoreClientConfig) -> Self {
        self.vended_stores = Arc::new(VendedStoreCache::new(client));
        self
    }

    /// How many object stores this catalog has built from vended storage configuration.
    /// Reloads that reuse a store and swap its credentials do not count.
    pub fn vended_store_builds(&self) -> u64 {
        self.vended_stores.builds()
    }

    fn with_vended_stores(mut self, vended_stores: Arc<VendedStoreCache<VendedStoreKey>>) -> Self {
        self.vended_stores = vended_stores;
        self
    }

    fn vended_store_key(&self, identifier: &Identifier) -> VendedStoreKey {
        (self.name.clone(), identifier.clone())
    }

    fn get_object_store(
        &self,
        identifier: &Identifier,
        response: &models::LoadTableResult,
    ) -> Result<Arc<dyn ObjectStore>, Error> {
        let default_store = || {
            self.default_object_store_builder
                .as_ref()
                .ok_or(Error::NotFound("Default object store".to_string()))
                .and_then(|x| {
                    let bucket = Bucket::from_path(&response.metadata.location)?;
                    x.build(bucket)
                })
        };
        if self.ignore_storage_credentials {
            return default_store();
        }

        match vended_storage_config(response) {
            Some((config, credential_prefix)) => self.vended_stores.get_or_build(
                self.vended_store_key(identifier),
                Url::parse(&response.metadata.location)?,
                config,
                credential_prefix,
            ),
            None => default_store(),
        }
    }
}

#[async_trait]
impl Catalog for RestCatalog {
    /// Catalog name
    fn name(&self) -> &str {
        self.name.as_ref().unwrap()
    }
    /// Create a namespace in the catalog
    async fn create_namespace(
        &self,
        namespace: &Namespace,
        properties: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, String>, Error> {
        let configuration = self.configuration.clone();
        let response = catalog_api_api::create_namespace(
            &configuration,
            self.name.as_deref(),
            models::CreateNamespaceRequest {
                namespace: namespace.to_vec(),
                properties,
            },
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(response.properties.unwrap_or_default())
    }
    /// Drop a namespace in the catalog
    async fn drop_namespace(&self, namespace: &Namespace) -> Result<(), Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::drop_namespace(
            &configuration,
            self.name.as_deref(),
            &namespace.to_string(),
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(())
    }
    /// Load the namespace properties from the catalog
    async fn load_namespace(
        &self,
        namespace: &Namespace,
    ) -> Result<HashMap<String, String>, Error> {
        let configuration = self.configuration.clone();
        let response = catalog_api_api::load_namespace_metadata(
            &configuration,
            self.name.as_deref(),
            &namespace.to_string(),
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(response.properties.unwrap_or_default())
    }
    /// Update the namespace properties in the catalog
    async fn update_namespace(
        &self,
        namespace: &Namespace,
        updates: Option<HashMap<String, String>>,
        removals: Option<Vec<String>>,
    ) -> Result<(), Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::update_properties(
            &configuration,
            self.name.as_deref(),
            &namespace.to_string(),
            models::UpdateNamespacePropertiesRequest { updates, removals },
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(())
    }
    /// Check if a namespace exists
    async fn namespace_exists(&self, namespace: &Namespace) -> Result<bool, Error> {
        let configuration = self.configuration.clone();

        match catalog_api_api::namespace_exists(
            &configuration,
            self.name.as_deref(),
            &namespace.to_string(),
        )
        .await
        {
            Ok(()) => Ok(true),
            Err(apis::Error::ResponseError(response))
                if response.status == reqwest::StatusCode::NOT_FOUND =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
    /// Lists all tables in the given namespace.
    async fn list_tabulars(&self, namespace: &Namespace) -> Result<Vec<Identifier>, Error> {
        let configuration = self.configuration.clone();
        let tables = catalog_api_api::list_tables(
            &configuration,
            self.name.as_deref(),
            &namespace.to_string(),
            None,
            None,
        )
        .await
        .map_err(Into::<Error>::into)?;
        let tables = tables.identifiers.unwrap_or(Vec::new()).into_iter();

        // Try to list views, but handle 404 gracefully (some catalogs like Databricks Unity Catalog
        // don't implement the views endpoint yet)
        let views = match catalog_api_api::list_views(
            &self.configuration,
            self.name.as_deref(),
            &namespace.to_string(),
            None,
            None,
        )
        .await
        {
            Ok(views) => views.identifiers.unwrap_or(Vec::new()),
            Err(err) => {
                if let apis::Error::ResponseError(ref response_err) = err {
                    if response_err.status.as_u16() == 404 {
                        Vec::new()
                    } else {
                        return Err(err.into());
                    }
                } else {
                    return Err(err.into());
                }
            }
        };

        Ok(views.into_iter().chain(tables).collect())
    }
    /// Lists all namespaces in the catalog.
    async fn list_namespaces(&self, parent: Option<&str>) -> Result<Vec<Namespace>, Error> {
        let configuration = self.configuration.clone();
        let namespaces = catalog_api_api::list_namespaces(
            &configuration,
            self.name.as_deref(),
            None,
            None,
            parent,
        )
        .await
        .map_err(Into::<Error>::into)?;
        namespaces
            .namespaces
            .ok_or(Error::NotFound(format!(
                "Namespaces in catalog {:?}",
                self.name
            )))?
            .into_iter()
            .map(|x| Namespace::try_new(&x))
            .collect::<Result<Vec<_>, iceberg_rust::spec::error::Error>>()
            .map_err(Error::from)
    }
    /// Check if a table exists
    async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, Error> {
        let configuration = self.configuration.clone();

        match catalog_api_api::view_exists(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .or_else(|_| {
            let configuration = configuration.clone();
            async move {
                catalog_api_api::table_exists(
                    &configuration,
                    self.name.as_deref(),
                    &identifier.namespace().to_string(),
                    identifier.name(),
                )
                .await
            }
        })
        .await
        .map_err(Into::<Error>::into)
        {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_table(&self, identifier: &Identifier) -> Result<(), Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::drop_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            None,
        )
        .await
        .map_err(Into::<Error>::into)?;
        self.vended_stores.evict(&self.vended_store_key(identifier));
        Ok(())
    }
    /// Rename a table through the Iceberg REST catalog's atomic endpoint.
    async fn rename_table(
        &self,
        source: &Identifier,
        destination: &Identifier,
    ) -> Result<(), Error> {
        catalog_api_api::rename_table(
            &self.configuration,
            self.name.as_deref(),
            models::RenameTableRequest::new(source.clone(), destination.clone()),
        )
        .await
        .map_err(Into::<Error>::into)?;
        self.vended_stores.evict(&self.vended_store_key(source));
        Ok(())
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_view(&self, identifier: &Identifier) -> Result<(), Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::drop_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .await
        .map_err(Into::<Error>::into)
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_materialized_view(&self, identifier: &Identifier) -> Result<(), Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::drop_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .await
        .map_err(Into::<Error>::into)
    }
    /// Load a table.
    async fn load_tabular(self: Arc<Self>, identifier: &Identifier) -> Result<Tabular, Error> {
        let configuration = self.configuration.clone();
        // Tables dominate every workload, so try loadTable first and fall
        // back to loadView on 404 — the previous order paid a wasted
        // loadView round-trip (always a 404) for every table load.
        let headers = configuration
            .access_delegation
            .as_ref()
            .map(|value| HashMap::from([("X-Iceberg-Access-Delegation".to_owned(), value.clone())]))
            .unwrap_or_default();
        let table_response = catalog_api_api::load_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            headers,
            None,
        )
        .await;
        match table_response {
            Ok(response) => {
                let object_store = self.get_object_store(identifier, &response)?;

                self.cache
                    .write()
                    .unwrap()
                    .insert(identifier.clone(), object_store.clone());

                let table_metadata = response.metadata;

                Ok(Tabular::Table(
                    Table::new(
                        identifier.clone(),
                        self.clone(),
                        object_store,
                        table_metadata,
                    )
                    .await?,
                ))
            }
            Err(apis::Error::ResponseError(content)) if content.status == 404 => {
                // Not a table: it may be a view or materialized view.
                let view_response = catalog_api_api::load_view(
                    &configuration,
                    self.name.as_deref(),
                    &identifier.namespace().to_string(),
                    identifier.name(),
                )
                .await;
                match view_response.map(|x| x.metadata) {
                    Ok(TabularMetadata::View(view)) => Ok(Tabular::View(
                        View::new(identifier.clone(), self.clone(), view).await?,
                    )),
                    Ok(TabularMetadata::MaterializedView(matview)) => {
                        Ok(Tabular::MaterializedView(
                            MaterializedView::new(identifier.clone(), self.clone(), matview)
                                .await?,
                        ))
                    }
                    Ok(TabularMetadata::Table(_)) => Err(Error::InvalidFormat(
                        "Entity returned from load_view cannot be a table.".to_owned(),
                    )),
                    Err(apis::Error::ResponseError(view_content)) if view_content.status == 404 => {
                        Err(Error::CatalogNotFound)
                    }
                    Err(err) => Err(Into::<Error>::into(err)),
                }
            }
            Err(err) => Err(Into::<Error>::into(err)),
        }
    }
    /// Register a table with the catalog if it doesn't exist.
    async fn create_table(
        self: Arc<Self>,
        identifier: Identifier,
        create_table: CreateTable,
    ) -> Result<Table, Error> {
        let configuration = self.configuration.clone();
        let response = catalog_api_api::create_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            create_table,
            configuration.access_delegation.as_deref(),
        )
        .map_err(Into::<Error>::into)
        .await?;

        let object_store = self.get_object_store(&identifier, &response)?;

        self.cache
            .write()
            .unwrap()
            .insert(identifier.clone(), object_store.clone());

        Table::new(identifier.clone(), self, object_store, response.metadata).await
    }
    /// Update a table by atomically changing the pointer to the metadata file
    async fn update_table(
        self: Arc<Self>,
        commit: iceberg_rust::catalog::commit::CommitTable,
    ) -> Result<Table, Error> {
        let configuration = self.configuration.clone();
        let identifier = commit.identifier.clone();
        let response = catalog_api_api::update_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .await
        .map_err(Into::<Error>::into)?;

        let Some(object_store) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(Error::NotFound(format!(
                "Object store for table {}",
                &identifier
            )));
        };

        Table::new(identifier, self, object_store, response.metadata).await
    }
    async fn create_view(
        self: Arc<Self>,
        identifier: Identifier,
        create_view: CreateView<Option<()>>,
    ) -> Result<View, Error> {
        let configuration = self.configuration.clone();
        catalog_api_api::create_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            create_view,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            async move {
                if let TabularMetadata::View(metadata) = response.metadata {
                    View::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create view didn't return view metadata.".to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn update_view(self: Arc<Self>, commit: CommitView<Option<()>>) -> Result<View, Error> {
        let configuration = self.configuration.clone();
        let identifier = commit.identifier.clone();
        catalog_api_api::replace_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            let identifier = identifier.clone();
            async move {
                if let TabularMetadata::View(metadata) = response.metadata {
                    View::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create view didn't return view metadata.".to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn create_materialized_view(
        self: Arc<Self>,
        identifier: Identifier,
        create_view: CreateMaterializedView,
    ) -> Result<MaterializedView, Error> {
        let configuration = self.configuration.clone();
        let (create_view, mut create_table) = create_view.into();
        create_table.name.clone_from(&create_view.name);
        catalog_api_api::create_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            create_table,
            None,
        )
        .map_err(Into::<Error>::into)
        .await?;
        catalog_api_api::create_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            create_view,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            async move {
                if let TabularMetadata::MaterializedView(metadata) = response.metadata {
                    MaterializedView::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create materialzied view didn't return materialized view metadata."
                            .to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn update_materialized_view(
        self: Arc<Self>,
        commit: CommitView<Identifier>,
    ) -> Result<MaterializedView, Error> {
        let configuration = self.configuration.clone();
        let identifier = commit.identifier.clone();
        catalog_api_api::replace_view(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            let identifier = identifier.clone();
            async move {
                if let TabularMetadata::MaterializedView(metadata) = response.metadata {
                    MaterializedView::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create materialzied view didn't return materialized view metadata."
                            .to_owned(),
                    ))
                }
            }
        })
        .await
    }
    /// Register a table with the catalog if it doesn't exist.
    async fn register_table(
        self: Arc<Self>,
        identifier: Identifier,
        metadata_location: &str,
    ) -> Result<Table, Error> {
        let configuration = self.configuration.clone();
        let request = models::RegisterTableRequest::new(
            identifier.name().to_owned(),
            metadata_location.to_owned(),
        );

        let response = catalog_api_api::register_table(
            &configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            request,
        )
        .map_err(Into::<Error>::into)
        .await?;
        let object_store = self.get_object_store(&identifier, &response)?;

        Table::new(identifier.clone(), self, object_store, response.metadata).await
    }
}

#[derive(Debug, Clone)]
pub struct RestCatalogList {
    configuration: Configuration,
    object_store_builder: Option<ObjectStoreBuilder>,
    ignore_storage_credentials: bool,
    vended_stores: Arc<VendedStoreCache<VendedStoreKey>>,
}

impl RestCatalogList {
    pub fn new(
        configuration: Configuration,
        object_store_builder: Option<ObjectStoreBuilder>,
        ignore_storage_credentials: bool,
    ) -> Self {
        Self {
            configuration,
            object_store_builder,
            ignore_storage_credentials,
            vended_stores: Arc::default(),
        }
    }

    /// See [`RestCatalog::with_store_client_config`]. Catalogs returned by this list share one
    /// vended-store cache, keyed by catalog name and table identifier.
    pub fn with_store_client_config(mut self, client: StoreClientConfig) -> Self {
        self.vended_stores = Arc::new(VendedStoreCache::new(client));
        self
    }
}

#[async_trait]
impl CatalogList for RestCatalogList {
    fn catalog(&self, name: &str) -> Option<Arc<dyn Catalog>> {
        Some(Arc::new(
            RestCatalog::new(
                Some(name),
                self.configuration.clone(),
                self.object_store_builder.clone(),
                self.ignore_storage_credentials,
            )
            .with_vended_stores(self.vended_stores.clone()),
        ))
    }
    async fn list_catalogs(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
pub struct RestNoPrefixCatalogList {
    name: String,
    configuration: Configuration,
    object_store_builder: Option<ObjectStoreBuilder>,
    ignore_storage_credentials: bool,
    vended_stores: Arc<VendedStoreCache<VendedStoreKey>>,
}

impl RestNoPrefixCatalogList {
    pub fn new(
        name: &str,
        configuration: Configuration,
        object_store_builder: Option<ObjectStoreBuilder>,
        ignore_storage_credentials: bool,
    ) -> Self {
        Self {
            name: name.to_owned(),
            configuration,
            object_store_builder,
            ignore_storage_credentials,
            vended_stores: Arc::default(),
        }
    }

    /// See [`RestCatalog::with_store_client_config`]. Catalogs returned by this list share one
    /// vended-store cache.
    pub fn with_store_client_config(mut self, client: StoreClientConfig) -> Self {
        self.vended_stores = Arc::new(VendedStoreCache::new(client));
        self
    }
}

#[async_trait]
impl CatalogList for RestNoPrefixCatalogList {
    fn catalog(&self, name: &str) -> Option<Arc<dyn Catalog>> {
        if self.name == name {
            Some(Arc::new(
                RestCatalog::new(
                    None,
                    self.configuration.clone(),
                    self.object_store_builder.clone(),
                    self.ignore_storage_credentials,
                )
                .with_vended_stores(self.vended_stores.clone()),
            ))
        } else {
            None
        }
    }
    async fn list_catalogs(&self) -> Vec<String> {
        vec![self.name.clone()]
    }
}

/// The storage options a `LoadTableResult` vends, with the prefix its credentials are scoped to.
/// `None` when the response vends nothing and the default object store applies.
fn vended_storage_config(
    response: &models::LoadTableResult,
) -> Option<(HashMap<String, String>, &str)> {
    let credential = response
        .storage_credentials
        .as_ref()
        .and_then(|credentials| credentials.first());
    let config = match (credential, &response.config) {
        (Some(credential), Some(config)) => {
            // Enrich credentials with other options that might only be found in the config (e.g.
            // a custom endpoint)
            let mut options = credential.config.clone();
            options.extend(config.clone());
            options
        }
        (Some(credential), None) => credential.config.clone(),
        (None, Some(config)) => config.clone(),
        (None, None) => return None,
    };
    let prefix = credential.map_or("", |credential| credential.prefix.as_str());
    Some((config, prefix))
}

#[cfg(test)]
pub mod tests {
    use datafusion::{
        arrow::array::{Float64Array, Int64Array},
        common::tree_node::{TransformedResult, TreeNode},
        execution::SessionStateBuilder,
        prelude::SessionContext,
    };
    use datafusion_iceberg::{
        catalog::catalog::IcebergCatalog,
        planner::{iceberg_transform, IcebergQueryPlanner},
    };
    use iceberg_rust::{
        catalog::{identifier::Identifier, namespace::Namespace, Catalog},
        object_store::ObjectStoreBuilder,
        spec::{
            schema::Schema,
            types::{PrimitiveType, StructField, StructType, Type},
        },
        table::Table,
        test_utils::is_podman,
    };
    use object_store::{memory::InMemory, ObjectStore, ObjectStoreExt};
    use std::{convert::TryFrom, sync::Arc, time::Duration};
    use testcontainers::{
        core::{wait::LogWaitStrategy, ExecCommand, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };
    use testcontainers_modules::localstack::LocalStack;
    use tokio::time::sleep;

    use crate::{apis::configuration::Configuration, catalog::RestCatalog};

    fn configuration(url: &str) -> Configuration {
        Configuration {
            base_path: url.to_owned(),
            user_agent: None,
            client: reqwest::Client::new(),
            access_delegation: None,
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: None,
            aws_v4_key: None,
        }
    }

    #[tokio::test]
    async fn namespace_exists_uses_status_for_bodyless_head_response() {
        for (response_status, expected) in [
            ("200 OK", Some(true)),
            ("404 Not Found", Some(false)),
            ("403 Forbidden", None),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                use std::io::{Read, Write};

                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("local REST test server did not accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0_u8; 1024];
                let size = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..size])
                    .starts_with("HEAD /v1/catalog/namespaces/PUBLIC "));
                write!(
                    stream,
                    "HTTP/1.1 {response_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            });
            let catalog = RestCatalog::new(
                Some("catalog"),
                configuration(&format!("http://{address}")),
                None,
                false,
            );
            let namespace = Namespace::try_new(&["PUBLIC".to_string()]).unwrap();
            let result = catalog.namespace_exists(&namespace).await;
            server.join().unwrap();
            match expected {
                Some(exists) => assert_eq!(result.unwrap(), exists),
                None => assert!(result.is_err()),
            }
        }
    }

    /// Serve one canned HTTP response per connection, in order, on a local port. Each response
    /// closes its connection, so every client request lands on the next response.
    fn serve_responses(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        let (address, server) = serve_requests(responses);
        let server = std::thread::spawn(move || {
            server
                .join()
                .unwrap()
                .into_iter()
                .map(|request| request.lines().next().unwrap_or_default().to_string())
                .collect()
        });
        (address, server)
    }

    /// Like [`serve_responses`], but returns each full request head (request line and headers).
    fn serve_requests(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};

            let mut request_lines = Vec::new();
            for response in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("local REST test server did not accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let size = stream.read(&mut buffer).unwrap();
                    assert!(size > 0, "client closed before sending a request");
                    request.extend_from_slice(&buffer[..size]);
                }
                request_lines.push(String::from_utf8_lossy(&request).into_owned());
                stream.write_all(response.as_bytes()).unwrap();
            }
            request_lines
        });
        (address, server)
    }

    fn http_response(status_line: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn load_tabular_error(
        responses: Vec<String>,
    ) -> (iceberg_rust::error::Error, Vec<String>) {
        let (address, server) = serve_responses(responses);
        let catalog = Arc::new(RestCatalog::new(
            Some("catalog"),
            configuration(&format!("http://{address}")),
            None,
            false,
        ));
        let identifier = Identifier::new(&["PUBLIC".to_string()], "T");
        let error = match catalog.load_tabular(&identifier).await {
            Ok(_) => panic!("load_tabular unexpectedly succeeded"),
            Err(error) => error,
        };
        (error, server.join().unwrap())
    }

    const REST_ERROR_BODY: &str =
        r#"{"error":{"message":"denied","type":"RESTException","code":0}}"#;

    #[tokio::test]
    async fn load_tabular_429_carries_status_and_retry_after_seconds() {
        let (error, requests) = load_tabular_error(vec![http_response(
            "429 Too Many Requests",
            "Retry-After: 2\r\n",
            REST_ERROR_BODY,
        )])
        .await;
        assert_eq!(
            requests,
            ["GET /v1/catalog/namespaces/PUBLIC/tables/T HTTP/1.1"]
        );
        assert!(
            matches!(error, iceberg_rust::error::Error::HttpResponse(_)),
            "{error:?}"
        );
        assert_eq!(error.http_status(), Some(429));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
        assert_eq!(
            error.to_string(),
            format!("Response status: 429 Too Many Requests, Response content: {REST_ERROR_BODY} doesn't have the right format")
        );
    }

    #[tokio::test]
    async fn load_tabular_503_carries_retry_after_http_date() {
        let retry_at = chrono::Utc::now() + chrono::Duration::seconds(120);
        let retry_after = format!(
            "Retry-After: {}\r\n",
            retry_at.format("%a, %d %b %Y %H:%M:%S GMT")
        );
        let (error, _) = load_tabular_error(vec![http_response(
            "503 Service Unavailable",
            &retry_after,
            "",
        )])
        .await;
        assert!(matches!(error, iceberg_rust::error::Error::HttpResponse(_)));
        assert_eq!(error.http_status(), Some(503));
        let delay = error
            .retry_after()
            .expect("Retry-After HTTP-date is parsed");
        assert!(
            delay > Duration::from_secs(100) && delay <= Duration::from_secs(120),
            "{delay:?}"
        );
    }

    #[tokio::test]
    async fn load_tabular_401_carries_status_without_retry_after() {
        let (error, _) =
            load_tabular_error(vec![http_response("401 Unauthorized", "", REST_ERROR_BODY)]).await;
        assert!(matches!(error, iceberg_rust::error::Error::HttpResponse(_)));
        assert_eq!(error.http_status(), Some(401));
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test]
    async fn load_tabular_404_keeps_catalog_not_found() {
        let not_found = http_response("404 Not Found", "", REST_ERROR_BODY);
        let (error, requests) = load_tabular_error(vec![not_found.clone(), not_found]).await;
        assert_eq!(
            requests,
            [
                "GET /v1/catalog/namespaces/PUBLIC/tables/T HTTP/1.1",
                "GET /v1/catalog/namespaces/PUBLIC/views/T HTTP/1.1",
            ]
        );
        assert!(
            matches!(error, iceberg_rust::error::Error::CatalogNotFound),
            "{error:?}"
        );
        assert_eq!(error.http_status(), None);
    }

    #[tokio::test]
    async fn load_table_api_404_exposes_status() {
        let (address, server) = serve_responses(vec![http_response(
            "404 Not Found",
            "Retry-After: 7\r\n",
            REST_ERROR_BODY,
        )]);
        let error = crate::apis::catalog_api_api::load_table(
            &configuration(&format!("http://{address}")),
            Some("catalog"),
            "PUBLIC",
            "T",
            Default::default(),
            None,
        )
        .await
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.status(), Some(reqwest::StatusCode::NOT_FOUND));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
        let error: iceberg_rust::error::Error = error.into();
        assert!(matches!(error, iceberg_rust::error::Error::NotFound(_)));
    }

    /// A `LoadTableResult` body for an S3 table at `location` whose vended credentials are
    /// `key_id` / `token`, served by the S3 endpoint at `endpoint`.
    fn vended_load_table_body(location: &str, endpoint: &str, key_id: &str, token: &str) -> String {
        use iceberg_rust::spec::{
            schema::SchemaBuilder,
            table_metadata::{FormatVersion, TableMetadataBuilder},
        };
        let schema = SchemaBuilder::default()
            .with_schema_id(0)
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
        let metadata = TableMetadataBuilder::default()
            .format_version(FormatVersion::V2)
            .location(location.to_string())
            .current_schema_id(0)
            .schemas(std::collections::HashMap::from_iter([(0, schema)]))
            .build()
            .unwrap();
        let owned = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let result = crate::models::LoadTableResult {
            metadata_location: Some(format!("{location}/metadata/v1.metadata.json")),
            metadata,
            config: Some(owned(&[
                ("s3.endpoint", endpoint),
                ("client.region", "us-east-1"),
            ])),
            storage_credentials: Some(vec![crate::models::StorageCredential::new(
                location.to_string(),
                owned(&[
                    ("s3.access-key-id", key_id),
                    ("s3.secret-access-key", "vended-secret"),
                    ("s3.session-token", token),
                    // Expiry changes on every vend and must not force a new store.
                    (
                        "s3.session-token-expires-at-ms",
                        &format!("{}", token.len()),
                    ),
                ]),
            )]),
        };
        http_response("200 OK", "", &serde_json::to_string(&result).unwrap())
    }

    fn s3_not_found() -> String {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    }

    async fn load_table(catalog: &Arc<RestCatalog>, name: &str) -> Table {
        let identifier = Identifier::new(&["PUBLIC".to_string()], name);
        match catalog.clone().load_tabular(&identifier).await.unwrap() {
            iceberg_rust::catalog::tabular::Tabular::Table(table) => table,
            _ => panic!("expected a table"),
        }
    }

    fn same_store(left: &Arc<dyn ObjectStore>, right: &Arc<dyn ObjectStore>) -> bool {
        std::ptr::eq(
            Arc::as_ptr(left) as *const (),
            Arc::as_ptr(right) as *const (),
        )
    }

    /// Issue one GET through `store` and return the signed request head the S3 mock received.
    async fn signed_get(
        store: &Arc<dyn ObjectStore>,
        s3: std::thread::JoinHandle<Vec<String>>,
    ) -> String {
        let result = store
            .get(&object_store::path::Path::from("t/data/file.parquet"))
            .await;
        assert!(
            matches!(result, Err(object_store::Error::NotFound { .. })),
            "{result:?}"
        );
        let requests = s3.join().unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        requests[0].to_ascii_lowercase()
    }

    #[tokio::test]
    async fn reload_same_location_reuses_store() {
        let (s3, _s3_server) = serve_requests(vec![]);
        let endpoint = format!("http://{s3}");
        let (rest, rest_server) = serve_responses(vec![
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY1", "token-1"),
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY2", "token-2"),
        ]);
        let catalog = Arc::new(RestCatalog::new(
            Some("catalog"),
            configuration(&format!("http://{rest}")),
            None,
            false,
        ));
        let first = load_table(&catalog, "T").await;
        let second = load_table(&catalog, "T").await;
        rest_server.join().unwrap();
        assert!(same_store(&first.object_store(), &second.object_store()));
    }

    #[tokio::test]
    async fn reload_swaps_credentials() {
        let (s3, s3_server) = serve_requests(vec![s3_not_found()]);
        let endpoint = format!("http://{s3}");
        let (rest, rest_server) = serve_responses(vec![
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY1", "token-1"),
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY2", "token-2"),
        ]);
        let catalog = Arc::new(RestCatalog::new(
            Some("catalog"),
            configuration(&format!("http://{rest}")),
            None,
            false,
        ));
        let first = load_table(&catalog, "T").await;
        let _second = load_table(&catalog, "T").await;
        rest_server.join().unwrap();
        // The table loaded first signs its next request with the credentials vended last.
        let request = signed_get(&first.object_store(), s3_server).await;
        assert!(
            request.starts_with("get /bucket/t/data/file.parquet "),
            "{request}"
        );
        assert!(request.contains("credential=key2/"), "{request}");
        assert!(
            request.contains("x-amz-security-token: token-2"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn different_locations_do_not_share_store() {
        let (s3, s3_server) = serve_requests(vec![s3_not_found(), s3_not_found()]);
        let endpoint = format!("http://{s3}");
        // Two tables, even at the same location, never share a store or its credentials.
        let (rest, rest_server) = serve_responses(vec![
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY_A", "token-a"),
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY_B", "token-b"),
            vended_load_table_body("s3://bucket/u", &endpoint, "KEY_A", "token-a"),
        ]);
        let catalog = Arc::new(RestCatalog::new(
            Some("catalog"),
            configuration(&format!("http://{rest}")),
            None,
            false,
        ));
        let a = load_table(&catalog, "A").await;
        let b = load_table(&catalog, "B").await;
        // A moves to a new location: its store is rebuilt, not reused.
        let a_moved = load_table(&catalog, "A").await;
        rest_server.join().unwrap();
        assert!(!same_store(&a.object_store(), &b.object_store()));
        assert!(!same_store(&a.object_store(), &a_moved.object_store()));

        let a_store = a.object_store();
        let b_store = b.object_store();
        a_store
            .get(&object_store::path::Path::from("t/data/file.parquet"))
            .await
            .unwrap_err();
        b_store
            .get(&object_store::path::Path::from("t/data/file.parquet"))
            .await
            .unwrap_err();
        let requests: Vec<String> = s3_server
            .join()
            .unwrap()
            .into_iter()
            .map(|request| request.to_ascii_lowercase())
            .collect();
        assert!(requests[0].contains("credential=key_a/"), "{requests:?}");
        assert!(requests[0].contains("x-amz-security-token: token-a"));
        assert!(requests[1].contains("credential=key_b/"), "{requests:?}");
        assert!(requests[1].contains("x-amz-security-token: token-b"));
    }

    #[tokio::test]
    async fn vended_stores_share_one_http_client() {
        use iceberg_rust::object_store::vended::{SharedHttpConnector, StoreClientConfig};

        let (s3, _s3_server) = serve_requests(vec![]);
        let endpoint = format!("http://{s3}");
        let (rest, rest_server) = serve_responses(vec![
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY1", "token-1"),
            vended_load_table_body("s3://bucket/u", &endpoint, "KEY2", "token-2"),
            vended_load_table_body("s3://bucket/t", &endpoint, "KEY3", "token-3"),
        ]);
        let connector = Arc::new(SharedHttpConnector::new());
        let catalog = Arc::new(
            RestCatalog::new(
                Some("catalog"),
                configuration(&format!("http://{rest}")),
                None,
                false,
            )
            .with_store_client_config(
                StoreClientConfig::new()
                    .with_client_options(
                        object_store::ClientOptions::new()
                            .with_timeout(Duration::from_secs(10))
                            .with_connect_timeout(Duration::from_secs(3)),
                    )
                    .with_http_connector(connector.clone()),
            ),
        );
        let t = load_table(&catalog, "T").await;
        let u = load_table(&catalog, "U").await;
        let t_reloaded = load_table(&catalog, "T").await;
        rest_server.join().unwrap();

        assert!(!same_store(&t.object_store(), &u.object_store()));
        assert!(same_store(&t.object_store(), &t_reloaded.object_store()));
        // Two tables -> two stores; the reload swapped credentials instead of building.
        assert_eq!(catalog.vended_store_builds(), 2);
        // Both stores came from one HTTP client (one connection pool).
        assert_eq!(connector.clients_created(), 1);
        let store = format!("{:?}", t.object_store());
        assert!(store.contains("timeout: Some(Parsed(10s))"), "{store}");
        assert!(
            store.contains("connect_timeout: Some(Parsed(3s))"),
            "{store}"
        );
    }

    #[tokio::test]
    async fn vended_store_requests_use_embedder_timeout() {
        use iceberg_rust::object_store::vended::StoreClientConfig;

        // An S3 endpoint that accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let s3 = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(4).collect();
            std::thread::sleep(Duration::from_secs(10));
            drop(held);
        });
        let endpoint = format!("http://{s3}");
        let (rest, rest_server) = serve_responses(vec![vended_load_table_body(
            "s3://bucket/t",
            &endpoint,
            "KEY1",
            "token-1",
        )]);
        let catalog = Arc::new(
            RestCatalog::new(
                Some("catalog"),
                configuration(&format!("http://{rest}")),
                None,
                false,
            )
            .with_store_client_config(
                StoreClientConfig::new()
                    .with_client_options(
                        object_store::ClientOptions::new().with_timeout(Duration::from_millis(200)),
                    )
                    .with_retry_config(object_store::RetryConfig {
                        max_retries: 1,
                        retry_timeout: Duration::from_secs(2),
                        backoff: object_store::BackoffConfig {
                            init_backoff: Duration::from_millis(10),
                            max_backoff: Duration::from_millis(10),
                            base: 2.,
                        },
                    }),
            ),
        );
        let table = load_table(&catalog, "T").await;
        rest_server.join().unwrap();

        let started = std::time::Instant::now();
        let result = table
            .object_store()
            .get(&object_store::path::Path::from("t/data/file.parquet"))
            .await;
        let elapsed = started.elapsed();
        assert!(result.is_err(), "{result:?}");
        // object_store's default 30 s request timeout would hold this request far longer.
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?} {result:?}");
    }

    #[tokio::test]
    async fn test_create_update_drop_table() {
        let container_host = if is_podman() {
            "host.containers.internal"
        } else {
            "172.17.0.1"
        };

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

        let rest = GenericImage::new("apache/iceberg-rest-fixture", "latest")
            .with_wait_for(WaitFor::Log(LogWaitStrategy::stderr(
                "INFO org.eclipse.jetty.server.Server - Started ",
            )))
            .with_env_var("AWS_REGION", "us-east-1")
            .with_env_var("AWS_ACCESS_KEY_ID", "user")
            .with_env_var("AWS_SECRET_ACCESS_KEY", "password")
            .with_env_var("CATALOG_WAREHOUSE", "s3://warehouse/")
            .with_env_var("CATALOG_IO__IMPL", "org.apache.iceberg.aws.s3.S3FileIO")
            .with_env_var(
                "CATALOG_S3_ENDPOINT",
                format!("http://{}:{}", container_host, localstack_port),
            )
            .with_env_var("CATALOG_S3_PATH__STYLE__ACCESS", "true")
            .start()
            .await
            .unwrap();

        let rest_host = rest.get_host().await.unwrap();
        let rest_port = rest.get_host_port_ipv4(8181).await.unwrap();

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

        let iceberg_catalog = Arc::new(RestCatalog::new(
            None,
            configuration(&format!("http://{rest_host}:{rest_port}")),
            Some(object_store),
            false,
        ));

        iceberg_catalog
            .create_namespace(&Namespace::try_new(&["tpch".to_owned()]).unwrap(), None)
            .await
            .expect("Failed to create namespace");

        let catalog = Arc::new(
            IcebergCatalog::new(iceberg_catalog.clone(), None)
                .await
                .unwrap(),
        );

        let state = SessionStateBuilder::new()
            .with_default_features()
            .with_query_planner(Arc::new(IcebergQueryPlanner::new()))
            .build();

        let ctx = SessionContext::new_with_state(state);

        ctx.register_catalog("warehouse", catalog);

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
    L_COMMENT VARCHAR NOT NULL ) STORED AS ICEBERG LOCATION 's3://warehouse/tpch/lineitem' PARTITIONED BY ( \"month(L_SHIPDATE)\" );";

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

        assert_eq!(
            iceberg_catalog
                .tabular_exists(&Identifier::new(&["tpch".to_owned()], "lineitem"))
                .await
                .map_err(|s| s.to_string()),
            Ok(true)
        );
        assert_eq!(
            iceberg_catalog
                .tabular_exists(&Identifier::new(&["tpch".to_owned()], "non_existing_table"))
                .await
                .map_err(|s| s.to_string()),
            Ok(false)
        );

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
    }

    // -----------------------------------------------------------------------
    // Placeholders for the upstream REST catalog suite. iceberg-rest-catalog
    // wires a `RestCatalog` impl, but doesn't surface most upstream features
    // (session caches, view-support detection, server-side scan planning,
    // OAuth + auth-session caching, remote-sign, metric reporters, ETag).
    // -----------------------------------------------------------------------

    use rstest::rstest;

    // -- TestRESTCatalog (72) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[case(35)]
    #[case(36)]
    #[case(37)]
    #[case(38)]
    #[case(39)]
    #[case(40)]
    #[case(41)]
    #[case(42)]
    #[case(43)]
    #[case(44)]
    #[case(45)]
    #[case(46)]
    #[case(47)]
    #[case(48)]
    #[case(49)]
    #[case(50)]
    #[case(51)]
    #[case(52)]
    #[case(53)]
    #[case(54)]
    #[case(55)]
    #[case(56)]
    #[case(57)]
    #[case(58)]
    #[case(59)]
    #[case(60)]
    #[case(61)]
    #[case(62)]
    #[case(63)]
    #[case(64)]
    #[case(65)]
    #[case(66)]
    #[case(67)]
    #[case(68)]
    #[case(69)]
    #[case(70)]
    #[case(71)]
    #[case(72)]
    #[ignore = "upstream REST catalog suite covers tables + views + namespaces + table-load-credentials + token-refresh + view-support detection across 72 @TestTemplate methods"]
    fn test_rest_catalog_suite_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRESTCatalog");
    }

    // -- TestRESTViewCatalog (5) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[ignore = "no REST view catalog endpoints fully implemented"]
    fn test_rest_view_catalog_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRESTViewCatalog");
    }

    // -- TestRESTScanPlanning (34) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[ignore = "no server-side scan-planning protocol implementation"]
    fn test_rest_scan_planning_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRESTScanPlanning");
    }

    // -- TestRESTTableCache (9) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[ignore = "no REST table cache (per-session caching + refresh)"]
    fn test_rest_table_cache_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRESTTableCache");
    }

    // -- TestRESTUtil (14) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[ignore = "no RESTUtil helpers (URL encoding of namespaces / identifiers)"]
    fn test_rest_util_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRESTUtil");
    }

    // -- TestResourcePaths (30) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[ignore = "no ResourcePaths construction helper"]
    fn test_resource_paths_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestResourcePaths");
    }

    // -- TestEndpoint (6) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[ignore = "no endpoint discovery from /v1/config"]
    fn test_endpoint_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestEndpoint");
    }

    // -- TestConfigResponse + TestConfigResponseParser (6+10=16) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[ignore = "ConfigResponse + parser: 6 + 10 scenarios covering server config response shape"]
    fn test_config_response_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestConfigResponse + Parser");
    }

    // -- TestCreateTableRequest (4) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[ignore = "CreateTableRequest JSON body shape"]
    fn test_create_table_request_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestCreateTableRequest");
    }

    // -- TestCommitTransactionRequestParser (5) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[ignore = "CommitTransactionRequest JSON body shape"]
    fn test_commit_transaction_request_parser_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestCommitTransactionRequestParser");
    }

    // -- TestLoadTableResponse + Parser (6+7=13) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[ignore = "LoadTableResponse + parser: 6 + 7 scenarios (storage credentials, table metadata)"]
    fn test_load_table_response_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestLoadTableResponse + Parser");
    }

    // -- TestLoadViewResponseParser (4) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[ignore = "LoadViewResponse parser (existing cycle covered this in iceberg-rust-spec for the inner metadata; REST-side request still gap)"]
    fn test_load_view_response_parser_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestLoadViewResponseParser");
    }

    // -- TestUpdateTableRequestParser (7) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[ignore = "UpdateTableRequest JSON shape (assertions + updates)"]
    fn test_update_table_request_parser_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestUpdateTableRequestParser");
    }

    // -- TestRegisterTableRequestParser + TestRegisterViewRequestParser (4+3=7) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[ignore = "Register table / view request body shape"]
    fn test_register_request_parser_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRegisterTable + ViewRequestParser");
    }

    // -- TestRenameTableRequest (3) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[ignore = "Rename request body shape"]
    fn test_rename_table_request_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestRenameTableRequest");
    }

    // -- Namespace request/response parsers (4+4+4+5+5+4+4 = 30) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[ignore = "CreateNamespace, GetNamespace, ListNamespaces, ListTables, UpdateNamespaceProperties (request + response): 30 scenarios"]
    fn test_namespace_request_response_scenarios(#[case] _scenario: usize) {
        unimplemented!("Namespace request/response parsers");
    }

    // -- TestCatalogErrorResponseParser (5) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[ignore = "CatalogErrorResponse parser (apis::Error mapping)"]
    fn test_catalog_error_response_parser_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestCatalogErrorResponseParser");
    }

    // -- TestHTTPClient + TestHTTPHeaders + TestHTTPRequest (26+7+9=42) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[case(35)]
    #[case(36)]
    #[case(37)]
    #[case(38)]
    #[case(39)]
    #[case(40)]
    #[case(41)]
    #[case(42)]
    #[ignore = "low-level HTTPClient + Headers + Request behaviour: 26 + 7 + 9 scenarios"]
    fn test_http_client_headers_request_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestHTTPClient + Headers + Request");
    }

    // -- TestExponentialHttpRequestRetryStrategy (20) --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[ignore = "no exponential HTTP retry/backoff strategy"]
    fn test_exponential_http_retry_scenarios(#[case] _scenario: usize) {
        unimplemented!("TestExponentialHttpRequestRetryStrategy");
    }

    // -- Authentication suite: AuthManagers (7), AuthSessionCache (2), BasicAuthManager (3),
    //    OAuth2Manager (23), OAuth2Util (7), OAuthTokenResponse (2),
    //    OAuthErrorResponseParser (3), DefaultAuthSession (2) = 49 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[case(35)]
    #[case(36)]
    #[case(37)]
    #[case(38)]
    #[case(39)]
    #[case(40)]
    #[case(41)]
    #[case(42)]
    #[case(43)]
    #[case(44)]
    #[case(45)]
    #[case(46)]
    #[case(47)]
    #[case(48)]
    #[case(49)]
    #[ignore = "authentication suite (AuthManagers, AuthSessionCache, BasicAuth, OAuth2Manager, OAuth2Util, OAuthTokenResponse + ErrorResponse parsers, DefaultAuthSession)"]
    fn test_authentication_suite_scenarios(#[case] _scenario: usize) {
        unimplemented!("Authentication suite");
    }

    // -- LoadCredentialsResponseParser (3) + StorageCredential (3) = 6 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[ignore = "LoadCredentialsResponse parser + StorageCredential"]
    fn test_storage_credentials_scenarios(#[case] _scenario: usize) {
        unimplemented!("LoadCredentialsResponse + StorageCredential");
    }

    // -- Remote sign: TestRemoteSignRequestParser (9) + TestRemoteSignResponseParser (4) = 13 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[ignore = "no S3 remote-sign request/response parsers"]
    fn test_remote_sign_scenarios(#[case] _scenario: usize) {
        unimplemented!("Remote sign request/response");
    }

    // -- Remote scan planning: TestRemoteScanPlanning (1) + TestFetchPlanningResultResponseParser (13) +
    //    TestFetchScanTasksRequest (2) + TestFetchScanTasksResponseParser (5) +
    //    TestPlanTableScanRequestParser (13) + TestPlanTableScanResponseParser (20) = 54 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[case(35)]
    #[case(36)]
    #[case(37)]
    #[case(38)]
    #[case(39)]
    #[case(40)]
    #[case(41)]
    #[case(42)]
    #[case(43)]
    #[case(44)]
    #[case(45)]
    #[case(46)]
    #[case(47)]
    #[case(48)]
    #[case(49)]
    #[case(50)]
    #[case(51)]
    #[case(52)]
    #[case(53)]
    #[case(54)]
    #[ignore = "no remote scan planning (long-running scan-plan polling + plan/fetch parsers)"]
    fn test_remote_scan_planning_suite_scenarios(#[case] _scenario: usize) {
        unimplemented!("Remote scan planning suite");
    }

    // -- TestErrorHandlers (7) + TestETagProvider (4) = 11 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[ignore = "no ErrorHandlers + ETagProvider"]
    fn test_error_handlers_and_etag_scenarios(#[case] _scenario: usize) {
        unimplemented!("ErrorHandlers + ETagProvider");
    }

    // -- Metric reporting suite: TestCommitReporting (3) + TestScanReport (4) + ScanReportParser (11) +
    //    ScanMetricsResultParser (7) + ReportMetricsRequestParser (5) + MetricsReporters (7) +
    //    CacheMetricsReport (2) + CounterResultParser (6) + TimerResultParser (9) = 54 --
    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[case(5)]
    #[case(6)]
    #[case(7)]
    #[case(8)]
    #[case(9)]
    #[case(10)]
    #[case(11)]
    #[case(12)]
    #[case(13)]
    #[case(14)]
    #[case(15)]
    #[case(16)]
    #[case(17)]
    #[case(18)]
    #[case(19)]
    #[case(20)]
    #[case(21)]
    #[case(22)]
    #[case(23)]
    #[case(24)]
    #[case(25)]
    #[case(26)]
    #[case(27)]
    #[case(28)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(32)]
    #[case(33)]
    #[case(34)]
    #[case(35)]
    #[case(36)]
    #[case(37)]
    #[case(38)]
    #[case(39)]
    #[case(40)]
    #[case(41)]
    #[case(42)]
    #[case(43)]
    #[case(44)]
    #[case(45)]
    #[case(46)]
    #[case(47)]
    #[case(48)]
    #[case(49)]
    #[case(50)]
    #[case(51)]
    #[case(52)]
    #[case(53)]
    #[case(54)]
    #[ignore = "no metric reporting suite (CommitReporting, ScanReport + parser, MetricsReporters, Counter/Timer result parsers, ReportMetricsRequest parser)"]
    fn test_metric_reporting_suite_scenarios(#[case] _scenario: usize) {
        unimplemented!("Metric reporting suite");
    }
}
