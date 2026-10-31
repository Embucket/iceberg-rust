use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use iceberg_rust::{
    catalog::{
        commit::{
            apply_table_updates, apply_view_updates, check_table_requirements,
            check_view_requirements, CommitTable, CommitView, TableRequirement,
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
        table_metadata::{new_metadata_location, MetadataLog, TableMetadata},
        tabular::TabularMetadata,
        util::strip_prefix,
        view_metadata::ViewMetadata,
    },
    table::Table,
    view::View,
};
use object_store::ObjectStoreExt;
use sqlx::{
    any::{install_default_drivers, AnyPoolOptions, AnyRow},
    pool::PoolOptions,
    AnyPool, Executor, Row,
};

use crate::error::Error;

/// A SQL-backed catalog with namespace identities stored separately from properties.
///
/// Property-only namespaces from older versions remain readable and are promoted
/// when updated. Stop older writable catalog instances before upgrading: they do
/// not recognize the new identity rows, so mixed-version writes or a downgrade
/// can lose namespace visibility or leave stale identities after a drop.
#[derive(Debug)]
pub struct SqlCatalog {
    name: String,
    pool: AnyPool,
    object_store: ObjectStoreBuilder,
    cache: Arc<RwLock<HashMap<Identifier, (String, TabularMetadata)>>>,
    #[cfg(test)]
    namespace_create_barrier: Option<Arc<tokio::sync::Barrier>>,
    #[cfg(test)]
    namespace_mutation_pause: Option<Arc<NamespaceMutationPause>>,
}

#[cfg(test)]
#[derive(Debug)]
struct NamespaceMutationPause {
    namespace: String,
    continuation: std::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

#[cfg(test)]
impl NamespaceMutationPause {
    async fn after_missing_identity(&self, namespace: &str) {
        if self.namespace != namespace {
            return;
        }
        let continuation = self.continuation.lock().unwrap().take();
        if let Some((entered, resume)) = continuation {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }
}

#[derive(Clone, Copy)]
enum NamespaceState {
    Current,
    Legacy,
    Missing,
}

pub mod error;

/// How a catalog connects to its database.
///
/// The catalog opens its pool through sqlx's `Any` driver, which takes a URL
/// and nothing else, so a caller had no way to size the pool or to configure
/// the sessions on it. Both matter under load: the pool's defaults let ten
/// writers contend on a database that may serialize them, and settings such as
/// SQLite's `busy_timeout` or PostgreSQL's `statement_timeout` are per-session
/// and cannot be carried on the URL — sqlx's SQLite URL parser rejects them as
/// query parameters outright.
#[derive(Debug, Clone, Default)]
pub struct SqlCatalogOptions {
    pool: Option<PoolOptions<sqlx::Any>>,
    session_statements: Vec<String>,
}

impl SqlCatalogOptions {
    /// Options that reproduce the defaults: sqlx's pool settings, except that a
    /// private in-memory SQLite database is held to a single connection, since
    /// a second connection would open a different database.
    pub fn new() -> Self {
        Self::default()
    }

    /// Size and time-bound the connection pool.
    pub fn with_pool_options(mut self, pool: PoolOptions<sqlx::Any>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Statements to run on every new connection before it is used, such as
    /// `pragma busy_timeout = 30000` or `set statement_timeout = '30s'`.
    ///
    /// They run in order, after the catalog's own tables are ensured, and an
    /// error from any of them fails the connection.
    pub fn with_session_statements(
        mut self,
        statements: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.session_statements = statements.into_iter().map(Into::into).collect();
        self
    }
}

impl SqlCatalog {
    pub async fn new(
        url: &str,
        name: &str,
        object_store: ObjectStoreBuilder,
    ) -> Result<Self, Error> {
        Self::new_with_options(url, name, object_store, SqlCatalogOptions::default()).await
    }

    /// Open a catalog with explicit connection options.
    pub async fn new_with_options(
        url: &str,
        name: &str,
        object_store: ObjectStoreBuilder,
        options: SqlCatalogOptions,
    ) -> Result<Self, Error> {
        install_default_drivers();

        let mut pool_options = options.pool.unwrap_or_default();
        if url == "sqlite://" {
            // A private in-memory SQLite database is only reachable through the
            // connection that created it; a second connection opens a distinct,
            // empty database. Enforce the single-connection cap even when the
            // caller supplied their own pool options.
            pool_options = pool_options.max_connections(1);
        }

        let pool = pool_options_with_setup(
            pool_options,
            url.starts_with("sqlite"),
            options.session_statements,
        )
        .connect_lazy(url)?;

        Ok(SqlCatalog {
            name: name.to_owned(),
            pool,
            object_store,
            cache: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            namespace_create_barrier: None,
            #[cfg(test)]
            namespace_mutation_pause: None,
        })
    }

    pub fn catalog_list(&self) -> Arc<SqlCatalogList> {
        Arc::new(SqlCatalogList {
            pool: self.pool.clone(),
            object_store: self.object_store.clone(),
        })
    }

    fn sql_with_binds(&self, postgres: &'static str, sqlite_mysql: &'static str) -> &'static str {
        match self.pool.connect_options().database_url.scheme() {
            "postgres" | "postgresql" => postgres,
            _ => sqlite_mysql,
        }
    }

    async fn insert_tabular_reference(
        &self,
        identifier: &Identifier,
        metadata_location: &str,
    ) -> Result<(), IcebergError> {
        let namespace = identifier.namespace().to_string();
        sqlx::query(self.sql_with_binds(
            "insert into iceberg_tables (catalog_name, table_namespace, table_name, metadata_location) values ($1, $2, $3, $4)",
            "insert into iceberg_tables (catalog_name, table_namespace, table_name, metadata_location) values (?, ?, ?, ?)",
        ))
        .bind(&self.name)
        .bind(&namespace)
        .bind(identifier.name())
        .bind(metadata_location)
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn delete_tabular_reference(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        let namespace = identifier.namespace().to_string();
        sqlx::query(self.sql_with_binds(
            "delete from iceberg_tables where catalog_name = $1 and table_namespace = $2 and table_name = $3",
            "delete from iceberg_tables where catalog_name = ? and table_namespace = ? and table_name = ?",
        ))
        .bind(&self.name)
        .bind(&namespace)
        .bind(identifier.name())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn lock_namespace_for_mutation(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
        namespace: &str,
    ) -> Result<NamespaceState, IcebergError> {
        let is_sqlite = self.pool.connect_options().database_url.scheme() == "sqlite";
        if is_sqlite {
            // SQLite has no SELECT FOR UPDATE; a write statement takes the writer lock.
            sqlx::query("update iceberg_namespaces set namespace = namespace where 1 = 0")
                .execute(&mut **transaction)
                .await
                .map_err(Error::from)?;
        }
        let row_probe_sql = self.sql_with_binds(
            "select 1 from iceberg_namespaces where catalog_name = $1 and namespace = $2",
            "select 1 from iceberg_namespaces where catalog_name = ? and namespace = ?",
        );
        let row_sql = if is_sqlite {
            "select 1 from iceberg_namespaces where catalog_name = ? and namespace = ?"
        } else {
            self.sql_with_binds(
                "select 1 from iceberg_namespaces where catalog_name = $1 and namespace = $2 for update",
                "select 1 from iceberg_namespaces where catalog_name = ? and namespace = ? for update",
            )
        };
        let property_sql = if is_sqlite {
            "select 1 from iceberg_namespace_properties where catalog_name = ? and namespace = ? order by property_key limit 1"
        } else {
            self.sql_with_binds(
                "select 1 from iceberg_namespace_properties where catalog_name = $1 and namespace = $2 order by property_key limit 1 for update",
                "select 1 from iceberg_namespace_properties where catalog_name = ? and namespace = ? order by property_key limit 1 for update",
            )
        };

        // Probe without a locking read: MySQL can otherwise gap-lock a missing
        // identity row and deadlock two writers promoting the same legacy row.
        let row = sqlx::query(row_probe_sql)
            .bind(&self.name)
            .bind(namespace)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(Error::from)?;
        #[cfg(test)]
        if let (None, Some(pause)) = (&row, &self.namespace_mutation_pause) {
            pause.after_missing_identity(namespace).await;
        }
        if row.is_some() {
            let locked = sqlx::query(row_sql)
                .bind(&self.name)
                .bind(namespace)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(Error::from)?;
            if locked.is_some() {
                return Ok(NamespaceState::Current);
            }
        }
        let property = sqlx::query(property_sql)
            .bind(&self.name)
            .bind(namespace)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(Error::from)?;
        if property.is_none() {
            // A concurrent legacy promotion may have removed the last property
            // while this locking read waited. Recheck its new identity row.
            let row = sqlx::query(row_sql)
                .bind(&self.name)
                .bind(namespace)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(Error::from)?;
            return Ok(if row.is_some() {
                NamespaceState::Current
            } else {
                NamespaceState::Missing
            });
        }
        // The property row serializes promoters of this namespace. A locking
        // read of a missing identity row would gap-lock unrelated namespaces
        // on MySQL's default REPEATABLE READ isolation level.
        Ok(NamespaceState::Legacy)
    }

    fn default_object_store(&self, bucket: Bucket) -> Arc<dyn object_store::ObjectStore> {
        Arc::new(self.object_store.build(bucket).unwrap())
    }

    /// Atomically move the catalog's metadata pointer for `identifier` from
    /// `previous_metadata_location` to `metadata_location`.
    ///
    /// The update only matches while the previous location is still current, so
    /// a writer that committed first leaves this update matching no rows. That
    /// outcome is a lost commit, not a success: the new metadata file exists in
    /// the object store but nothing references it. Report it as a conflict and
    /// drop the cache entry, whose metadata is now known to be stale, so the
    /// next load refreshes from the catalog.
    async fn swap_metadata_location(
        &self,
        identifier: &Identifier,
        previous_metadata_location: &str,
        metadata_location: &str,
    ) -> Result<(), IcebergError> {
        let namespace = identifier.namespace().to_string();
        let result = sqlx::query(self.sql_with_binds(
            "update iceberg_tables set metadata_location = $1, previous_metadata_location = $2 where catalog_name = $3 and table_namespace = $4 and table_name = $5 and metadata_location = $6",
            "update iceberg_tables set metadata_location = ?, previous_metadata_location = ? where catalog_name = ? and table_namespace = ? and table_name = ? and metadata_location = ?",
        ))
        .bind(metadata_location)
        .bind(previous_metadata_location)
        .bind(&self.name)
        .bind(&namespace)
        .bind(identifier.name())
        .bind(previous_metadata_location)
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;

        if result.rows_affected() == 0 {
            self.cache.write().unwrap().remove(identifier);
            return Err(IcebergError::CommitConflict(identifier.to_string()));
        }

        Ok(())
    }
}

/// Pool options whose connections get the default SQLite pragmas, then the
/// catalog's tables, then the caller's session statements.
///
/// The caller's statements run last so they can override a default: a table
/// wanting `synchronous = full`, say, sets it and wins.
fn pool_options_with_setup(
    pool_options: PoolOptions<sqlx::Any>,
    is_sqlite: bool,
    session_statements: Vec<String>,
) -> PoolOptions<sqlx::Any> {
    // The hook runs per connection, so the statements are shared rather than
    // cloned for each one.
    let session_statements = Arc::new(session_statements);

    AnyPoolOptions::after_connect(pool_options, move |connection, _| {
        let session_statements = session_statements.clone();
        Box::pin(async move {
            if is_sqlite {
                // Enable write-ahead logging and a busy timeout on every SQLite
                // connection. WAL lets readers proceed during a write and makes
                // each write cheaper, so concurrent catalog commits don't
                // serialize behind an exclusive rollback-journal lock; the busy
                // timeout avoids immediate "database is locked" errors under
                // brief write contention. Both are no-ops on an in-memory
                // database.
                connection.execute("PRAGMA journal_mode=WAL;").await?;
                connection.execute("PRAGMA busy_timeout=30000;").await?;
            }
            connection
                .execute(
                    "create table if not exists iceberg_tables (
                                catalog_name varchar(255) not null,
                                table_namespace varchar(255) not null,
                                table_name varchar(255) not null,
                                metadata_location varchar(255) not null,
                                previous_metadata_location varchar(255),
                                primary key (catalog_name, table_namespace, table_name)
                            );",
                )
                .await?;
            connection
                .execute(
                    "create table if not exists iceberg_namespace_properties (
                                catalog_name varchar(255) not null,
                                namespace varchar(255) not null,
                                property_key varchar(255),
                                property_value varchar(255),
                                primary key (catalog_name, namespace, property_key)
                            );",
                )
                .await?;
            connection
                .execute(
                    "create table if not exists iceberg_namespaces (
                                catalog_name varchar(255) not null,
                                namespace varchar(255) not null,
                                legacy_marker varchar(1) not null default '0',
                                primary key (catalog_name, namespace)
                            );",
                )
                .await?;
            for statement in session_statements.iter() {
                connection.execute(statement.as_str()).await?;
            }
            Ok(())
        })
    })
}

#[derive(Debug)]
struct TableRef {
    table_namespace: String,
    table_name: String,
    metadata_location: String,
    _previous_metadata_location: Option<String>,
}

fn query_map(row: &AnyRow) -> Result<TableRef, sqlx::Error> {
    Ok(TableRef {
        table_namespace: row.try_get(0)?,
        table_name: row.try_get(1)?,
        metadata_location: row.try_get(2)?,
        _previous_metadata_location: row.try_get::<String, _>(3).map(Some).or_else(|err| {
            if let sqlx::Error::ColumnDecode {
                index: _,
                source: _,
            } = err
            {
                Ok(None)
            } else {
                Err(err)
            }
        })?,
    })
}

fn namespace_insert_error(namespace: &str, error: sqlx::Error) -> IcebergError {
    if matches!(&error, sqlx::Error::Database(database) if database.is_unique_violation()) {
        IcebergError::CommitConflict(namespace.to_owned())
    } else {
        Error::from(error).into()
    }
}

fn retryable_namespace_deadlock(error: &IcebergError) -> bool {
    let IcebergError::External(source) = error else {
        return false;
    };
    let Some(Error::SqlError(sqlx::Error::Database(database))) = source.downcast_ref::<Error>()
    else {
        return false;
    };
    matches!(database.code().as_deref(), Some("40001" | "40P01"))
}

async fn backoff_namespace_deadlock(retries: &mut u64, error: &IcebergError) -> bool {
    if *retries >= 3 || !retryable_namespace_deadlock(error) {
        return false;
    }
    *retries += 1;
    futures_timer::Delay::new(std::time::Duration::from_millis(10 * *retries)).await;
    true
}

#[async_trait]
impl Catalog for SqlCatalog {
    /// Catalog name
    fn name(&self) -> &str {
        &self.name
    }
    /// Create a namespace in the catalog
    async fn create_namespace(
        &self,
        namespace: &Namespace,
        properties: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, String>, IcebergError> {
        let namespace_str = namespace.to_string();
        let properties = properties.unwrap_or_default();

        // Older catalogs recorded only properties. The precheck retains their
        // identity; the namespace row enforces uniqueness for new writers.
        if self.namespace_exists(namespace).await? {
            return Err(IcebergError::CommitConflict(namespace_str));
        }

        #[cfg(test)]
        if let Some(barrier) = &self.namespace_create_barrier {
            barrier.wait().await;
        }

        let mut transaction = self.pool.begin().await.map_err(Error::from)?;
        sqlx::query(self.sql_with_binds(
            "insert into iceberg_namespaces (catalog_name, namespace) values ($1, $2)",
            "insert into iceberg_namespaces (catalog_name, namespace) values (?, ?)",
        ))
        .bind(&self.name)
        .bind(&namespace_str)
        .execute(&mut *transaction)
        .await
        .map_err(|error| namespace_insert_error(&namespace_str, error))?;

        // Insert namespace properties into the database
        let insert_property = self.sql_with_binds(
            "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ($1, $2, $3, $4)",
            "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values (?, ?, ?, ?)",
        );
        for (key, value) in &properties {
            sqlx::query(insert_property)
                .bind(&self.name)
                .bind(&namespace_str)
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await
                .map_err(Error::from)?;
        }

        transaction.commit().await.map_err(Error::from)?;

        Ok(properties)
    }
    /// Drop a namespace in the catalog
    async fn drop_namespace(&self, namespace: &Namespace) -> Result<(), IcebergError> {
        let namespace_str = namespace.to_string();
        let mut retries = 0;
        loop {
            let result = async {
                let mut transaction = self.pool.begin().await.map_err(Error::from)?;
                if matches!(
                    self.lock_namespace_for_mutation(&mut transaction, &namespace_str)
                        .await?,
                    NamespaceState::Missing
                ) {
                    return Ok(());
                }
                sqlx::query(self.sql_with_binds(
                    "delete from iceberg_namespace_properties where catalog_name = $1 and namespace = $2",
                    "delete from iceberg_namespace_properties where catalog_name = ? and namespace = ?",
                ))
                .bind(&self.name)
                .bind(&namespace_str)
                .execute(&mut *transaction)
                .await
                .map_err(Error::from)?;
                sqlx::query(self.sql_with_binds(
                    "delete from iceberg_namespaces where catalog_name = $1 and namespace = $2",
                    "delete from iceberg_namespaces where catalog_name = ? and namespace = ?",
                ))
                .bind(&self.name)
                .bind(&namespace_str)
                .execute(&mut *transaction)
                .await
                .map_err(Error::from)?;
                transaction.commit().await.map_err(Error::from)?;
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                if backoff_namespace_deadlock(&mut retries, error).await {
                    continue;
                }
            }
            return result;
        }
    }
    /// Load the namespace properties from the catalog
    async fn load_namespace(
        &self,
        namespace: &Namespace,
    ) -> Result<HashMap<String, String>, IcebergError> {
        let namespace_str = namespace.to_string();

        let rows = sqlx::query(self.sql_with_binds(
            "select p.property_key, p.property_value, n.legacy_marker from iceberg_namespace_properties p left join iceberg_namespaces n on n.catalog_name = p.catalog_name and n.namespace = p.namespace where p.catalog_name = $1 and p.namespace = $2",
            "select p.property_key, p.property_value, n.legacy_marker from iceberg_namespace_properties p left join iceberg_namespaces n on n.catalog_name = p.catalog_name and n.namespace = p.namespace where p.catalog_name = ? and p.namespace = ?",
        ))
        .bind(&self.name)
        .bind(&namespace_str)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;

        let mut properties = HashMap::new();
        for row in &rows {
            let key: String = row.try_get(0).map_err(Error::from)?;
            let marker: Option<String> = row.try_get(2).map_err(Error::from)?;
            // Older catalogs used this key as an internal existence marker.
            if key == "exists" && marker.as_deref() != Some("0") {
                continue;
            }
            let value: String = row.try_get(1).map_err(Error::from)?;
            properties.insert(key, value);
        }

        Ok(properties)
    }
    /// Update the namespace properties in the catalog
    async fn update_namespace(
        &self,
        namespace: &Namespace,
        updates: Option<HashMap<String, String>>,
        removals: Option<Vec<String>>,
    ) -> Result<(), IcebergError> {
        let namespace_str = namespace.to_string();
        let touches_exists = removals
            .as_ref()
            .is_some_and(|keys| keys.iter().any(|key| key == "exists"))
            || updates
                .as_ref()
                .is_some_and(|keys| keys.contains_key("exists"));
        let mut retries = 0;
        loop {
            let result: Result<(), IcebergError> = async {
                let mut transaction = self.pool.begin().await.map_err(Error::from)?;
                let delete_property = self.sql_with_binds(
                    "delete from iceberg_namespace_properties where catalog_name = $1 and namespace = $2 and property_key = $3",
                    "delete from iceberg_namespace_properties where catalog_name = ? and namespace = ? and property_key = ?",
                );
        match self
            .lock_namespace_for_mutation(&mut transaction, &namespace_str)
            .await?
        {
            NamespaceState::Current => {}
            NamespaceState::Legacy => {
                let legacy_marker = sqlx::query_scalar::<_, String>(self.sql_with_binds(
                    "select property_key from iceberg_namespace_properties where catalog_name = $1 and namespace = $2 and property_key = $3",
                    "select property_key from iceberg_namespace_properties where catalog_name = ? and namespace = ? and property_key = ?",
                ))
                .bind(&self.name)
                .bind(&namespace_str)
                .bind("exists")
                .fetch_optional(&mut *transaction)
                .await
                .map_err(Error::from)?;
                let marker = if legacy_marker.as_deref() == Some("exists") {
                    "1"
                } else {
                    "0"
                };
                let insert_identity = match self.pool.connect_options().database_url.scheme() {
                    "postgres" | "postgresql" => "insert into iceberg_namespaces (catalog_name, namespace, legacy_marker) values ($1, $2, $3) on conflict (catalog_name, namespace) do nothing",
                    "sqlite" => "insert into iceberg_namespaces (catalog_name, namespace, legacy_marker) values (?, ?, ?) on conflict (catalog_name, namespace) do nothing",
                    _ => "insert into iceberg_namespaces (catalog_name, namespace, legacy_marker) values (?, ?, ?) on duplicate key update namespace = namespace",
                };
                sqlx::query(insert_identity)
                    .bind(&self.name)
                    .bind(&namespace_str)
                    .bind(marker)
                    .execute(&mut *transaction)
                    .await
                    .map_err(Error::from)?;
            }
            NamespaceState::Missing => return Err(IcebergError::CatalogNotFound),
        }

        if let Some(removals) = removals.as_ref() {
            for key in removals {
                sqlx::query(delete_property)
                    .bind(&self.name)
                    .bind(&namespace_str)
                    .bind(key)
                    .execute(&mut *transaction)
                    .await
                    .map_err(Error::from)?;
            }
        }

        if let Some(updates) = updates.as_ref() {
            let insert_property = self.sql_with_binds(
                "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ($1, $2, $3, $4)",
                "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values (?, ?, ?, ?)",
            );
            for (key, value) in updates {
                // Delete-then-insert keeps this portable across sqlite/postgres/mysql
                // (sqlx::any doesn't expose dialect-specific upsert syntax uniformly).
                sqlx::query(delete_property)
                    .bind(&self.name)
                    .bind(&namespace_str)
                    .bind(key)
                    .execute(&mut *transaction)
                    .await
                    .map_err(Error::from)?;

                sqlx::query(insert_property)
                    .bind(&self.name)
                    .bind(&namespace_str)
                    .bind(key)
                    .bind(value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(Error::from)?;
            }
        }

        if touches_exists {
            sqlx::query(self.sql_with_binds(
                "update iceberg_namespaces set legacy_marker = '0' where catalog_name = $1 and namespace = $2",
                "update iceberg_namespaces set legacy_marker = '0' where catalog_name = ? and namespace = ?",
            ))
            .bind(&self.name)
            .bind(&namespace_str)
            .execute(&mut *transaction)
            .await
            .map_err(Error::from)?;
        }

        transaction.commit().await.map_err(Error::from)?;
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                if backoff_namespace_deadlock(&mut retries, error).await {
                    continue;
                }
            }
            return result;
        }
    }
    /// Check if a namespace exists
    async fn namespace_exists(&self, namespace: &Namespace) -> Result<bool, IcebergError> {
        let namespace_str = namespace.to_string();

        let row = sqlx::query(self.sql_with_binds(
            "select 1 where exists (select 1 from iceberg_namespaces where catalog_name = $1 and namespace = $2) or exists (select 1 from iceberg_namespace_properties where catalog_name = $3 and namespace = $4)",
            "select 1 where exists (select 1 from iceberg_namespaces where catalog_name = ? and namespace = ?) or exists (select 1 from iceberg_namespace_properties where catalog_name = ? and namespace = ?)",
        ))
        .bind(&self.name)
        .bind(&namespace_str)
        .bind(&self.name)
        .bind(&namespace_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;

        Ok(row.is_some())
    }
    async fn list_tabulars(&self, namespace: &Namespace) -> Result<Vec<Identifier>, IcebergError> {
        let namespace = namespace.to_string();

        let rows = {
            sqlx::query(self.sql_with_binds(
                "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = $1 and table_namespace = $2",
                "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = ? and table_namespace = ?",
            ))
            .bind(&self.name)
            .bind(&namespace)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?
        };
        let iter = rows.iter().map(query_map);

        Ok(iter
            .map(|x| {
                x.and_then(|y| {
                    Identifier::parse(&(y.table_namespace.to_string() + "." + &y.table_name), None)
                        .map_err(|err| sqlx::Error::Decode(Box::new(err)))
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(Error::from)?)
    }
    async fn list_namespaces(&self, _parent: Option<&str>) -> Result<Vec<Namespace>, IcebergError> {
        let rows = {
            sqlx::query(self.sql_with_binds(
                "select namespace from iceberg_namespaces where catalog_name = $1 union select namespace from iceberg_namespace_properties where catalog_name = $2",
                "select namespace from iceberg_namespaces where catalog_name = ? union select namespace from iceberg_namespace_properties where catalog_name = ?",
            ))
            .bind(&self.name)
            .bind(&self.name)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?
        };
        let iter = rows.iter().map(|row| row.try_get::<String, _>(0));

        Ok(iter
            .map(|x| {
                x.and_then(|y| {
                    Namespace::try_new(&y.split('.').map(ToString::to_string).collect::<Vec<_>>())
                        .map_err(|err| sqlx::Error::Decode(Box::new(err)))
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(Error::from)?)
    }
    async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, IcebergError> {
        let namespace = identifier.namespace().to_string();
        let rows = sqlx::query(self.sql_with_binds(
            "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = $1 and table_namespace = $2 and table_name = $3",
            "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = ? and table_namespace = ? and table_name = ?",
        ))
        .bind(&self.name)
        .bind(&namespace)
        .bind(identifier.name())
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut iter = rows.iter().map(query_map);

        Ok(iter.next().is_some())
    }
    async fn drop_table(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.delete_tabular_reference(identifier).await
    }
    async fn drop_view(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.delete_tabular_reference(identifier).await
    }
    async fn drop_materialized_view(&self, identifier: &Identifier) -> Result<(), IcebergError> {
        self.delete_tabular_reference(identifier).await
    }
    async fn load_tabular(
        self: Arc<Self>,
        identifier: &Identifier,
    ) -> Result<Tabular, IcebergError> {
        let path = {
            let namespace = identifier.namespace().to_string();
            let row = sqlx::query(self.sql_with_binds(
                "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = $1 and table_namespace = $2 and table_name = $3",
                "select table_namespace, table_name, metadata_location, previous_metadata_location from iceberg_tables where catalog_name = ? and table_namespace = ? and table_name = ?",
            ))
            .bind(&self.name)
            .bind(&namespace)
            .bind(identifier.name())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| IcebergError::CatalogNotFound)?;
            let row = query_map(&row).map_err(Error::from)?;

            row.metadata_location
        };

        let bucket = Bucket::from_path(&path)?;
        let object_store = self.default_object_store(bucket);

        let bytes = object_store
            .get(&strip_prefix(&path).as_str().into())
            .await?
            .bytes()
            .await?;
        let metadata: TabularMetadata = serde_json::from_slice(&bytes)?;
        self.cache
            .write()
            .unwrap()
            .insert(identifier.clone(), (path.clone(), metadata.clone()));
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
        create_table: CreateTable,
    ) -> Result<Table, IcebergError> {
        let metadata: TableMetadata = create_table.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = new_metadata_location(&metadata);
        object_store
            .put_metadata(&metadata_location, metadata.as_ref())
            .await?;

        object_store.put_version_hint(&metadata_location).await.ok();
        self.insert_tabular_reference(&identifier, &metadata_location)
            .await?;
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
        create_view: CreateView<Option<()>>,
    ) -> Result<View, IcebergError> {
        let metadata: ViewMetadata = create_view.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = new_metadata_location(&metadata);
        object_store
            .put_metadata(&metadata_location, metadata.as_ref())
            .await?;

        object_store.put_version_hint(&metadata_location).await.ok();
        self.insert_tabular_reference(&identifier, &metadata_location)
            .await?;
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
        let (create_view, create_table) = create_view.into();
        let metadata: MaterializedViewMetadata = create_view.try_into()?;
        let table_metadata: TableMetadata = create_table.try_into()?;
        // Create metadata
        let location = metadata.location.to_string();

        // Write metadata to object_store
        let bucket = Bucket::from_path(&location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = new_metadata_location(&metadata);

        let table_metadata_location = new_metadata_location(&table_metadata);
        let table_identifier = metadata.current_version(None)?.storage_table();
        object_store
            .put_metadata(&metadata_location, metadata.as_ref())
            .await?;
        object_store.put_version_hint(&metadata_location).await.ok();

        object_store
            .put_metadata(&table_metadata_location, table_metadata.as_ref())
            .await?;
        {
            let mut transaction = self.pool.begin().await.map_err(Error::from)?;
            let namespace = identifier.namespace().to_string();
            let table_namespace = table_identifier.namespace().to_string();
            let insert = self.sql_with_binds(
                "insert into iceberg_tables (catalog_name, table_namespace, table_name, metadata_location) values ($1, $2, $3, $4)",
                "insert into iceberg_tables (catalog_name, table_namespace, table_name, metadata_location) values (?, ?, ?, ?)",
            );
            sqlx::query(insert)
                .bind(&self.name)
                .bind(&namespace)
                .bind(identifier.name())
                .bind(&metadata_location)
                .execute(&mut *transaction)
                .await
                .map_err(Error::from)?;
            sqlx::query(insert)
                .bind(&self.name)
                .bind(&table_namespace)
                .bind(table_identifier.name())
                .bind(&table_metadata_location)
                .execute(&mut *transaction)
                .await
                .map_err(Error::from)?;

            transaction.commit().await.map_err(Error::from)?;
        }
        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.clone(), metadata.clone().into()),
        );
        Ok(MaterializedView::new(identifier.clone(), self.clone(), metadata).await?)
    }

    async fn update_table(self: Arc<Self>, commit: CommitTable) -> Result<Table, IcebergError> {
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

        let bucket = Bucket::from_path(&previous_metadata_location)?;
        let object_store = self.default_object_store(bucket);

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
        // Timestamp of the metadata file we are about to supersede, recorded in
        // the metadata log per the Iceberg spec (the log entry marks when the
        // previous metadata was created).
        let previous_last_updated_ms = metadata.last_updated_ms;
        apply_table_updates(&mut metadata, commit.updates)?;

        // Maintain the metadata log and honor `write.metadata.previous-versions-max`
        // / `write.metadata.delete-after-commit.enabled`. iceberg-rust did not
        // previously record superseded metadata files, so every commit left an
        // orphan `metadata.json` behind; this bounds that growth.
        let delete_after_commit = metadata
            .properties
            .get("write.metadata.delete-after-commit.enabled")
            .map(|v| v == "true")
            .unwrap_or(false);
        let previous_versions_max = metadata
            .properties
            .get("write.metadata.previous-versions-max")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100);
        metadata.metadata_log.push(MetadataLog {
            metadata_file: previous_metadata_location.to_string(),
            timestamp_ms: previous_last_updated_ms,
        });
        let expired_metadata_files: Vec<String> =
            if metadata.metadata_log.len() > previous_versions_max {
                let remove = metadata.metadata_log.len() - previous_versions_max;
                metadata
                    .metadata_log
                    .drain(0..remove)
                    .map(|entry| entry.metadata_file)
                    .collect()
            } else {
                Vec::new()
            };

        let metadata_location = new_metadata_location(&metadata);
        object_store
            .put_metadata(&metadata_location, metadata.as_ref())
            .await?;
        object_store.put_version_hint(&metadata_location).await.ok();

        // A lost compare-and-swap returns early, so reaching this point means
        // our commit landed and the aged-out files are ours to reclaim — a
        // concurrent winner's files are never touched. Best effort: a failed
        // delete only leaves an orphan, never corrupts state.
        self.swap_metadata_location(&identifier, &previous_metadata_location, &metadata_location)
            .await?;

        if delete_after_commit {
            for path in expired_metadata_files {
                let _ = object_store.delete(&strip_prefix(&path).into()).await;
            }
        }

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
        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(IcebergError::InvalidFormat(
                "Create table assertion".to_owned(),
            ));
        };
        let (previous_metadata_location, mut metadata) = entry;

        let bucket = Bucket::from_path(&previous_metadata_location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = match &mut metadata {
            TabularMetadata::View(metadata) => {
                if !check_view_requirements(&commit.requirements, metadata) {
                    return Err(IcebergError::InvalidFormat(
                        "View requirements not valid".to_owned(),
                    ));
                }
                apply_view_updates(metadata, commit.updates)?;
                let metadata_location = new_metadata_location(&*metadata);
                object_store
                    .put_metadata(&metadata_location, metadata.as_ref())
                    .await?;
                object_store.put_version_hint(&metadata_location).await.ok();

                Ok(metadata_location)
            }
            _ => Err(IcebergError::InvalidFormat(
                "View update on entity that is not a view".to_owned(),
            )),
        }?;

        self.swap_metadata_location(&identifier, &previous_metadata_location, &metadata_location)
            .await?;

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
        let identifier = commit.identifier;
        let Some(entry) = self.cache.read().unwrap().get(&identifier).cloned() else {
            return Err(IcebergError::InvalidFormat(
                "Create table assertion".to_owned(),
            ));
        };
        let (previous_metadata_location, mut metadata) = entry;

        let bucket = Bucket::from_path(&previous_metadata_location)?;
        let object_store = self.default_object_store(bucket);

        let metadata_location = match &mut metadata {
            TabularMetadata::MaterializedView(metadata) => {
                if !check_view_requirements(&commit.requirements, metadata) {
                    return Err(IcebergError::InvalidFormat(
                        "Materialized view requirements not valid".to_owned(),
                    ));
                }
                apply_view_updates(metadata, commit.updates)?;

                let metadata_location = new_metadata_location(&*metadata);
                object_store
                    .put_metadata(&metadata_location, metadata.as_ref())
                    .await?;
                object_store.put_version_hint(&metadata_location).await.ok();

                Ok(metadata_location)
            }
            _ => Err(IcebergError::InvalidFormat(
                "Materialized view update on entity that is not a materialized view".to_owned(),
            )),
        }?;

        self.swap_metadata_location(&identifier, &previous_metadata_location, &metadata_location)
            .await?;

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
        identifier: Identifier,
        metadata_location: &str,
    ) -> Result<Table, IcebergError> {
        let bucket = Bucket::from_path(metadata_location)?;
        let object_store = self.default_object_store(bucket);

        let metadata: TableMetadata = serde_json::from_slice(
            &object_store
                .get(&metadata_location.into())
                .await?
                .bytes()
                .await?,
        )?;

        self.insert_tabular_reference(&identifier, metadata_location)
            .await?;
        self.cache.write().unwrap().insert(
            identifier.clone(),
            (metadata_location.to_string(), metadata.clone().into()),
        );
        Ok(Table::new(
            identifier.clone(),
            self.clone(),
            object_store.clone(),
            metadata,
        )
        .await?)
    }
}

impl SqlCatalog {
    pub fn duplicate(&self, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            pool: self.pool.clone(),
            object_store: self.object_store.clone(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            namespace_create_barrier: None,
            #[cfg(test)]
            namespace_mutation_pause: None,
        }
    }
}

#[derive(Debug)]
pub struct SqlCatalogList {
    pool: AnyPool,
    object_store: ObjectStoreBuilder,
}

impl SqlCatalogList {
    pub async fn new(url: &str, object_store: ObjectStoreBuilder) -> Result<Self, Error> {
        Self::new_with_options(url, object_store, SqlCatalogOptions::default()).await
    }

    /// Open a catalog list with explicit connection options.
    pub async fn new_with_options(
        url: &str,
        object_store: ObjectStoreBuilder,
        options: SqlCatalogOptions,
    ) -> Result<Self, Error> {
        install_default_drivers();

        let pool_options = options.pool.unwrap_or_else(|| {
            let pool_options = PoolOptions::new();
            if url.starts_with("sqlite") {
                pool_options.max_connections(1)
            } else {
                pool_options
            }
        });

        let pool = pool_options_with_setup(
            pool_options,
            url.starts_with("sqlite"),
            options.session_statements,
        )
        .connect(url)
        .await?;

        Ok(SqlCatalogList { pool, object_store })
    }
}

#[async_trait]
impl CatalogList for SqlCatalogList {
    fn catalog(&self, name: &str) -> Option<Arc<dyn Catalog>> {
        Some(Arc::new(SqlCatalog {
            name: name.to_owned(),
            pool: self.pool.clone(),
            object_store: self.object_store.clone(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            namespace_create_barrier: None,
            #[cfg(test)]
            namespace_mutation_pause: None,
        }))
    }
    async fn list_catalogs(&self) -> Vec<String> {
        let rows = {
            sqlx::query("select distinct catalog_name from iceberg_tables;")
                .fetch_all(&self.pool)
                .await
                .map_err(Error::from)
                .unwrap_or_default()
        };
        let iter = rows.iter().map(|row| row.try_get::<String, _>(0));

        iter.collect::<Result<_, sqlx::Error>>()
            .map_err(Error::from)
            .unwrap_or_default()
    }
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
        catalog::{namespace::Namespace, Catalog},
        object_store::ObjectStoreBuilder,
        spec::util::strip_prefix,
    };
    use object_store::ObjectStoreExt;
    use testcontainers::{core::ExecCommand, runners::AsyncRunner, ImageExt};
    use testcontainers_modules::{localstack::LocalStack, mysql::Mysql, postgres::Postgres};
    use tokio::time::{sleep, timeout};

    use crate::{SqlCatalog, SqlCatalogOptions};
    use iceberg_rust::error::Error as IcebergError;
    use iceberg_rust::object_store::store::version_hint_content;
    use sqlx::pool::PoolOptions;
    use std::{collections::HashMap, sync::Arc, time::Duration};

    #[tokio::test]
    async fn duplicate_namespace_insert_reports_commit_conflict() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();

        for (name, properties, duplicate_properties) in [
            (
                "empty",
                None,
                Some(HashMap::from([("owner".to_owned(), "second".to_owned())])),
            ),
            (
                "with_properties",
                Some(HashMap::from([("owner".to_owned(), "first".to_owned())])),
                Some(HashMap::from([("region".to_owned(), "second".to_owned())])),
            ),
            (
                "with_exists",
                Some(HashMap::from([("exists".to_owned(), "user".to_owned())])),
                Some(HashMap::from([("region".to_owned(), "second".to_owned())])),
            ),
        ] {
            let namespace = Namespace::try_new(&[name.to_owned()]).unwrap();
            catalog
                .create_namespace(&namespace, properties.clone())
                .await
                .unwrap();
            let error = catalog
                .create_namespace(&namespace, duplicate_properties)
                .await
                .unwrap_err();
            assert!(
                matches!(error, IcebergError::CommitConflict(ref identifier) if identifier == name),
                "expected typed namespace conflict, got {error:?}"
            );
            assert_eq!(
                catalog.load_namespace(&namespace).await.unwrap(),
                properties.clone().unwrap_or_default()
            );
            let namespace_count: i64 = sqlx::query_scalar(&format!(
                "select count(*) from iceberg_namespaces where catalog_name = 'warehouse' and namespace = '{name}'"
            ))
            .fetch_one(&catalog.pool)
            .await
            .unwrap();
            assert_eq!(namespace_count, 1);
            catalog
                .update_namespace(
                    &namespace,
                    Some(HashMap::from([("exists".to_owned(), "user".to_owned())])),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                catalog.load_namespace(&namespace).await.unwrap(),
                properties
                    .unwrap_or_default()
                    .into_iter()
                    .chain([("exists".to_owned(), "user".to_owned())])
                    .collect()
            );
            catalog
                .update_namespace(&namespace, None, Some(vec!["exists".to_owned()]))
                .await
                .unwrap();
            assert!(catalog.namespace_exists(&namespace).await.unwrap());
        }
    }

    async fn assert_concurrent_namespace_create_from_two_pools(url: &str) {
        let options =
            SqlCatalogOptions::new().with_pool_options(PoolOptions::new().max_connections(1));
        let mut first = SqlCatalog::new_with_options(
            url,
            "warehouse",
            ObjectStoreBuilder::memory(),
            options.clone(),
        )
        .await
        .unwrap();
        let mut second =
            SqlCatalog::new_with_options(url, "warehouse", ObjectStoreBuilder::memory(), options)
                .await
                .unwrap();

        for (name, first_properties, second_properties) in [
            ("empty", None, None),
            (
                "empty_and_properties",
                None,
                Some(HashMap::from([("region".to_owned(), "second".to_owned())])),
            ),
            (
                "disjoint_properties",
                Some(HashMap::from([("owner".to_owned(), "first".to_owned())])),
                Some(HashMap::from([("region".to_owned(), "second".to_owned())])),
            ),
        ] {
            let namespace = Namespace::try_new(&[name.to_owned()]).unwrap();
            // Warm both independent pools before forcing both CREATE prechecks
            // to finish, so only the identity insert can decide the winner.
            assert!(!first.namespace_exists(&namespace).await.unwrap());
            assert!(!second.namespace_exists(&namespace).await.unwrap());
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            first.namespace_create_barrier = Some(barrier.clone());
            second.namespace_create_barrier = Some(barrier);
            let (first_result, second_result) = timeout(Duration::from_secs(10), async {
                tokio::join!(
                    first.create_namespace(&namespace, first_properties.clone()),
                    second.create_namespace(&namespace, second_properties.clone()),
                )
            })
            .await
            .unwrap();
            first.namespace_create_barrier = None;
            second.namespace_create_barrier = None;
            let (winner, conflict) = match (first_result, second_result) {
                (Ok(properties), Err(error)) => {
                    assert_eq!(properties, first_properties.unwrap_or_default());
                    (properties, error)
                }
                (Err(error), Ok(properties)) => {
                    assert_eq!(properties, second_properties.unwrap_or_default());
                    (properties, error)
                }
                results => panic!("expected exactly one successful CREATE, got {results:?}"),
            };
            assert!(
                matches!(conflict, IcebergError::CommitConflict(ref identifier) if identifier == name),
                "expected typed namespace conflict, got {conflict:?}"
            );
            assert_eq!(first.load_namespace(&namespace).await.unwrap(), winner);
            assert_eq!(second.load_namespace(&namespace).await.unwrap(), winner);
            let identity_count: i64 = sqlx::query_scalar(first.sql_with_binds(
                "select count(*) from iceberg_namespaces where catalog_name = $1 and namespace = $2",
                "select count(*) from iceberg_namespaces where catalog_name = ? and namespace = ?",
            ))
            .bind(&first.name)
            .bind(name)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            assert_eq!(identity_count, 1);
            let property_count: i64 = sqlx::query_scalar(first.sql_with_binds(
                "select count(*) from iceberg_namespace_properties where catalog_name = $1 and namespace = $2",
                "select count(*) from iceberg_namespace_properties where catalog_name = ? and namespace = ?",
            ))
            .bind(&first.name)
            .bind(name)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            assert_eq!(property_count, i64::try_from(winner.len()).unwrap());
        }

        first.pool.close().await;
        second.pool.close().await;
    }

    #[tokio::test]
    async fn concurrent_namespace_create_from_two_pools_has_one_winner() {
        let path = std::env::temp_dir().join(format!(
            "iceberg-sql-namespace-create-race-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        assert_concurrent_namespace_create_from_two_pools(&url).await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn postgres_concurrent_namespace_create_from_two_pools_has_one_winner() {
        let postgres = Postgres::default()
            .with_db_name("postgres")
            .with_user("postgres")
            .with_password("postgres")
            .start()
            .await
            .unwrap();
        let host = postgres.get_host().await.unwrap();
        let port = postgres.get_host_port_ipv4(5432).await.unwrap();
        assert_concurrent_namespace_create_from_two_pools(&format!(
            "postgres://postgres:postgres@{host}:{port}/postgres"
        ))
        .await;
    }

    #[tokio::test]
    async fn mysql_concurrent_namespace_create_from_two_pools_has_one_winner() {
        let mysql = Mysql::default().start().await.unwrap();
        let host = mysql.get_host().await.unwrap();
        let port = mysql.get_host_port_ipv4(3306).await.unwrap();
        assert_concurrent_namespace_create_from_two_pools(&format!(
            "mysql://root@{host}:{port}/test"
        ))
        .await;
    }

    #[tokio::test]
    async fn namespace_row_unique_violation_maps_to_commit_conflict() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["duplicate".to_owned()]).unwrap();
        catalog.create_namespace(&namespace, None).await.unwrap();
        let sql_error = sqlx::query("insert into iceberg_namespaces (catalog_name, namespace) values ('warehouse', 'duplicate')")
            .execute(&catalog.pool)
            .await
            .unwrap_err();
        assert!(matches!(
            super::namespace_insert_error("duplicate", sql_error),
            IcebergError::CommitConflict(identifier) if identifier == "duplicate"
        ));
    }

    #[tokio::test]
    async fn duplicate_legacy_namespace_without_row_is_rejected() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'legacy', 'owner', 'first')")
            .execute(&catalog.pool)
            .await
            .unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'legacy', 'exists', 'user')")
            .execute(&catalog.pool)
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["legacy".to_owned()]).unwrap();
        let error = catalog
            .create_namespace(
                &namespace,
                Some(HashMap::from([("region".to_owned(), "second".to_owned())])),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IcebergError::CommitConflict(_)));
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap(),
            HashMap::from([("owner".to_owned(), "first".to_owned())])
        );
        catalog
            .update_namespace(&namespace, None, Some(vec!["exists".to_owned()]))
            .await
            .unwrap();
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap(),
            HashMap::from([("owner".to_owned(), "first".to_owned())])
        );
        catalog
            .update_namespace(&namespace, None, Some(vec!["owner".to_owned()]))
            .await
            .unwrap();
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert!(catalog.load_namespace(&namespace).await.unwrap().is_empty());
        let namespace_count: i64 = sqlx::query_scalar(
            "select count(*) from iceberg_namespaces where catalog_name = 'warehouse' and namespace = 'legacy'",
        )
        .fetch_one(&catalog.pool)
        .await
        .unwrap();
        assert_eq!(namespace_count, 1);
    }

    #[tokio::test]
    async fn old_property_only_catalog_survives_last_property_removal() {
        sqlx::any::install_default_drivers();
        let url = "sqlite:file:legacy_namespace_upgrade?mode=memory&cache=shared";
        let old_pool = sqlx::AnyPool::connect(url).await.unwrap();
        sqlx::query("create table iceberg_namespace_properties (catalog_name varchar(255) not null, namespace varchar(255) not null, property_key varchar(255), property_value varchar(255), primary key (catalog_name, namespace, property_key))")
            .execute(&old_pool)
            .await
            .unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'legacy', 'owner', 'user')")
            .execute(&old_pool)
            .await
            .unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'empty_legacy', 'exists', 'true')")
            .execute(&old_pool)
            .await
            .unwrap();

        let catalog = SqlCatalog::new(url, "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["legacy".to_owned()]).unwrap();
        let empty_legacy = Namespace::try_new(&["empty_legacy".to_owned()]).unwrap();
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert!(catalog.namespace_exists(&empty_legacy).await.unwrap());
        assert!(catalog
            .load_namespace(&empty_legacy)
            .await
            .unwrap()
            .is_empty());
        catalog
            .update_namespace(
                &empty_legacy,
                Some(HashMap::from([("owner".to_owned(), "new".to_owned())])),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&empty_legacy).await.unwrap(),
            HashMap::from([("owner".to_owned(), "new".to_owned())])
        );
        let marker_count: i64 = sqlx::query_scalar("select count(*) from iceberg_namespace_properties where catalog_name = 'warehouse' and namespace = 'empty_legacy' and property_key = 'exists'")
            .fetch_one(&catalog.pool)
            .await
            .unwrap();
        assert_eq!(marker_count, 1);
        let marker_flag: String = sqlx::query_scalar("select legacy_marker from iceberg_namespaces where catalog_name = 'warehouse' and namespace = 'empty_legacy'")
            .fetch_one(&catalog.pool)
            .await
            .unwrap();
        assert_eq!(marker_flag, "1");
        catalog
            .update_namespace(
                &empty_legacy,
                Some(HashMap::from([("exists".to_owned(), "user".to_owned())])),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&empty_legacy).await.unwrap(),
            HashMap::from([
                ("owner".to_owned(), "new".to_owned()),
                ("exists".to_owned(), "user".to_owned()),
            ])
        );
        catalog
            .update_namespace(&namespace, None, Some(vec!["owner".to_owned()]))
            .await
            .unwrap();
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert!(catalog.load_namespace(&namespace).await.unwrap().is_empty());
        assert_eq!(
            catalog
                .list_namespaces(None)
                .await
                .unwrap()
                .into_iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
            ["empty_legacy", "legacy"]
        );
    }

    #[tokio::test]
    async fn legacy_user_exists_property_survives_unrelated_update() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["legacy".to_owned()]).unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'legacy', 'exists', 'user')")
            .execute(&catalog.pool)
            .await
            .unwrap();

        catalog
            .update_namespace(
                &namespace,
                Some(HashMap::from([("owner".to_owned(), "new".to_owned())])),
                None,
            )
            .await
            .unwrap();
        let value: String = sqlx::query_scalar("select property_value from iceberg_namespace_properties where catalog_name = 'warehouse' and namespace = 'legacy' and property_key = 'exists'")
            .fetch_one(&catalog.pool)
            .await
            .unwrap();
        assert_eq!(value, "user");
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap(),
            HashMap::from([("owner".to_owned(), "new".to_owned())])
        );

        catalog
            .update_namespace(
                &namespace,
                Some(HashMap::from([("exists".to_owned(), "visible".to_owned())])),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap()["exists"],
            "visible"
        );
    }

    #[tokio::test]
    async fn namespace_values_are_bound_in_sql() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let safe = Namespace::try_new(&["safe".to_owned()]).unwrap();
        let quoted = Namespace::try_new(&["missing' OR 1=1 -- ".to_owned()]).unwrap();
        catalog.create_namespace(&safe, None).await.unwrap();
        assert!(!catalog.namespace_exists(&quoted).await.unwrap());
        catalog
            .create_namespace(
                &quoted,
                Some(HashMap::from([(
                    "owner's".to_owned(),
                    "value 'quoted'".to_owned(),
                )])),
            )
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&quoted).await.unwrap()["owner's"],
            "value 'quoted'"
        );
        catalog
            .update_namespace(
                &quoted,
                Some(HashMap::from([(
                    "x' OR 1=1 -- ".to_owned(),
                    "literal".to_owned(),
                )])),
                None,
            )
            .await
            .unwrap();
        catalog
            .update_namespace(&quoted, None, Some(vec!["x' OR 1=1 -- ".to_owned()]))
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&quoted).await.unwrap(),
            HashMap::from([("owner's".to_owned(), "value 'quoted'".to_owned())])
        );
        catalog.drop_namespace(&quoted).await.unwrap();
        assert!(!catalog.namespace_exists(&quoted).await.unwrap());
        assert!(catalog.namespace_exists(&safe).await.unwrap());
    }

    #[tokio::test]
    async fn tabular_queries_bind_namespace_name_and_metadata_location() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let safe = Namespace::try_new(&["safe".to_owned()]).unwrap();
        let quoted = Namespace::try_new(&["missing' OR 1=1 -- ".to_owned()]).unwrap();
        let safe_table = iceberg_rust::catalog::identifier::Identifier::new(&safe, "item");
        let quoted_table = iceberg_rust::catalog::identifier::Identifier::new(&quoted, "item");
        catalog.create_namespace(&safe, None).await.unwrap();
        catalog.create_namespace(&quoted, None).await.unwrap();
        catalog
            .insert_tabular_reference(&safe_table, "memory://safe")
            .await
            .unwrap();
        catalog
            .insert_tabular_reference(&quoted_table, "memory://old' location")
            .await
            .unwrap();

        assert_eq!(
            catalog.list_tabulars(&quoted).await.unwrap().as_slice(),
            std::slice::from_ref(&quoted_table)
        );
        assert!(catalog.tabular_exists(&quoted_table).await.unwrap());
        catalog
            .swap_metadata_location(
                &quoted_table,
                "memory://old' location",
                "memory://new' location",
            )
            .await
            .unwrap();
        catalog.drop_table(&quoted_table).await.unwrap();
        assert!(!catalog.tabular_exists(&quoted_table).await.unwrap());
        assert!(catalog.tabular_exists(&safe_table).await.unwrap());
    }

    #[tokio::test]
    async fn empty_and_legacy_namespaces_list_and_drop() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let empty = Namespace::try_new(&["empty".to_owned()]).unwrap();
        let legacy = Namespace::try_new(&["legacy".to_owned()]).unwrap();
        catalog.create_namespace(&empty, None).await.unwrap();
        sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ('warehouse', 'legacy', 'owner', 'user')")
            .execute(&catalog.pool)
            .await
            .unwrap();

        let mut names = catalog
            .list_namespaces(None)
            .await
            .unwrap()
            .into_iter()
            .map(|namespace| namespace.to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["empty", "legacy"]);

        catalog.drop_namespace(&empty).await.unwrap();
        catalog.drop_namespace(&legacy).await.unwrap();
        assert!(!catalog.namespace_exists(&empty).await.unwrap());
        assert!(!catalog.namespace_exists(&legacy).await.unwrap());
        assert!(catalog.list_namespaces(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_namespace_drop_preserves_properties_and_row() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["protected".to_owned()]).unwrap();
        let properties = HashMap::from([("owner".to_owned(), "user".to_owned())]);
        catalog
            .create_namespace(&namespace, Some(properties.clone()))
            .await
            .unwrap();
        sqlx::query("create trigger reject_namespace_delete before delete on iceberg_namespaces when old.namespace = 'protected' begin select raise(abort, 'namespace protected'); end")
            .execute(&catalog.pool)
            .await
            .unwrap();
        assert!(matches!(
            catalog.drop_namespace(&namespace).await,
            Err(IcebergError::External(_))
        ));
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap(),
            properties
        );
    }

    #[tokio::test]
    async fn concurrent_update_and_drop_do_not_restore_namespace() {
        let path = std::env::temp_dir().join(format!(
            "iceberg-sql-namespace-race-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let first = SqlCatalog::new(&url, "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let second = SqlCatalog::new(&url, "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        let probe = Namespace::try_new(&["probe".to_owned()]).unwrap();
        assert!(!first.namespace_exists(&probe).await.unwrap());
        assert!(!second.namespace_exists(&probe).await.unwrap());

        for index in 0..16 {
            let namespace = Namespace::try_new(&[format!("current_{index}")]).unwrap();
            first
                .create_namespace(
                    &namespace,
                    Some(HashMap::from([("owner".to_owned(), "before".to_owned())])),
                )
                .await
                .unwrap();
            let (updated, dropped) = tokio::join!(
                first.update_namespace(
                    &namespace,
                    Some(HashMap::from([("owner".to_owned(), "after".to_owned())])),
                    None,
                ),
                second.drop_namespace(&namespace),
            );
            assert!(matches!(
                updated,
                Ok(()) | Err(IcebergError::CatalogNotFound)
            ));
            dropped.unwrap();
            assert!(!first.namespace_exists(&namespace).await.unwrap());
        }

        for index in 0..16 {
            let name = format!("legacy_{index}");
            sqlx::query("insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values (?, ?, ?, ?)")
                .bind("warehouse")
                .bind(&name)
                .bind("owner")
                .bind("before")
                .execute(&first.pool)
                .await
                .unwrap();
            let namespace = Namespace::try_new(&[name]).unwrap();
            let (updated, dropped) = tokio::join!(
                first.update_namespace(
                    &namespace,
                    Some(HashMap::from([("owner".to_owned(), "after".to_owned())])),
                    None,
                ),
                second.drop_namespace(&namespace),
            );
            assert!(matches!(
                updated,
                Ok(()) | Err(IcebergError::CatalogNotFound)
            ));
            dropped.unwrap();
            assert!(!first.namespace_exists(&namespace).await.unwrap());
        }

        drop(first);
        drop(second);
        std::fs::remove_file(&path).unwrap();
    }

    async fn assert_legacy_concurrency_on_locking_catalog(url: &str) {
        let namespace = Namespace::try_new(&["legacy_drop".to_owned()]).unwrap();
        let (probe_entered, missing_identity_probed) = tokio::sync::oneshot::channel();
        let (resume_drop, resumed) = tokio::sync::oneshot::channel();
        let mut catalog = SqlCatalog::new(url, "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        catalog.namespace_mutation_pause = Some(Arc::new(super::NamespaceMutationPause {
            namespace: namespace.to_string(),
            continuation: std::sync::Mutex::new(Some((probe_entered, resumed))),
        }));
        let catalog = Arc::new(catalog);
        // Both transactions must use initialized connections, not run setup while
        // the promoting transaction already holds its namespace/property locks.
        let mut warmed = Vec::new();
        for _ in 0..2 {
            let mut connection = catalog.pool.acquire().await.unwrap();
            sqlx::query("select 1")
                .execute(&mut *connection)
                .await
                .unwrap();
            warmed.push(connection);
        }
        drop(warmed);
        let insert_property = catalog.sql_with_binds(
            "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values ($1, $2, $3, $4)",
            "insert into iceberg_namespace_properties (catalog_name, namespace, property_key, property_value) values (?, ?, ?, ?)",
        );
        sqlx::query(insert_property)
            .bind("warehouse")
            .bind("legacy_drop")
            .bind("exists")
            .bind("true")
            .execute(&catalog.pool)
            .await
            .unwrap();

        let mut promoting = catalog.pool.begin().await.unwrap();
        sqlx::query(catalog.sql_with_binds(
            "select property_key from iceberg_namespace_properties where catalog_name = $1 and namespace = $2 for update",
            "select property_key from iceberg_namespace_properties where catalog_name = ? and namespace = ? for update",
        ))
        .bind("warehouse")
        .bind("legacy_drop")
        .fetch_one(&mut *promoting)
        .await
        .unwrap();
        sqlx::query(catalog.sql_with_binds(
            "delete from iceberg_namespace_properties where catalog_name = $1 and namespace = $2",
            "delete from iceberg_namespace_properties where catalog_name = ? and namespace = ?",
        ))
        .bind("warehouse")
        .bind("legacy_drop")
        .execute(&mut *promoting)
        .await
        .unwrap();
        sqlx::query(catalog.sql_with_binds(
            "insert into iceberg_namespaces (catalog_name, namespace, legacy_marker) values ($1, $2, '1')",
            "insert into iceberg_namespaces (catalog_name, namespace, legacy_marker) values (?, ?, '1')",
        ))
        .bind("warehouse")
        .bind("legacy_drop")
        .execute(&mut *promoting)
        .await
        .unwrap();

        let dropping_catalog = catalog.clone();
        let dropping_namespace = namespace.clone();
        let dropping =
            tokio::spawn(async move { dropping_catalog.drop_namespace(&dropping_namespace).await });
        timeout(Duration::from_secs(10), missing_identity_probed)
            .await
            .expect("DROP must complete its missing-identity probe")
            .expect("missing-identity probe notification");
        assert!(
            !dropping.is_finished(),
            "DROP is parked before its property query"
        );
        // The initial probe saw no identity. Commit removal of the last legacy
        // property and promotion before allowing DROP's locking read to continue.
        promoting.commit().await.unwrap();
        resume_drop.send(()).expect("resume DROP after promotion");
        timeout(Duration::from_secs(10), dropping)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!catalog.namespace_exists(&namespace).await.unwrap());

        for name in ["legacy_a", "legacy_b"] {
            sqlx::query(insert_property)
                .bind("warehouse")
                .bind(name)
                .bind("owner")
                .bind("before")
                .execute(&catalog.pool)
                .await
                .unwrap();
        }
        let mut first = catalog.pool.begin().await.unwrap();
        let mut second = catalog.pool.begin().await.unwrap();
        assert!(matches!(
            catalog
                .lock_namespace_for_mutation(&mut first, "legacy_a")
                .await
                .unwrap(),
            super::NamespaceState::Legacy
        ));
        assert!(matches!(
            catalog
                .lock_namespace_for_mutation(&mut second, "legacy_b")
                .await
                .unwrap(),
            super::NamespaceState::Legacy
        ));
        let insert_identity = catalog.sql_with_binds(
            "insert into iceberg_namespaces (catalog_name, namespace) values ($1, $2)",
            "insert into iceberg_namespaces (catalog_name, namespace) values (?, ?)",
        );
        let (first_insert, second_insert) = timeout(Duration::from_secs(10), async {
            tokio::join!(
                sqlx::query(insert_identity)
                    .bind("warehouse")
                    .bind("legacy_a")
                    .execute(&mut *first),
                sqlx::query(insert_identity)
                    .bind("warehouse")
                    .bind("legacy_b")
                    .execute(&mut *second),
            )
        })
        .await
        .unwrap();
        first_insert.unwrap();
        second_insert.unwrap();
        first.commit().await.unwrap();
        second.commit().await.unwrap();
    }

    #[tokio::test]
    async fn postgres_legacy_namespace_locking_races() {
        let postgres = Postgres::default()
            .with_db_name("postgres")
            .with_user("postgres")
            .with_password("postgres")
            .start()
            .await
            .unwrap();
        let host = postgres.get_host().await.unwrap();
        let port = postgres.get_host_port_ipv4(5432).await.unwrap();
        assert_legacy_concurrency_on_locking_catalog(&format!(
            "postgres://postgres:postgres@{host}:{port}/postgres"
        ))
        .await;
    }

    #[tokio::test]
    async fn mysql_legacy_namespace_locking_races() {
        let mysql = Mysql::default().start().await.unwrap();
        let host = mysql.get_host().await.unwrap();
        let port = mysql.get_host_port_ipv4(3306).await.unwrap();
        assert_legacy_concurrency_on_locking_catalog(&format!("mysql://root@{host}:{port}/test"))
            .await;
    }

    #[tokio::test]
    async fn namespace_row_survives_collation_equivalent_property_removal() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        sqlx::query("drop table iceberg_namespace_properties")
            .execute(&catalog.pool)
            .await
            .unwrap();
        sqlx::query("create table iceberg_namespace_properties (catalog_name varchar(255) not null, namespace varchar(255) not null, property_key varchar(255) collate rtrim, property_value varchar(255), primary key (catalog_name, namespace, property_key))")
            .execute(&catalog.pool)
            .await
            .unwrap();

        let namespace = Namespace::try_new(&["protected".to_owned()]).unwrap();
        catalog
            .create_namespace(
                &namespace,
                Some(HashMap::from([("exists ".to_owned(), "user".to_owned())])),
            )
            .await
            .unwrap();
        assert_eq!(
            catalog.load_namespace(&namespace).await.unwrap()["exists "],
            "user"
        );
        catalog
            .update_namespace(&namespace, None, Some(vec!["exists ".to_owned()]))
            .await
            .unwrap();
        assert!(catalog.namespace_exists(&namespace).await.unwrap());
        assert!(catalog.load_namespace(&namespace).await.unwrap().is_empty());
        let namespace_count: i64 = sqlx::query_scalar("select count(*) from iceberg_namespaces where catalog_name = 'warehouse' and namespace = 'protected'")
            .fetch_one(&catalog.pool)
            .await
            .unwrap();
        assert_eq!(namespace_count, 1);

        let colliding = Namespace::try_new(&["colliding".to_owned()]).unwrap();
        let error = catalog
            .create_namespace(
                &colliding,
                Some(HashMap::from([
                    ("exists".to_owned(), "one".to_owned()),
                    ("exists ".to_owned(), "two".to_owned()),
                ])),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IcebergError::External(_)));
        assert!(!catalog.namespace_exists(&colliding).await.unwrap());
    }

    #[tokio::test]
    async fn failed_namespace_property_insert_rolls_back_namespace_row_and_properties() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        sqlx::query("create trigger reject_blocked_property before insert on iceberg_namespace_properties when new.property_key = 'blocked' begin select raise(abort, 'blocked property'); end")
            .execute(&catalog.pool)
            .await
            .unwrap();
        let namespace = Namespace::try_new(&["atomic".to_owned()]).unwrap();
        let error = catalog
            .create_namespace(
                &namespace,
                Some(HashMap::from([("blocked".to_owned(), "value".to_owned())])),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IcebergError::External(_)));
        assert!(!catalog.namespace_exists(&namespace).await.unwrap());
    }

    #[tokio::test]
    async fn namespace_insert_non_unique_database_error_remains_external() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();
        sqlx::query("create trigger reject_namespace_row before insert on iceberg_namespaces when new.namespace = 'missing_table' begin select raise(abort, 'namespace rejected'); end")
            .execute(&catalog.pool)
            .await
            .unwrap();

        let namespace = Namespace::try_new(&["missing_table".to_owned()]).unwrap();
        let error = catalog
            .create_namespace(&namespace, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IcebergError::External(_)),
            "expected external SQL error, got {error:?}"
        );
        assert!(!catalog.namespace_exists(&namespace).await.unwrap());
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

        let postgres = Postgres::default()
            .with_db_name("postgres")
            .with_user("postgres")
            .with_password("postgres")
            .start()
            .await
            .unwrap();

        let postgres_host = postgres.get_host().await.unwrap();
        let postgres_port = postgres.get_host_port_ipv4(5432).await.unwrap();

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

        let iceberg_catalog = Arc::new(
            SqlCatalog::new(
                &format!("postgres://postgres:postgres@{postgres_host}:{postgres_port}/postgres"),
                "warehouse",
                object_store,
            )
            .await
            .unwrap(),
        );

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

        let object_store = iceberg_catalog
            .default_object_store(iceberg_rust::object_store::Bucket::S3("warehouse"));

        let version_hint = object_store
            .get(&strip_prefix("s3://warehouse/tpch/lineitem/metadata/version-hint.text").into())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();

        let cache = iceberg_catalog.cache.read().unwrap();
        let keys = cache.values().collect::<Vec<_>>();
        let version = version_hint_content(&keys[0].clone().0);

        assert_eq!(std::str::from_utf8(&version_hint).unwrap(), version);
    }

    /// With `write.metadata.delete-after-commit.enabled` + a small
    /// `write.metadata.previous-versions-max`, superseded metadata files are
    /// reclaimed on commit instead of accumulating forever. Uses an in-memory
    /// sqlite catalog + in-memory object store (no containers).
    #[tokio::test]
    async fn delete_after_commit_prunes_old_metadata_files() {
        use futures::TryStreamExt;
        use iceberg_rust::object_store::Bucket;
        use iceberg_rust::spec::schema::Schema;
        use iceberg_rust::spec::types::{PrimitiveType, StructField, Type};
        use iceberg_rust::table::Table;

        let catalog = Arc::new(
            SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
                .await
                .unwrap(),
        );

        catalog
            .create_namespace(&Namespace::try_new(&["ns".to_string()]).unwrap(), None)
            .await
            .unwrap();

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

        let mut table = Table::builder()
            .with_name("t")
            .with_location("/warehouse/ns/t")
            .with_schema(schema)
            .build(&["ns".to_string()], catalog.clone())
            .await
            .unwrap();

        // Keep only one previous metadata file and reclaim the rest.
        table
            .new_transaction(None)
            .update_properties(vec![
                (
                    "write.metadata.delete-after-commit.enabled".to_string(),
                    "true".to_string(),
                ),
                (
                    "write.metadata.previous-versions-max".to_string(),
                    "1".to_string(),
                ),
            ])
            .commit()
            .await
            .unwrap();

        // Several more commits, each writing a fresh metadata file.
        for i in 0..5 {
            table
                .new_transaction(None)
                .update_properties(vec![("marker".to_string(), i.to_string())])
                .commit()
                .await
                .unwrap();
        }

        let object_store = catalog.default_object_store(Bucket::Local);
        let metadata_files = object_store
            .list(Some(&strip_prefix("/warehouse/ns/t/metadata").into()))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .into_iter()
            .filter(|meta| meta.location.as_ref().ends_with(".metadata.json"))
            .count();

        // Without pruning this would be one file per version (7); with
        // previous-versions-max = 1 only the current + one previous survive.
        assert!(
            metadata_files <= 2,
            "expected metadata files pruned to <= 2, found {metadata_files}"
        );
    }

    /// Two catalog instances over the same database — the shape of any
    /// multi-process deployment — must not both report success for commits
    /// built on the same base metadata. The second commit's compare-and-swap
    /// matches no rows, so its snapshot is never recorded; reporting `Ok`
    /// there silently loses the write.
    #[tokio::test]
    async fn concurrent_commit_from_a_second_catalog_reports_conflict() {
        use iceberg_rust::catalog::identifier::Identifier;
        use iceberg_rust::catalog::tabular::Tabular;
        use iceberg_rust::error::Error as IcebergError;
        use iceberg_rust::spec::schema::Schema;
        use iceberg_rust::spec::types::{PrimitiveType, StructField, Type};
        use iceberg_rust::table::Table;

        // A shared-cache in-memory database stands in for a shared catalog
        // database; the cloned builder keeps both catalogs on one object store.
        let url = "sqlite:file:conflict_test?mode=memory&cache=shared";
        let object_store = ObjectStoreBuilder::memory();

        let catalog_a = Arc::new(
            SqlCatalog::new(url, "warehouse", object_store.clone())
                .await
                .unwrap(),
        );
        let catalog_b = Arc::new(
            SqlCatalog::new(url, "warehouse", object_store)
                .await
                .unwrap(),
        );

        catalog_a
            .create_namespace(&Namespace::try_new(&["ns".to_string()]).unwrap(), None)
            .await
            .unwrap();

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

        let mut table_a = Table::builder()
            .with_name("t")
            .with_location("/warehouse/ns/t")
            .with_schema(schema)
            .build(&["ns".to_string()], catalog_a.clone())
            .await
            .unwrap();

        // Catalog B loads the table, caching the same base metadata location
        // that catalog A is about to supersede.
        let identifier = Identifier::new(&["ns".to_string()], "t");
        let Tabular::Table(mut table_b) =
            catalog_b.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("expected a table");
        };

        table_a
            .new_transaction(None)
            .update_properties(vec![("owner".to_string(), "a".to_string())])
            .commit()
            .await
            .unwrap();

        // B's commit is built on metadata A already superseded: it must fail
        // rather than report a success the catalog never recorded.
        let result = table_b
            .new_transaction(None)
            .update_properties(vec![("owner".to_string(), "b".to_string())])
            .commit()
            .await;

        assert!(
            matches!(result, Err(IcebergError::CommitConflict(_))),
            "expected a commit conflict, got {result:?}"
        );

        // And the losing commit must not have overwritten the winner.
        let Tabular::Table(reloaded) = catalog_a.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("expected a table");
        };
        assert_eq!(
            reloaded.metadata().properties.get("owner"),
            Some(&"a".to_string())
        );
    }

    /// Per-session settings cannot be carried on the URL — sqlx's SQLite parser
    /// rejects `busy_timeout` as a query parameter — so the only way to set one
    /// is on the connection itself.
    #[tokio::test]
    async fn session_statements_are_applied_to_pooled_connections() {
        use sqlx::Row;

        let options = SqlCatalogOptions::new()
            .with_session_statements(["pragma busy_timeout = 12345".to_string()]);

        let catalog = SqlCatalog::new_with_options(
            "sqlite:file:session_statements?mode=memory&cache=shared",
            "warehouse",
            ObjectStoreBuilder::memory(),
            options,
        )
        .await
        .unwrap();

        let timeout: i64 = sqlx::query("pragma busy_timeout")
            .fetch_one(&catalog.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(timeout, 12345);
    }

    /// The SQLite defaults must survive: a caller supplying options is adding
    /// to them, not replacing them.
    #[tokio::test]
    async fn the_default_sqlite_pragmas_survive_caller_options() {
        use sqlx::Row;

        let catalog = SqlCatalog::new_with_options(
            "sqlite:file:defaults_survive?mode=memory&cache=shared",
            "warehouse",
            ObjectStoreBuilder::memory(),
            SqlCatalogOptions::new().with_session_statements(["pragma synchronous = normal"]),
        )
        .await
        .unwrap();

        let timeout: i64 = sqlx::query("pragma busy_timeout")
            .fetch_one(&catalog.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(timeout, 30000, "the default busy timeout must still apply");

        let synchronous: i64 = sqlx::query("pragma synchronous")
            .fetch_one(&catalog.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(synchronous, 1, "the caller's statement must have run too");
    }

    /// The pool's size is the caller's to choose; the default leaves several
    /// writers contending on a database that may well serialize them.
    #[tokio::test]
    async fn pool_options_are_honored() {
        let options =
            SqlCatalogOptions::new().with_pool_options(PoolOptions::new().max_connections(3));

        let catalog = SqlCatalog::new_with_options(
            "sqlite:file:pool_options?mode=memory&cache=shared",
            "warehouse",
            ObjectStoreBuilder::memory(),
            options,
        )
        .await
        .unwrap();

        // Force the lazy pool to connect before asking about its size.
        catalog
            .create_namespace(&Namespace::try_new(&["ns".to_string()]).unwrap(), None)
            .await
            .unwrap();

        assert_eq!(catalog.pool.options().get_max_connections(), 3);
    }

    /// A private in-memory SQLite database (`sqlite://`) is only reachable
    /// through the connection that created it, so the single-connection cap
    /// must hold even when the caller supplies their own pool options —
    /// otherwise a second connection silently opens a distinct, empty
    /// database.
    #[tokio::test]
    async fn sqlite_private_memory_caps_connections_even_with_explicit_pool_options() {
        let options =
            SqlCatalogOptions::new().with_pool_options(PoolOptions::new().max_connections(5));

        let catalog = SqlCatalog::new_with_options(
            "sqlite://",
            "warehouse",
            ObjectStoreBuilder::memory(),
            options,
        )
        .await
        .unwrap();

        assert_eq!(catalog.pool.options().get_max_connections(), 1);

        catalog
            .create_namespace(&Namespace::try_new(&["ns".to_string()]).unwrap(), None)
            .await
            .unwrap();
        assert!(catalog
            .namespace_exists(&Namespace::try_new(&["ns".to_string()]).unwrap())
            .await
            .unwrap());
    }

    /// A catalog opened the old way must behave exactly as before, including
    /// the single-connection cap that keeps a private in-memory SQLite
    /// database from being opened twice.
    #[tokio::test]
    async fn the_default_constructor_keeps_its_previous_behavior() {
        let catalog = SqlCatalog::new("sqlite://", "warehouse", ObjectStoreBuilder::memory())
            .await
            .unwrap();

        assert_eq!(catalog.pool.options().get_max_connections(), 1);

        // And it still works end to end.
        catalog
            .create_namespace(&Namespace::try_new(&["ns".to_string()]).unwrap(), None)
            .await
            .unwrap();
        assert!(catalog
            .namespace_exists(&Namespace::try_new(&["ns".to_string()]).unwrap())
            .await
            .unwrap());
    }
}
