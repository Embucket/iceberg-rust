//! One absent native view and its first tracked base, published from one decision.
use super::*;
use iceberg_rust::catalog::create::CreateView;
use iceberg_rust::spec::view_metadata::ViewRepresentation;
use iceberg_rust::view::View;
use std::path::Path;

const MANAGED_VIEW_KEY: &str = "embucket.sql.view-managed.v1";
const VIEW_HEAD: &str = "v0.metadata.json";

/// Proposed CREATE of an absent view with the exact current native table.
pub struct TrackingCreate<'a> {
    pub table_id: &'a Identifier,
    pub table_before: &'a TableMetadata,
    /// First enablement advances this clock. A tracked base keeps its exact head.
    pub table_altered_at_ms: Option<i64>,
    pub view_id: &'a Identifier,
    pub view: CreateView<Option<()>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateDecision {
    operation_id: Uuid,
    table: Head,
    base: CreateBase,
    namespace: String,
    name: String,
    view_after: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CreateBase {
    Advance { altered_at_ms: i64 },
    PreserveTracked {},
}

impl CreateBase {
    const fn advances(&self) -> bool {
        matches!(self, Self::Advance { .. })
    }

    fn successor(&self, before: &TableMetadata) -> Result<TableMetadata, IcebergError> {
        match self {
            Self::Advance { altered_at_ms } => tracking_table_after(before, *altered_at_ms),
            Self::PreserveTracked {} => {
                if before.table_uuid.is_nil()
                    || before.properties.get(TRACKING_KEY).map(String::as_str) != Some("true")
                    || !before
                        .properties
                        .get(ALTERED_KEY)
                        .is_some_and(|value| value.parse::<i64>().is_ok_and(|clock| clock > 0))
                {
                    return Err(refusal("Tracking CREATE cannot preserve an untracked base"));
                }
                Ok(before.clone())
            }
        }
    }
}

fn absent(path: &Path) -> Result<bool, IcebergError> {
    Ok(!control_exists(path)?)
}

fn only_entry(path: &Path, allowed: &str) -> Result<(), IcebergError> {
    for item in std::fs::read_dir(path).map_err(io)? {
        if item.map_err(io)?.file_name() != allowed {
            return Err(refusal(
                "Unexpected file inside decided tracked CREATE destination",
            ));
        }
    }
    Ok(())
}

fn directory(root: &NativeRoot, path: &Path) -> Result<(), IcebergError> {
    if absent(path)? {
        let mut create = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            create.mode(0o700);
        }
        create.create(path).map_err(io)?;
    }
    root.sync_directory(path)
}

impl CreateDecision {
    #[cfg(test)]
    pub(super) fn table_after_bytes(&self) -> Result<Vec<u8>, IcebergError> {
        unhex(&self.table.after)
    }

    fn view_id(&self) -> Identifier {
        Identifier::new(std::slice::from_ref(&self.namespace), &self.name)
    }

    fn successor(
        &self,
        root: &NativeRoot,
    ) -> Result<(TableMetadata, ViewMetadata, PathBuf), IcebergError> {
        if self.operation_id.is_nil()
            || self.table.namespace != self.namespace
            || self.table.name == self.name
            || !super::super::plain_component(&self.namespace)
            || !super::super::plain_component(&self.name)
        {
            return Err(refusal(
                "Tracked CREATE requires distinct native objects in one namespace",
            ));
        }
        let before: TableMetadata = serde_json::from_slice(&unhex(&self.table.before)?)?;
        let table = self.base.successor(&before)?;
        let view: ViewMetadata = serde_json::from_slice(&unhex(&self.view_after)?)?;
        let location = format!(
            "{}/{}/{}",
            root.location.trim_end_matches('/'),
            self.namespace,
            self.name
        );
        let version = view.current_version(None)?;
        let properties = &view.properties;
        let options = properties
            .get(VIEW_OPTIONS_KEY)
            .ok_or_else(|| refusal("Tracked CREATE requires the original managed VIEW options"))?;
        if options.len() > 65_536
            || serde_json::from_str::<serde_json::Value>(options)?
                .get("change_tracking")
                .is_none_or(|value| value.as_bool() != Some(true))
            || properties.get(MANAGED_VIEW_KEY).map(String::as_str) != Some("true")
            || view.location != location
            || view.view_uuid.is_nil()
            || view.view_uuid == table.table_uuid
            || view.schemas.len() != 1
            || view.versions.len() != 1
            || !view.version_log.is_empty()
            || version.default_namespace().as_slice() != [self.namespace.as_str()]
            || !matches!(version.representations.as_slice(),
                [ViewRepresentation::Sql { dialect, .. }] if dialect.eq_ignore_ascii_case("snowflake"))
            || serde_json::from_slice::<TableMetadata>(&unhex(&self.table.after)?)? != table
            || (!self.base.advances() && self.table.before != self.table.after)
        {
            return Err(refusal(
                "Tracked CREATE successor does not match its original objects and options",
            ));
        }
        let destination = root.native(&location)?;
        let parent = destination
            .parent()
            .ok_or_else(|| refusal("Missing tracked CREATE parent"))?;
        root.sync_directory(parent)?;
        root.sync_ancestors(parent)?;
        Ok((table, view, destination))
    }

    fn stage_name(&self) -> String {
        format!(
            ".iceberg-tracked-create-{}.metadata.json",
            self.operation_id
        )
    }

    fn check_destination(
        &self,
        root: &NativeRoot,
        destination: &Path,
        decided: bool,
    ) -> Result<(), IcebergError> {
        if absent(destination)? {
            return Ok(());
        }
        if !decided {
            return Err(refusal(
                "The tracked CREATE destination is already occupied",
            ));
        }
        root.sync_directory(destination)?;
        only_entry(destination, "metadata")?;
        let metadata = destination.join("metadata");
        if !absent(&metadata)? {
            root.sync_directory(&metadata)?;
            only_entry(&metadata, VIEW_HEAD)?;
            let file = metadata.join(VIEW_HEAD);
            if !absent(&file)? && root.read(&file, MAX_METADATA)? != unhex(&self.view_after)? {
                return Err(refusal(
                    "The decided tracked CREATE destination has unexpected metadata",
                ));
            }
        }
        Ok(())
    }

    fn install_view(&self, root: &NativeRoot, destination: &Path) -> Result<(), IcebergError> {
        directory(root, destination)?;
        let metadata = destination.join("metadata");
        directory(root, &metadata)?;
        let stage_name = self.stage_name();
        let stage = root.file(&stage_name);
        let after = unhex(&self.view_after)?;
        let head = metadata.join(VIEW_HEAD);
        if absent(&head)? {
            if absent(&stage)? {
                root.replace_control(&stage_name, &after, &mut false)?;
            } else if root.read(&stage, MAX_METADATA)? != after {
                return Err(refusal("The tracked CREATE staged metadata changed"));
            }
            std::fs::hard_link(&stage, &head).map_err(io)?;
        }
        if root.read(&head, MAX_METADATA)? != after {
            return Err(refusal("The tracked CREATE native image changed"));
        }
        root.regular(&head)?.sync_all().map_err(io)?;
        root.sync_ancestors(&metadata)?;
        if !absent(&stage)? {
            if root.read(&stage, MAX_METADATA)? != after {
                return Err(refusal("The tracked CREATE staged metadata changed"));
            }
            std::fs::remove_file(&stage).map_err(io)?;
            root.sync_directory(&root.path)?;
        }
        Ok(())
    }

    pub(super) fn install(&self, root: &NativeRoot, cache: &Cache) -> Result<(), IcebergError> {
        let (table, _, destination) = self.successor(root)?;
        let table_after = self.table.check(
            root,
            &table.location,
            Some(table.table_uuid),
            self.base.advances(),
        )?;
        self.check_destination(root, &destination, true)?;
        let stage = root.file(&self.stage_name());
        if !absent(&stage)? && root.read(&stage, MAX_METADATA)? != unhex(&self.view_after)? {
            return Err(refusal("The tracked CREATE staged metadata changed"));
        }
        if self.base.advances() {
            root.install_head(&unhex(&self.table.after)?, &table_after)?;
        }
        self.install_view(root, &destination)?;
        let mut cache = cache
            .write()
            .map_err(|_| refusal("Tracking publication cache is poisoned"))?;
        if self.base.advances() {
            cache.remove(&self.table.identifier());
        }
        cache.remove(&self.view_id());
        drop(cache);
        Control::new(root, TrackingState::Idle {}).write(root)
    }
}

pub(super) struct PreparedCreate {
    pub(super) root: NativeRoot,
    pub(super) decision: CreateDecision,
    pub(super) view: ViewMetadata,
}

impl FileCatalog {
    pub(super) async fn prepare_tracked_create_under_scope(
        self: &Arc<Self>,
        change: TrackingCreate<'_>,
    ) -> Result<PreparedCreate, IcebergError> {
        if !matches!(self.object_store, ObjectStoreBuilder::Filesystem(_))
            || change.table_id.namespace().len() != 1
            || change.table_id.namespace() != change.view_id.namespace()
            || change.table_id == change.view_id
            || change.view.name != change.view_id.name()
            || change.view.location.is_some()
        {
            return Err(refusal(
                "Tracked CREATE is limited to one absent native view with a local base",
            ));
        }
        self.check_pair_health()?;
        let root = self.pair_root()?;
        if self.tabular_exists_under_scope(change.view_id).await? {
            return Err(refusal(
                "The tracked CREATE destination is already registered",
            ));
        }
        let Tabular::Table(table) = Arc::clone(self)
            .load_tabular_under_scope(change.table_id)
            .await?
        else {
            return Err(refusal("Tracked CREATE source must be a real table"));
        };
        if table.metadata() != change.table_before {
            return Err(IcebergError::CommitConflict(change.table_id.to_string()));
        }
        let base = match change.table_altered_at_ms {
            Some(altered_at_ms) => CreateBase::Advance { altered_at_ms },
            None => CreateBase::PreserveTracked {},
        };
        let table_after = base.successor(change.table_before)?;
        let mut table_head = Head::capture(
            &root,
            change.table_id,
            self.metadata_location(change.table_id).await?,
            &serde_json::to_vec(&table_after)?,
        )?;
        if !base.advances() {
            table_head.after = table_head.before.clone();
        }
        if serde_json::from_slice::<TableMetadata>(&unhex(&table_head.before)?)?
            != *change.table_before
        {
            return Err(IcebergError::CommitConflict(change.table_id.to_string()));
        }
        let namespace = change.view_id.namespace()[0].clone();
        let mut request = change.view;
        request.location = Some(self.tabular_path(&namespace, change.view_id.name()));
        let view: ViewMetadata = request.try_into()?;
        let view_bytes = serde_json::to_vec(&view)?;
        if view_bytes.len() > MAX_METADATA {
            return Err(refusal(
                "Tracked CREATE view metadata exceeds its native byte bound",
            ));
        }
        let decision = CreateDecision {
            operation_id: Uuid::new_v4(),
            table: table_head,
            base,
            namespace,
            name: change.view_id.name().to_owned(),
            view_after: hex(&view_bytes),
        };
        Ok(PreparedCreate {
            root,
            decision,
            view,
        })
    }

    /// Create an absent managed view and enable tracking on its exact native base.
    /// If the base is already tracked, write only the view and preserve the table head.
    /// The native worker retains writer exclusion through recovery or acknowledgement.
    pub async fn create_view_enabling_base_tracking(
        self: &Arc<Self>,
        change: TrackingCreate<'_>,
    ) -> Result<View, IcebergError> {
        let view_id = change.view_id.clone();
        let lock = self.catalog_mutation_lock().write_owned().await;
        let PreparedCreate {
            root,
            decision,
            view,
        } = self.prepare_tracked_create_under_scope(change).await?;
        let catalog = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            if let Err(error) = Control::require_idle(&root) {
                catalog.pair_state.0.store(FENCED, Ordering::Release);
                return Err(error);
            }
            let (table, _, destination) = decision.successor(&root)?;
            decision
                .table
                .check(&root, &table.location, Some(table.table_uuid), false)?;
            decision.check_destination(&root, &destination, false)?;
            catalog.pair_state.0.store(PENDING, Ordering::Release);
            let result = (|| {
                let control = Control::new(
                    &root,
                    TrackingState::Creating {
                        decision: Box::new(decision),
                    },
                );
                control.write(&root)?;
                witness(&root, true)?;
                let TrackingState::Creating { decision } = control.state else {
                    return Err(refusal("Missing tracked CREATE decision"));
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
        .map_err(io)??;
        View::new(
            view_id,
            Arc::clone(self) as Arc<dyn iceberg_rust::catalog::Catalog>,
            view,
        )
        .await
    }
}
