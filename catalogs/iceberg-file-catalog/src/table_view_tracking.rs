//! The exact native metadata edits for enabling tracking on a view and its base table.

use iceberg_rust::catalog::ExistingViewPropertyReplacement;
use iceberg_rust::spec::table_metadata::TableMetadata;
use iceberg_rust::spec::view_metadata::ViewMetadata;

use serde::{Deserialize, Serialize};

use super::{
    control_exists, hex, io, parse_version, refusal, unhex, Arc, Cache, FileCatalog, IcebergError,
    Identifier, NativeRoot, ObjectStoreBuilder, Ordering, PathBuf, Tabular, Uuid, FENCED, HEALTHY,
    MAX_JOURNAL, MAX_METADATA, PENDING,
};

const TRACKING_KEY: &str = "embucket.sql.change-tracking.v1";
const ALTERED_KEY: &str = "embucket.sql.change-tracking-altered-ms.v1";
const MAX_HEAD_FILES: usize = 4096;
const VIEW_OPTIONS_KEY: &str = "embucket.sql.view-options.v1";
pub(super) const JOURNAL: &str = ".iceberg-table-view-tracking-v1";
pub(super) const WITNESS: &str = ".iceberg-table-view-tracking-required-v1";

/// Prepare the overlap-free metadata changes before any file is replaced. The
/// publication layer must bind the two full predecessors under its native writer.
pub(super) fn tracking_successors(
    table: &TableMetadata,
    view: &ViewMetadata,
    table_altered_at_ms: i64,
    view_edit: &ExistingViewPropertyReplacement,
) -> Result<(TableMetadata, ViewMetadata), IcebergError> {
    if table.table_uuid.is_nil()
        || view.view_uuid.is_nil()
        || table.table_uuid == view.view_uuid
        || table.location == view.location
    {
        return Err(refusal(
            "Change tracking needs distinct native table and view identities",
        ));
    }
    if view_edit.key() != VIEW_OPTIONS_KEY
        || view.properties.get(VIEW_OPTIONS_KEY).map(String::as_str) != Some(view_edit.expected())
    {
        return Err(refusal("Tracking needs the exact existing view options"));
    }

    let table_after = tracking_table_after(table, table_altered_at_ms)?;
    let mut view_after = view.clone();
    view_after
        .properties
        .insert(VIEW_OPTIONS_KEY.into(), view_edit.replacement().into());
    Ok((table_after, view_after))
}

fn tracking_table_after(
    table: &TableMetadata,
    table_altered_at_ms: i64,
) -> Result<TableMetadata, IcebergError> {
    if table.table_uuid.is_nil() {
        return Err(refusal("An unbound table cannot enable tracking"));
    }
    if !matches!(
        table.properties.get(TRACKING_KEY).map(String::as_str),
        None | Some("false")
    ) {
        return Err(refusal(
            "Change tracking pair requires a previously untracked base table",
        ));
    }
    let old_altered = match table.properties.get(ALTERED_KEY) {
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| refusal("Invalid base table change-tracking clock"))?,
        None => 0,
    };
    if table_altered_at_ms <= table.last_updated_ms.max(old_altered) {
        return Err(refusal(
            "Change tracking must advance the table alteration clock",
        ));
    }
    let mut table_after = table.clone();
    table_after
        .properties
        .insert(TRACKING_KEY.into(), "true".into());
    table_after
        .properties
        .insert(ALTERED_KEY.into(), table_altered_at_ms.to_string());

    Ok(table_after)
}

/// Full expected native inputs to one same-catalog table/view tracking decision.
pub struct TrackingChange<'a> {
    pub table_id: &'a Identifier,
    pub view_id: &'a Identifier,
    pub table_before: &'a TableMetadata,
    pub view_before: &'a ViewMetadata,
    pub table_altered_at_ms: i64,
    pub view_edit: ExistingViewPropertyReplacement,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    namespace: String,
    name: String,
    previous: String,
    before: String,
    after: String,
}

impl Head {
    fn capture(
        root: &NativeRoot,
        identifier: &Identifier,
        previous: String,
        after: &[u8],
    ) -> Result<Self, IcebergError> {
        let before = root.read(&root.native(&previous)?, MAX_METADATA)?;
        if after.len() > MAX_METADATA {
            return Err(refusal(
                "Tracking successor exceeds its metadata byte bound",
            ));
        }
        Ok(Self {
            namespace: identifier.namespace()[0].clone(),
            name: identifier.name().to_owned(),
            previous,
            before: hex(&before),
            after: hex(after),
        })
    }

    fn checked_paths(
        &self,
        root: &NativeRoot,
        location: &str,
    ) -> Result<(PathBuf, PathBuf), IcebergError> {
        if !super::plain_component(&self.namespace) || !super::plain_component(&self.name) {
            return Err(refusal("Invalid tracking target identifier"));
        }
        let expected = format!(
            "{}/{}/{}",
            root.location.trim_end_matches('/'),
            self.namespace,
            self.name
        );
        if location != expected || self.previous.ends_with(".gz.metadata.json") {
            return Err(refusal(
                "Tracking predecessor requires the canonical plain local target",
            ));
        }
        let parent = root.native(&format!("{expected}/metadata"))?;
        let old = root.native(&self.previous)?;
        if old.parent() != Some(parent.as_path()) {
            return Err(refusal("Tracking predecessor lies outside its target"));
        }
        let old_version = parse_version(&self.previous)?;
        let new_version = old_version
            .checked_add(1)
            .ok_or_else(|| refusal("Tracking version cannot advance"))?;
        Ok((old, parent.join(format!("v{new_version}.metadata.json"))))
    }

    fn check(
        &self,
        root: &NativeRoot,
        location: &str,
        table_uuid: Option<Uuid>,
        decided: bool,
    ) -> Result<PathBuf, IcebergError> {
        let (old, new) = self.checked_paths(root, location)?;
        let parent = old
            .parent()
            .ok_or_else(|| refusal("Missing tracking metadata directory"))?;
        if let Some(uuid) = table_uuid {
            let marker = parent.join("table-uuid");
            if root.read(&marker, 16)? != uuid.as_bytes() {
                return Err(refusal("Tracking base table physical UUID changed"));
            }
            root.regular(&marker)?.sync_all().map_err(io)?;
        }
        if root.read(&old, MAX_METADATA)? != unhex(&self.before)? {
            return Err(refusal("Tracking predecessor bytes changed"));
        }
        root.regular(&old)?.sync_all().map_err(io)?;
        root.sync_ancestors(parent)?;
        let version = parse_version(&self.previous)?;
        for (index, item) in std::fs::read_dir(parent).map_err(io)?.enumerate() {
            if index >= MAX_HEAD_FILES {
                return Err(refusal("Tracking metadata history exceeds its entry bound"));
            }
            let path = item.map_err(io)?.path();
            let version_at_path = path.to_str().and_then(|path| parse_version(path).ok());
            if version_at_path.is_some_and(|current| current > version)
                && (!decided
                    || path != new
                    || root.read(&path, MAX_METADATA)? != unhex(&self.after)?)
            {
                return Err(refusal(
                    "Tracking head advanced outside its original decision",
                ));
            }
        }
        Ok(new)
    }

    fn identifier(&self) -> Identifier {
        Identifier::new(std::slice::from_ref(&self.namespace), &self.name)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    operation_id: Uuid,
    table: Head,
    view: Head,
    table_altered_at_ms: i64,
    expected_view_options: String,
    replacement_view_options: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum TrackingState {
    Idle {},
    Decided { decision: Box<Decision> },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    version: u8,
    catalog: String,
    state: TrackingState,
}

impl Control {
    fn new(root: &NativeRoot, state: TrackingState) -> Self {
        Self {
            version: 1,
            catalog: root.key.clone(),
            state,
        }
    }

    fn read(root: &NativeRoot) -> Result<Option<Self>, IcebergError> {
        let path = root.file(JOURNAL);
        if !control_exists(&path)? {
            if control_exists(&root.file(WITNESS))? {
                return Err(refusal("Tracking journal is missing for an active root"));
            }
            return Ok(None);
        }
        let value: Self = serde_json::from_slice(&root.read(&path, MAX_JOURNAL)?)?;
        if value.version != 1 || value.catalog != root.key {
            return Err(refusal(
                "Tracking journal belongs to another root or version",
            ));
        }
        Ok(Some(value))
    }

    fn write(&self, root: &NativeRoot) -> Result<(), IcebergError> {
        root.replace_control(JOURNAL, &serde_json::to_vec(self)?, &mut false)
    }
}

impl Decision {
    fn checked_successors(&self) -> Result<(TableMetadata, ViewMetadata), IcebergError> {
        if self.operation_id.is_nil()
            || self.table.namespace != self.view.namespace
            || self.table.name == self.view.name
        {
            return Err(refusal(
                "Tracking decision does not bind two distinct local objects",
            ));
        }
        let table: TableMetadata = serde_json::from_slice(&unhex(&self.table.before)?)?;
        let view: ViewMetadata = serde_json::from_slice(&unhex(&self.view.before)?)?;
        let view_edit = ExistingViewPropertyReplacement::try_new(
            VIEW_OPTIONS_KEY.into(),
            self.expected_view_options.clone(),
            self.replacement_view_options.clone(),
        )?;
        let (table_after, view_after) =
            tracking_successors(&table, &view, self.table_altered_at_ms, &view_edit)?;
        if serde_json::from_slice::<TableMetadata>(&unhex(&self.table.after)?)? != table_after
            || serde_json::from_slice::<ViewMetadata>(&unhex(&self.view.after)?)? != view_after
        {
            return Err(refusal("Tracking successor contains an unapproved edit"));
        }
        Ok((table_after, view_after))
    }

    fn install(&self, root: &NativeRoot, cache: &Cache) -> Result<(), IcebergError> {
        let (table, view) = self.checked_successors()?;
        let table_path = self
            .table
            .check(root, &table.location, Some(table.table_uuid), true)?;
        let view_path = self.view.check(root, &view.location, None, true)?;
        root.install_head(&unhex(&self.table.after)?, &table_path)?;
        root.install_head(&unhex(&self.view.after)?, &view_path)?;
        let mut cache = cache
            .write()
            .map_err(|_| refusal("Tracking publication cache is poisoned"))?;
        cache.remove(&self.table.identifier());
        cache.remove(&self.view.identifier());
        drop(cache);
        Control::new(root, TrackingState::Idle {}).write(root)
    }
}

fn witness(root: &NativeRoot, allow_create: bool) -> Result<(), IcebergError> {
    let bytes = serde_json::to_vec(&Control::new(root, TrackingState::Idle {}))?;
    let path = root.file(WITNESS);
    if control_exists(&path)? {
        if root.read(&path, 4096)? != bytes {
            return Err(refusal(
                "Tracking witness does not match the current catalog",
            ));
        }
        return Ok(());
    }
    if !allow_create {
        return Err(refusal("Tracking witness is missing for an active root"));
    }
    root.replace_control(WITNESS, &bytes, &mut false)
}

pub(super) fn recover(root: &NativeRoot, cache: &Cache) -> Result<(), IcebergError> {
    match Control::read(root)?.map(|control| control.state) {
        None => Ok(()),
        Some(TrackingState::Idle {}) => witness(root, false),
        Some(TrackingState::Decided { decision }) => {
            witness(root, true)?;
            decision.install(root, cache)
        }
    }
}

struct PreparedTracking {
    root: NativeRoot,
    decision: Decision,
    table_location: String,
    table_uuid: Uuid,
    view_location: String,
}

impl PreparedTracking {
    fn require_idle(&self) -> Result<(), IcebergError> {
        match Control::read(&self.root)? {
            None => Ok(()),
            Some(Control {
                state: TrackingState::Idle {},
                ..
            }) => witness(&self.root, false),
            Some(_) => Err(refusal(
                "Tracking root requires recovery before publication",
            )),
        }
    }

    fn validate_heads(&self) -> Result<(), IcebergError> {
        self.decision.table.check(
            &self.root,
            &self.table_location,
            Some(self.table_uuid),
            false,
        )?;
        self.decision
            .view
            .check(&self.root, &self.view_location, None, false)?;
        Ok(())
    }
}

impl FileCatalog {
    async fn prepare_tracking_under_scope(
        self: &Arc<Self>,
        change: TrackingChange<'_>,
    ) -> Result<PreparedTracking, IcebergError> {
        if !matches!(self.object_store, ObjectStoreBuilder::Filesystem(_))
            || change.table_id.namespace().len() != 1
            || change.view_id.namespace().len() != 1
            || change.table_id.namespace() != change.view_id.namespace()
            || change.table_id == change.view_id
        {
            return Err(refusal(
                "Tracking publication requires distinct native objects in one local namespace",
            ));
        }
        self.check_pair_health()?;
        let root = self.pair_root()?;
        let Tabular::Table(table) = Arc::clone(self)
            .load_tabular_under_scope(change.table_id)
            .await?
        else {
            return Err(refusal("Tracking base must be an Iceberg table"));
        };
        let Tabular::View(view) = Arc::clone(self)
            .load_tabular_under_scope(change.view_id)
            .await?
        else {
            return Err(refusal("Tracking target must be an Iceberg view"));
        };
        if table.metadata() != change.table_before || view.metadata() != change.view_before {
            return Err(IcebergError::CommitConflict(change.view_id.to_string()));
        }
        let (table_after, view_after) = tracking_successors(
            table.metadata(),
            view.metadata(),
            change.table_altered_at_ms,
            &change.view_edit,
        )?;
        let table_head = Head::capture(
            &root,
            change.table_id,
            self.metadata_location(change.table_id).await?,
            &serde_json::to_vec(&table_after)?,
        )?;
        let view_head = Head::capture(
            &root,
            change.view_id,
            self.metadata_location(change.view_id).await?,
            &serde_json::to_vec(&view_after)?,
        )?;
        if serde_json::from_slice::<TableMetadata>(&unhex(&table_head.before)?)?
            != *change.table_before
            || serde_json::from_slice::<ViewMetadata>(&unhex(&view_head.before)?)?
                != *change.view_before
        {
            return Err(IcebergError::CommitConflict(change.view_id.to_string()));
        }
        let decision = Decision {
            operation_id: Uuid::new_v4(),
            table: table_head,
            view: view_head,
            table_altered_at_ms: change.table_altered_at_ms,
            expected_view_options: change.view_edit.expected().to_owned(),
            replacement_view_options: change.view_edit.replacement().to_owned(),
        };
        Ok(PreparedTracking {
            root,
            decision,
            table_location: table_after.location,
            table_uuid: table_after.table_uuid,
            view_location: view_after.location,
        })
    }

    pub(super) async fn validate_tracking_under_scope(
        self: &Arc<Self>,
        change: TrackingChange<'_>,
    ) -> Result<(), IcebergError> {
        let prepared = self.prepare_tracking_under_scope(change).await?;
        tokio::task::spawn_blocking(move || {
            prepared.require_idle()?;
            prepared.validate_heads()
        })
        .await
        .map_err(io)?
    }

    /// Publish the table tracking flag and one existing view-option edit in one
    /// durable decision. Recovery completes either acknowledged metadata head.
    pub async fn enable_view_tracking_with_base(
        self: &Arc<Self>,
        change: TrackingChange<'_>,
    ) -> Result<(), IcebergError> {
        let lock = self.catalog_mutation_lock().write_owned().await;
        let prepared = self.prepare_tracking_under_scope(change).await?;
        let catalog = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            if let Err(error) = prepared.require_idle() {
                catalog.pair_state.0.store(FENCED, Ordering::Release);
                return Err(error);
            }
            prepared.validate_heads()?;
            let PreparedTracking { root, decision, .. } = prepared;
            catalog.pair_state.0.store(PENDING, Ordering::Release);
            let result = (|| {
                let state = TrackingState::Decided {
                    decision: Box::new(decision),
                };
                let control = Control::new(&root, state);
                control.write(&root)?;
                witness(&root, true)?;
                let TrackingState::Decided { decision } = control.state else {
                    return Err(refusal("Missing tracking decision"));
                };
                decision.install(&root, &catalog.cache)
            })();
            catalog.pair_state.0.store(
                if result.is_ok() { HEALTHY } else { FENCED },
                Ordering::Release,
            );
            result
        })
        .await
        .map_err(io)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg_rust::catalog::{create::CreateTableBuilder, namespace::Namespace, Catalog};
    use iceberg_rust::spec::schema::Schema;
    use iceberg_rust::spec::types::{PrimitiveType, StructField, Type};
    use iceberg_rust::spec::view_metadata::{Version, ViewRepresentation};
    use iceberg_rust::table::Table;
    use iceberg_rust::view::View;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
    const BEFORE: &str = r#"{"change_tracking":false}"#;
    const AFTER: &str = r#"{"change_tracking":true}"#;

    struct Fixture {
        root: tempfile::TempDir,
        catalog: Arc<FileCatalog>,
        table: Table,
        view: View,
    }

    impl Fixture {
        async fn new() -> TestResult<Self> {
            let root = tempfile::tempdir()?;
            let catalog = Self::open(&root).await?;
            let namespace = Namespace::try_new(&["APP".into()])?;
            catalog.create_namespace(&namespace, None).await?;
            let schema = Schema::builder()
                .with_struct_field(StructField {
                    id: 1,
                    name: "ID".into(),
                    required: true,
                    field_type: Type::Primitive(PrimitiveType::Long),
                    doc: None,
                    initial_default: None,
                    write_default: None,
                })
                .build()?;
            let table = CreateTableBuilder::default()
                .with_name("BASE")
                .with_schema(schema.clone())
                .build(&namespace, Arc::clone(&catalog) as Arc<dyn Catalog>)
                .await?;
            let view = View::builder()
                .with_name("V")
                .with_schema(schema)
                .with_view_version(
                    Version::builder()
                        .with_representation(ViewRepresentation::sql(
                            "SELECT ID FROM APP.BASE",
                            Some("snowflake"),
                        ))
                        .build()?,
                )
                .with_property((VIEW_OPTIONS_KEY.into(), BEFORE.into()))
                .build(&namespace, Arc::clone(&catalog) as Arc<dyn Catalog>)
                .await?;
            Ok(Self {
                root,
                catalog,
                table,
                view,
            })
        }

        async fn open(root: &tempfile::TempDir) -> TestResult<Arc<FileCatalog>> {
            Ok(Arc::new(
                FileCatalog::new(
                    "file:///catalog",
                    ObjectStoreBuilder::filesystem(root.path()),
                )
                .await?,
            ))
        }

        fn edit(expected: &str) -> Result<ExistingViewPropertyReplacement, IcebergError> {
            ExistingViewPropertyReplacement::try_new(
                VIEW_OPTIONS_KEY.into(),
                expected.into(),
                AFTER.into(),
            )
        }

        fn clock(&self) -> i64 {
            self.table.metadata().last_updated_ms + 10
        }

        async fn read(
            catalog: &Arc<FileCatalog>,
            table: &Identifier,
            view: &Identifier,
        ) -> TestResult<(TableMetadata, ViewMetadata)> {
            let reader = catalog.read_scope().await?;
            let Tabular::Table(table) = reader.load(table).await? else {
                return Err("tracking base table missing".into());
            };
            let Tabular::View(view) = reader.load(view).await? else {
                return Err("tracking view missing".into());
            };
            Ok((table.metadata().clone(), view.metadata().clone()))
        }
    }

    #[test]
    fn tracking_control_requires_an_explicit_state_and_decision() {
        for invalid in [
            r#"{"version":1,"catalog":"key"}"#,
            r#"{"version":1,"catalog":"key","state":{"phase":"decided"}}"#,
            r#"{"version":1,"catalog":"key","state":{"phase":"idle","decision":null}}"#,
        ] {
            assert!(
                serde_json::from_str::<Control>(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[tokio::test]
    async fn tracking_publication_preserves_both_native_predecessors_except_expected_properties(
    ) -> TestResult {
        let fixture = Fixture::new().await?;
        let base = fixture.table.metadata().clone();
        let view = fixture.view.metadata().clone();
        let table_id = fixture.table.identifier();
        let view_id = fixture.view.identifier();
        let operation = |edit| TrackingChange {
            table_id,
            view_id,
            table_before: &base,
            view_before: &view,
            table_altered_at_ms: fixture.clock(),
            view_edit: edit,
        };
        let reader = fixture.catalog.read_scope().await?;
        reader
            .validate_view_tracking_with_base(operation(Fixture::edit(BEFORE)?))
            .await?;
        assert!(reader
            .validate_view_tracking_with_base(operation(Fixture::edit("stale")?))
            .await
            .is_err());
        drop(reader);
        assert!(fixture
            .catalog
            .enable_view_tracking_with_base(operation(Fixture::edit("stale")?))
            .await
            .is_err());
        assert_eq!(
            Fixture::read(&fixture.catalog, table_id, view_id).await?,
            (base.clone(), view.clone())
        );
        assert!(!control_exists(
            &fixture.catalog.pair_root()?.file(JOURNAL)
        )?);

        fixture
            .catalog
            .enable_view_tracking_with_base(operation(Fixture::edit(BEFORE)?))
            .await?;
        let (mut table_after, mut view_after) =
            Fixture::read(&fixture.catalog, table_id, view_id).await?;
        let reopened = Fixture::open(&fixture.root).await?;
        assert_eq!(
            Fixture::read(&reopened, table_id, view_id).await?,
            (table_after.clone(), view_after.clone())
        );
        assert_eq!(
            table_after.properties.remove(TRACKING_KEY).as_deref(),
            Some("true")
        );
        assert_eq!(
            table_after.properties.remove(ALTERED_KEY),
            Some(fixture.clock().to_string())
        );
        assert_eq!(
            view_after
                .properties
                .insert(VIEW_OPTIONS_KEY.into(), BEFORE.into())
                .as_deref(),
            Some(AFTER)
        );
        assert_eq!((table_after, view_after), (base, view));
        Ok(())
    }

    #[tokio::test]
    async fn tracking_recovery_finishes_a_decided_pair_after_the_first_native_head() -> TestResult {
        let fixture = Fixture::new().await?;
        let table_id = fixture.table.identifier();
        let view_id = fixture.view.identifier();
        let root = fixture.catalog.pair_root()?;
        let (table, view) = tracking_successors(
            fixture.table.metadata(),
            fixture.view.metadata(),
            fixture.clock(),
            &Fixture::edit(BEFORE)?,
        )?;
        let decision = Decision {
            operation_id: Uuid::new_v4(),
            table: Head::capture(
                &root,
                table_id,
                fixture.catalog.metadata_location(table_id).await?,
                &serde_json::to_vec(&table)?,
            )?,
            view: Head::capture(
                &root,
                view_id,
                fixture.catalog.metadata_location(view_id).await?,
                &serde_json::to_vec(&view)?,
            )?,
            table_altered_at_ms: fixture.clock(),
            expected_view_options: BEFORE.into(),
            replacement_view_options: AFTER.into(),
        };
        let installed_table =
            decision
                .table
                .check(&root, &table.location, Some(table.table_uuid), false)?;
        let uninstalled_view = decision.view.check(&root, &view.location, None, false)?;
        let prepared = serde_json::to_vec(&Control::new(
            &root,
            TrackingState::Decided {
                decision: Box::new(decision),
            },
        ))?;
        root.replace_control(JOURNAL, &prepared, &mut false)?;
        witness(&root, true)?;
        // The exact successor image, including JSON order, belongs to the decision.
        let saved: Control = serde_json::from_slice(&prepared)?;
        let TrackingState::Decided { decision } = saved.state else {
            return Err("missing decision".into());
        };
        let table_image = unhex(&decision.table.after)?;
        root.install_head(&table_image, &installed_table)?;
        assert_eq!(root.read(&installed_table, MAX_METADATA)?, table_image);
        assert!(!uninstalled_view.exists());
        let reopened = Fixture::open(&fixture.root).await?;
        assert_eq!(
            Fixture::read(&reopened, table_id, view_id).await?,
            (table, view)
        );
        assert!(matches!(
            Control::read(&root)?.map(|control| control.state),
            Some(TrackingState::Idle {})
        ));
        Ok(())
    }

    #[tokio::test]
    async fn tracking_recovery_keeps_both_objects_fenced_on_a_third_head() -> TestResult {
        let fixture = Fixture::new().await?;
        let prepared = fixture
            .catalog
            .prepare_tracking_under_scope(TrackingChange {
                table_id: fixture.table.identifier(),
                view_id: fixture.view.identifier(),
                table_before: fixture.table.metadata(),
                view_before: fixture.view.metadata(),
                table_altered_at_ms: fixture.clock(),
                view_edit: Fixture::edit(BEFORE)?,
            })
            .await?;
        let table_after = prepared.decision.table.check(
            &prepared.root,
            &prepared.table_location,
            Some(prepared.table_uuid),
            false,
        )?;
        let view_after =
            prepared
                .decision
                .view
                .check(&prepared.root, &prepared.view_location, None, false)?;
        let root = prepared.root;
        Control::new(
            &root,
            TrackingState::Decided {
                decision: Box::new(prepared.decision),
            },
        )
        .write(&root)?;
        witness(&root, true)?;
        root.install_head(b"unexpected head", &view_after)?;
        assert!(Fixture::open(&fixture.root).await.is_err());
        assert!(!table_after.exists());
        assert_eq!(root.read(&view_after, MAX_METADATA)?, b"unexpected head");
        assert!(matches!(
            Control::read(&root)?.map(|value| value.state),
            Some(TrackingState::Decided { .. })
        ));
        Ok(())
    }
}
