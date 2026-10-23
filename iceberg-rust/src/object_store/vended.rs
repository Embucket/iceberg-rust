/*!
Object stores built from catalog-vended configuration (for example REST `loadTable`
`storage-credentials`).

Two concerns live here:

* [`StoreClientConfig`] lets the embedding application supply the HTTP client settings
  (`ClientOptions`, `RetryConfig`, `HttpConnector`) that vended stores are built with. Unset, it
  reproduces object_store's defaults and one new HTTP client per store.
* [`VendedStoreCache`] keeps one S3 store per table across metadata reloads and swaps the vended
  credentials in place, so the store's connection pool and TLS sessions survive a reload.

[`SharedHttpConnector`] additionally shares one HTTP client (and its connection pool) between all
stores built with the same `ClientOptions`. Sharing is safe across credential sets because SigV4
signs every request individually.
*/

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError, RwLock,
    },
};

use async_trait::async_trait;
use object_store::{
    aws::{AmazonS3Builder, AwsCredential},
    azure::MicrosoftAzureBuilder,
    client::{HttpClient, HttpConnector, ReqwestConnector},
    gcp::GoogleCloudStorageBuilder,
    ClientOptions, CredentialProvider, ObjectStore, ObjectStoreScheme, RetryConfig,
};
use url::Url;

use crate::error::Error;
use crate::object_store::parse::{
    object_store_from_config_with_client, s3_builder_from_config, AWS_ACCESS_KEY_ID,
    AWS_ALLOW_ANONYMOUS, AWS_ENDPOINT, AWS_REGION, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN,
    CLIENT_REGION,
};

/// HTTP client settings applied to object stores built from vended configuration.
///
/// Every field is optional; an unset field keeps object_store's default for that setting, so
/// `StoreClientConfig::default()` builds exactly what the catalog built before this type existed.
#[derive(Debug, Clone, Default)]
pub struct StoreClientConfig {
    client_options: Option<ClientOptions>,
    retry_config: Option<RetryConfig>,
    http_connector: Option<Arc<dyn HttpConnector>>,
}

impl StoreClientConfig {
    /// Settings that keep every object_store default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `options` (timeouts, pool settings, proxy, ...) for every vended store.
    ///
    /// Config-derived options are layered on top: an `http://` vended endpoint still enables
    /// `allow_http`.
    pub fn with_client_options(mut self, options: ClientOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    /// Use `retry` for every vended store.
    pub fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry_config = Some(retry);
        self
    }

    /// Create every vended store's HTTP client through `connector`, for example a
    /// [`SharedHttpConnector`] to share one connection pool between stores.
    pub fn with_http_connector(mut self, connector: Arc<dyn HttpConnector>) -> Self {
        self.http_connector = Some(connector);
        self
    }

    /// The configured client options, if any.
    pub fn client_options(&self) -> Option<&ClientOptions> {
        self.client_options.as_ref()
    }

    /// The configured retry policy, if any.
    pub fn retry_config(&self) -> Option<&RetryConfig> {
        self.retry_config.as_ref()
    }

    pub(crate) fn apply_s3(&self, mut builder: AmazonS3Builder) -> AmazonS3Builder {
        if let Some(options) = &self.client_options {
            builder = builder.with_client_options(options.clone());
        }
        if let Some(retry) = &self.retry_config {
            builder = builder.with_retry(retry.clone());
        }
        if let Some(connector) = &self.http_connector {
            builder = builder.with_http_connector(SharedConnectorHandle(connector.clone()));
        }
        builder
    }

    pub(crate) fn apply_gcs(
        &self,
        mut builder: GoogleCloudStorageBuilder,
    ) -> GoogleCloudStorageBuilder {
        if let Some(options) = &self.client_options {
            builder = builder.with_client_options(options.clone());
        }
        if let Some(retry) = &self.retry_config {
            builder = builder.with_retry(retry.clone());
        }
        if let Some(connector) = &self.http_connector {
            builder = builder.with_http_connector(SharedConnectorHandle(connector.clone()));
        }
        builder
    }

    pub(crate) fn apply_azure(&self, mut builder: MicrosoftAzureBuilder) -> MicrosoftAzureBuilder {
        if let Some(options) = &self.client_options {
            builder = builder.with_client_options(options.clone());
        }
        if let Some(retry) = &self.retry_config {
            builder = builder.with_retry(retry.clone());
        }
        if let Some(connector) = &self.http_connector {
            builder = builder.with_http_connector(SharedConnectorHandle(connector.clone()));
        }
        builder
    }
}

/// Adapts a shared `Arc<dyn HttpConnector>` to the builders' `with_http_connector<C>`.
#[derive(Debug)]
struct SharedConnectorHandle(Arc<dyn HttpConnector>);

impl HttpConnector for SharedConnectorHandle {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        self.0.connect(options)
    }
}

/// An [`HttpConnector`] that creates one reqwest client per distinct `ClientOptions` and hands
/// the same client (and so the same connection pool) to every store built with those options.
///
/// object_store calls `connect` on every store `build()`; with this connector, stores built with
/// identical options share a client instead of each opening a cold pool. Options are compared by
/// their `Debug` rendering, which covers every field except the contents of custom root
/// certificates.
#[derive(Debug, Default)]
pub struct SharedHttpConnector {
    clients: Mutex<HashMap<String, HttpClient>>,
    created: AtomicU64,
}

impl SharedHttpConnector {
    /// A connector with no clients yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct HTTP clients this connector has created.
    pub fn clients_created(&self) -> u64 {
        self.created.load(Ordering::Relaxed)
    }
}

impl HttpConnector for SharedHttpConnector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let key = format!("{options:?}");
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = ReqwestConnector::default().connect(options)?;
        clients.insert(key, client.clone());
        self.created.fetch_add(1, Ordering::Relaxed);
        Ok(client)
    }
}

/// AWS credentials that can be replaced while the store that signs with them stays alive.
///
/// Requests already signed keep the credential they were signed with; every later request signs
/// with the credential set by the most recent [`swap`](Self::swap).
#[derive(Debug)]
struct SwappableAwsCredential {
    current: RwLock<Arc<AwsCredential>>,
}

impl SwappableAwsCredential {
    fn new(credential: AwsCredential) -> Self {
        Self {
            current: RwLock::new(Arc::new(credential)),
        }
    }

    fn swap(&self, credential: AwsCredential) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(credential);
    }
}

#[async_trait]
impl CredentialProvider for SwappableAwsCredential {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
        Ok(self
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
}

/// Everything except the credential itself that shapes a vended S3 store. A reload whose scope
/// differs from the cached one rebuilds the store instead of swapping credentials into it.
#[derive(Debug, PartialEq, Eq)]
struct S3StoreScope {
    location: String,
    credential_prefix: String,
    options: Vec<(String, String)>,
}

impl S3StoreScope {
    fn new(url: &Url, credential_prefix: &str, config: &HashMap<String, String>) -> Self {
        let mut options: Vec<(String, String)> = config
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    CLIENT_REGION | AWS_REGION | AWS_ENDPOINT | AWS_ALLOW_ANONYMOUS
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        options.sort();
        Self {
            location: url.to_string(),
            credential_prefix: credential_prefix.to_owned(),
            options,
        }
    }
}

#[derive(Debug)]
struct CachedS3Store {
    scope: S3StoreScope,
    store: Arc<dyn ObjectStore>,
    credentials: Arc<SwappableAwsCredential>,
}

/// One object store per table, kept across metadata reloads.
///
/// For S3 configs that carry static (vended) credentials, a reload of the same table with the
/// same location, credential prefix and store-shaping options returns the cached store with the
/// new credentials swapped in. Any scope change rebuilds and replaces the entry. Entries are keyed
/// by the caller's table identity `K`, so two tables never share a store or its credentials, even
/// at the same location. Other schemes, and S3 configs without static credentials, are rebuilt on
/// every call, as before.
#[derive(Debug)]
pub struct VendedStoreCache<K> {
    client: StoreClientConfig,
    entries: RwLock<HashMap<K, CachedS3Store>>,
    builds: AtomicU64,
}

impl<K: Hash + Eq> Default for VendedStoreCache<K> {
    fn default() -> Self {
        Self::new(StoreClientConfig::default())
    }
}

impl<K: Hash + Eq> VendedStoreCache<K> {
    /// An empty cache whose stores are built with `client`.
    pub fn new(client: StoreClientConfig) -> Self {
        Self {
            client,
            entries: RwLock::new(HashMap::new()),
            builds: AtomicU64::new(0),
        }
    }

    /// The client settings stores are built with.
    pub fn client_config(&self) -> &StoreClientConfig {
        &self.client
    }

    /// How many stores this cache has built (cache hits do not count).
    pub fn builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
    }

    /// The store for table `key` at `url`, configured by the vended Iceberg `config`.
    ///
    /// # Arguments
    /// * `key` - identity of the table the store serves; never shared between tables
    /// * `url` - the table location
    /// * `config` - Iceberg storage options (credentials merged with table config)
    /// * `credential_prefix` - the storage-credential prefix the credentials are scoped to
    ///
    /// # Errors
    /// Returns an error if the store cannot be built from `url` and `config`.
    pub fn get_or_build(
        &self,
        key: K,
        url: Url,
        config: HashMap<String, String>,
        credential_prefix: &str,
    ) -> Result<Arc<dyn ObjectStore>, Error> {
        let is_s3 = matches!(
            ObjectStoreScheme::parse(&url).map_err(object_store::Error::from)?,
            (ObjectStoreScheme::AmazonS3, _)
        );
        let Some(credential) = is_s3.then(|| static_credential(&config)).flatten() else {
            self.evict(&key);
            self.builds.fetch_add(1, Ordering::Relaxed);
            return object_store_from_config_with_client(url, config, &self.client);
        };

        let scope = S3StoreScope::new(&url, credential_prefix, &config);
        {
            let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = entries.get(&key).filter(|entry| entry.scope == scope) {
                entry.credentials.swap(credential);
                return Ok(entry.store.clone());
            }
        }

        let credentials = Arc::new(SwappableAwsCredential::new(credential));
        let store: Arc<dyn ObjectStore> = Arc::new(
            s3_builder_from_config(url, &config, &self.client)
                .with_credentials(credentials.clone())
                .build()?,
        );
        self.builds.fetch_add(1, Ordering::Relaxed);
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                key,
                CachedS3Store {
                    scope,
                    store: store.clone(),
                    credentials,
                },
            );
        Ok(store)
    }

    /// Forget the store cached for `key`, if any.
    pub fn evict(&self, key: &K) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

/// The static AWS credential in an Iceberg S3 config, if it has both a key id and a secret.
fn static_credential(config: &HashMap<String, String>) -> Option<AwsCredential> {
    Some(AwsCredential {
        key_id: config.get(AWS_ACCESS_KEY_ID)?.clone(),
        secret_key: config.get(AWS_SECRET_ACCESS_KEY)?.clone(),
        token: config.get(AWS_SESSION_TOKEN).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Records the options each `connect` receives and delegates to a [`SharedHttpConnector`].
    #[derive(Debug, Default)]
    struct RecordingConnector {
        shared: SharedHttpConnector,
        seen: Mutex<Vec<String>>,
    }

    impl HttpConnector for RecordingConnector {
        fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
            self.seen.lock().unwrap().push(format!("{options:?}"));
            self.shared.connect(options)
        }
    }

    fn vended_config(key_id: &str, endpoint: &str) -> HashMap<String, String> {
        HashMap::from([
            (AWS_ACCESS_KEY_ID.to_string(), key_id.to_string()),
            (AWS_SECRET_ACCESS_KEY.to_string(), "secret".to_string()),
            (AWS_SESSION_TOKEN.to_string(), format!("token-{key_id}")),
            (AWS_ENDPOINT.to_string(), endpoint.to_string()),
            (CLIENT_REGION.to_string(), "us-east-1".to_string()),
        ])
    }

    fn timeouts() -> ClientOptions {
        ClientOptions::new()
            .with_timeout(Duration::from_secs(10))
            .with_connect_timeout(Duration::from_secs(3))
    }

    #[test]
    fn shared_connector_reuses_client_per_distinct_options() {
        let connector = SharedHttpConnector::new();
        connector.connect(&timeouts()).unwrap();
        connector.connect(&timeouts()).unwrap();
        assert_eq!(connector.clients_created(), 1);
        connector
            .connect(&timeouts().with_timeout(Duration::from_secs(11)))
            .unwrap();
        assert_eq!(connector.clients_created(), 2);
    }

    #[test]
    fn cache_builds_vended_store_with_client_options() {
        let connector = Arc::new(RecordingConnector::default());
        let client = StoreClientConfig::new()
            .with_client_options(timeouts())
            .with_http_connector(connector.clone());
        let cache = VendedStoreCache::<&str>::new(client);
        let url = Url::parse("s3://bucket/t").unwrap();
        let store = cache
            .get_or_build(
                "t",
                url,
                vended_config("KEY", "http://localhost:9000"),
                "s3://bucket/t",
            )
            .unwrap();

        let seen = connector.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].contains("timeout: Some(Parsed(10s))"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains("connect_timeout: Some(Parsed(3s))"),
            "{}",
            seen[0]
        );
        // The vended http:// endpoint still enables plain HTTP on top of the embedder's options.
        assert!(seen[0].contains("allow_http: Parsed(true)"), "{}", seen[0]);
        let store = format!("{store:?}");
        assert!(store.contains("timeout: Some(Parsed(10s))"), "{store}");
    }

    #[test]
    fn default_client_config_keeps_object_store_defaults() {
        let cache = VendedStoreCache::<&str>::default();
        let url = Url::parse("s3://bucket/t").unwrap();
        let store = cache
            .get_or_build("t", url, vended_config("KEY", "http://localhost:9000"), "")
            .unwrap();
        let store = format!("{store:?}");
        assert!(store.contains("timeout: Some(Parsed(30s))"), "{store}");
        assert!(
            store.contains("connect_timeout: Some(Parsed(5s))"),
            "{store}"
        );
    }

    #[test]
    fn reload_reuses_store_and_rebuilds_on_scope_change() {
        let connector = Arc::new(SharedHttpConnector::new());
        let cache = VendedStoreCache::<&str>::new(
            StoreClientConfig::new()
                .with_client_options(timeouts())
                .with_http_connector(connector.clone()),
        );
        let url = Url::parse("s3://bucket/t").unwrap();
        let endpoint = "http://localhost:9000";
        let first = cache
            .get_or_build("t", url.clone(), vended_config("K1", endpoint), "p")
            .unwrap();
        let second = cache
            .get_or_build("t", url.clone(), vended_config("K2", endpoint), "p")
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.builds(), 1);

        // Another table at the same location gets its own store but the shared HTTP client.
        let other = cache
            .get_or_build("u", url.clone(), vended_config("K3", endpoint), "p")
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other));

        // A different credential prefix or endpoint is a new scope: rebuild, never swap.
        let rescoped = cache
            .get_or_build("t", url.clone(), vended_config("K4", endpoint), "q")
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &rescoped));
        let moved = cache
            .get_or_build("t", url, vended_config("K5", "http://localhost:9001"), "q")
            .unwrap();
        assert!(!Arc::ptr_eq(&rescoped, &moved));
        assert_eq!(cache.builds(), 4);
        assert_eq!(connector.clients_created(), 1);
    }

    #[test]
    fn s3_config_without_static_credentials_is_rebuilt() {
        let cache = VendedStoreCache::<&str>::default();
        let url = Url::parse("s3://bucket/t").unwrap();
        let config = HashMap::from([
            (AWS_ALLOW_ANONYMOUS.to_string(), "true".to_string()),
            (CLIENT_REGION.to_string(), "us-east-1".to_string()),
        ]);
        let first = cache
            .get_or_build("t", url.clone(), config.clone(), "")
            .unwrap();
        let second = cache.get_or_build("t", url, config, "").unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(cache.builds(), 2);
    }
}
