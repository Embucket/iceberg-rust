//! Owned local view namespace publication, sharing the root's versioned decision slot.

use super::*;
use iceberg_rust::spec::view_metadata::ViewMetadata;

const OWNER_FILE: &str = ".iceberg-view-rename-owner-v1";
const MAX_HISTORY_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
struct RenamePlan {
    operation: Uuid,
    source: Identifier,
    destination: Identifier,
    old_head: String,
    new_head: String,
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
    old: ViewMetadata,
    new: ViewMetadata,
    history: Vec<ArtifactReceipt>,
    old_owner: Option<Vec<u8>>,
}

struct Places {
    source: PathBuf,
    destination: PathBuf,
    metadata: PathBuf,
    stage: PathBuf,
}

fn missing(path: &NativePath) -> Result<bool, IcebergError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io(error)),
        Ok(_) => Ok(false),
    }
}

fn leaf(path: &NativePath) -> Result<&str, IcebergError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && name.len() <= 255 && *name != "." && *name != "..")
        .ok_or_else(|| refusal("view rename has an invalid metadata leaf"))
}

impl RenamePlan {
    fn places(&self, root: &NativeRoot) -> Result<Places, IcebergError> {
        if self.source.namespace().len() != 1
            || self.source.namespace() != self.destination.namespace()
            || self.source == self.destination
            || !plain_component(&self.source.namespace()[0])
            || !plain_component(self.source.name())
            || !plain_component(self.destination.name())
            || self.operation.is_nil()
        {
            return Err(refusal(
                "view rename requires distinct plain names in one local namespace",
            ));
        }
        let location = |identifier: &Identifier| {
            format!(
                "{}/{}/{}",
                root.location.trim_end_matches('/'),
                identifier.namespace()[0],
                identifier.name()
            )
        };
        if self.old.location != location(&self.source)
            || self.new.location != location(&self.destination)
            || self.old.view_uuid.is_nil()
        {
            return Err(refusal("view rename identity or owned location changed"));
        }
        let mut expected = self.old.clone();
        expected.location = self.new.location.clone();
        if expected != self.new
            || self.old_head.ends_with(".gz.metadata.json")
            || serde_json::from_slice::<ViewMetadata>(&self.old_bytes)? != self.old
            || serde_json::from_slice::<ViewMetadata>(&self.new_bytes)? != self.new
            || self.old_bytes.len() > MAX_METADATA
            || self.new_bytes.len() > MAX_METADATA
            || self.new_head
                != new_filesystem_metadata_location(&self.new.location, &self.old_head, false)?
        {
            return Err(refusal(
                "view rename must preserve the exact plain-JSON view image except location",
            ));
        }
        self.old.current_version(None)?;
        self.old.current_schema(None)?;
        if let Some(owner) = &self.old_owner {
            valid_owner(root, owner, &self.source, self.old.view_uuid)?;
        }
        // Canonicalize the configured root/namespace prefix, never a view leaf.
        // A symlink at the source/target name is not authority over its target.
        let namespace = root.path.join(&self.source.namespace()[0]);
        for location in [&self.old.location, &self.new.location] {
            let path = root.native(location)?;
            if canonical_future_path(path.parent().ok_or_else(|| refusal("view parent"))?)?
                != namespace
            {
                return Err(refusal(
                    "view rename namespace is aliased outside its owned directory",
                ));
            }
        }
        let source = namespace.join(self.source.name());
        let destination = namespace.join(self.destination.name());
        let metadata = source.join("metadata");
        if !source.starts_with(&root.path)
            || source.parent() != destination.parent()
            || canonical_future_path(&root.native(&self.old_head)?)?.parent()
                != Some(metadata.as_path())
            || canonical_future_path(&root.native(&self.new_head)?)?.parent()
                != Some(destination.join("metadata").as_path())
        {
            return Err(refusal("view rename paths escape their exact namespace"));
        }
        validate_artifact_bounds(root, &self.history)?;
        let mut total = self.new_bytes.len() as u64 + 8192;
        let mut has_head = false;
        for receipt in &self.history {
            let path = receipt.path(root)?;
            if path.parent() != Some(metadata.as_path()) || receipt.length > MAX_METADATA as u64 {
                return Err(refusal(
                    "view rename history escapes its metadata directory or budget",
                ));
            }
            leaf(&path)?;
            total = total
                .checked_add(receipt.length)
                .ok_or_else(|| refusal("view history byte overflow"))?;
            if path == canonical_future_path(&root.native(&self.old_head)?)? {
                has_head = receipt.length == self.old_bytes.len() as u64
                    && receipt.checksum == checksum_update(FNV_OFFSET, &self.old_bytes);
            }
        }
        if self.history.len() > MAX_ARTIFACTS - 2 || total > MAX_HISTORY_BYTES || !has_head {
            return Err(refusal("view rename history is not complete and bounded"));
        }
        Ok(Places {
            stage: metadata.join(format!(".view-rename-{}", self.operation)),
            source,
            destination,
            metadata,
        })
    }

    fn owner_bytes(&self, root: &NativeRoot) -> Result<Vec<u8>, IcebergError> {
        Ok(serde_json::to_vec(
            &json!({"catalog":root.key,"operation_id":self.operation,
            "view_uuid":self.new.view_uuid,"destination":self.destination}),
        )?)
    }

    fn encode(&self, root: &NativeRoot, phase: &str) -> Result<Vec<u8>, IcebergError> {
        let artifacts = self
            .history
            .iter()
            .map(|r| {
                json!({"path":r.relative,"length":r.length,
            "checksum_fnv1a64":format!("{:016x}",r.checksum)})
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(
            &json!({"version":2,"kind":"view-rename","catalog":root.key,
            "operation_id":self.operation,"phase":phase,"source":self.source,"destination":self.destination,
            "old_head":self.old_head,"new_head":self.new_head,"old_hex":hex(&self.old_bytes),
            "new_json":std::str::from_utf8(&self.new_bytes).map_err(io)?,
            "source_owner_hex":self.old_owner.as_ref().map(|bytes|hex(bytes)),
            "artifacts":artifacts,"artifact_count":self.history.len(),
            "artifact_receipts_fnv1a64":format!("{:016x}",artifact_receipt_checksum(&self.history))}),
        )?;
        if bytes.len() > MAX_JOURNAL {
            return Err(refusal("view rename journal exceeds its byte budget"));
        }
        Ok(bytes)
    }
}

pub(super) fn is_record(bytes: &[u8]) -> Result<bool, IcebergError> {
    let value: Value = serde_json::from_slice(bytes)?;
    Ok(value["version"] == 2)
}

fn decode(root: &NativeRoot, bytes: &[u8]) -> Result<(String, RenamePlan), IcebergError> {
    let value: Value = serde_json::from_slice(bytes)?;
    let fields = [
        "version",
        "kind",
        "catalog",
        "operation_id",
        "phase",
        "source",
        "destination",
        "old_head",
        "new_head",
        "old_hex",
        "new_json",
        "source_owner_hex",
        "artifacts",
        "artifact_count",
        "artifact_receipts_fnv1a64",
    ];
    if value.as_object().is_none_or(|map| {
        map.len() != fields.len() || fields.iter().any(|key| !map.contains_key(*key))
    }) || value["version"] != 2
        || value["kind"] != "view-rename"
        || value["catalog"] != root.key
    {
        return Err(refusal("invalid or foreign view rename journal"));
    }
    let text = |key: &str| {
        value[key]
            .as_str()
            .ok_or_else(|| refusal(format!("missing view rename {key}")))
    };
    let phase = text("phase")?.to_owned();
    if !matches!(phase.as_str(), "ready" | "commit" | "complete" | "aborted") {
        return Err(refusal("unknown view rename phase"));
    }
    let old_bytes = unhex(text("old_hex")?)?;
    let new_bytes = text("new_json")?.as_bytes().to_vec();
    if new_bytes.len() > MAX_METADATA {
        return Err(refusal("view rename metadata exceeds its bound"));
    }
    let plan = RenamePlan {
        operation: Uuid::parse_str(text("operation_id")?).map_err(io)?,
        source: serde_json::from_value(value["source"].clone())?,
        destination: serde_json::from_value(value["destination"].clone())?,
        old_head: text("old_head")?.to_owned(),
        new_head: text("new_head")?.to_owned(),
        old: serde_json::from_slice(&old_bytes)?,
        new: serde_json::from_slice(&new_bytes)?,
        old_bytes,
        new_bytes,
        history: decode_artifacts(root, &value)?,
        old_owner: if value["source_owner_hex"].is_null() {
            None
        } else {
            Some(unhex(text("source_owner_hex")?)?)
        },
    };
    plan.places(root)?;
    Ok((phase, plan))
}

pub(super) fn validate_terminal(root: &NativeRoot, bytes: &[u8]) -> Result<(), IcebergError> {
    let (phase, _) = decode(root, bytes)?;
    if matches!(phase.as_str(), "complete" | "aborted") {
        Ok(())
    } else {
        Err(fenced())
    }
}

fn children(root: &NativeRoot, path: &NativePath) -> Result<Vec<PathBuf>, IcebergError> {
    root.sync_directory(path)?;
    let mut paths = Vec::new();
    for item in std::fs::read_dir(path).map_err(io)? {
        paths.push(item.map_err(io)?.path());
        if paths.len() > MAX_ARTIFACTS + 4 {
            return Err(refusal("view rename directory exceeds its entry budget"));
        }
    }
    Ok(paths)
}

fn valid_owner(
    root: &NativeRoot,
    bytes: &[u8],
    identifier: &Identifier,
    uuid: Uuid,
) -> Result<(), IcebergError> {
    if bytes.len() > 8192 {
        return Err(refusal("view rename owner marker exceeds its bound"));
    }
    let value: Value = serde_json::from_slice(bytes)?;
    if value.as_object().map(|map| map.len()) != Some(4)
        || value["catalog"] != root.key
        || serde_json::from_value::<Identifier>(value["destination"].clone())? != *identifier
        || serde_json::from_value::<Uuid>(value["view_uuid"].clone())? != uuid
        || serde_json::from_value::<Uuid>(value["operation_id"].clone())?.is_nil()
    {
        return Err(refusal("view rename has a foreign container owner marker"));
    }
    Ok(())
}

impl RenamePlan {
    fn capture(
        root: &NativeRoot,
        source: Identifier,
        destination: Identifier,
        old_head: String,
        old: ViewMetadata,
    ) -> Result<Self, IcebergError> {
        let operation = Uuid::new_v4();
        let mut new = old.clone();
        new.location = format!(
            "{}/{}/{}",
            root.location.trim_end_matches('/'),
            destination.namespace()[0],
            destination.name()
        );
        let old_bytes = root.read(&root.native(&old_head)?, MAX_METADATA)?;
        let new_bytes = serde_json::to_vec(&new)?;
        let new_head = new_filesystem_metadata_location(&new.location, &old_head, false)?;
        let source_dir = root.path.join(&source.namespace()[0]).join(source.name());
        let metadata = source_dir.join("metadata");
        let mut old_owner = None;
        for path in children(root, &source_dir)? {
            if path == metadata {
                root.sync_directory(&path)?;
            } else if path == source_dir.join(OWNER_FILE) {
                let bytes = root.read(&path, 8192)?;
                valid_owner(root, &bytes, &source, old.view_uuid)?;
                old_owner = Some(bytes);
            } else {
                return Err(refusal("view rename source contains unowned children"));
            }
        }
        let paths = children(root, &metadata)?;
        let mut total = 0_u64;
        for path in &paths {
            leaf(path)?;
            let file = root.regular(path)?;
            let length = file.metadata().map_err(io)?.len();
            total = total
                .checked_add(length)
                .ok_or_else(|| refusal("view history byte overflow"))?;
            if length > MAX_METADATA as u64 || total > MAX_HISTORY_BYTES {
                return Err(refusal("view rename history exceeds its byte budget"));
            }
        }
        let plan = Self {
            operation,
            source,
            destination,
            old_head,
            new_head,
            old_bytes,
            new_bytes,
            old,
            new,
            history: capture_artifacts(root, &paths)?,
            old_owner,
        };
        plan.places(root)?;
        plan.encode(root, "commit")?;
        validate_source(root, &plan, false, false)?;
        if !missing(&plan.places(root)?.destination)? {
            return Err(IcebergError::InvalidFormat(
                "View rename destination already exists".into(),
            ));
        }
        Ok(plan)
    }

    fn expected_files(
        &self,
        root: &NativeRoot,
    ) -> Result<HashMap<String, (u64, u64)>, IcebergError> {
        let mut files = HashMap::new();
        for receipt in &self.history {
            let path = receipt.path(root)?;
            let name = leaf(&path)?;
            if name != "version-hint.text" {
                files.insert(name.to_owned(), (receipt.length, receipt.checksum));
            }
        }
        let head = root.native(&self.new_head)?;
        if files
            .insert(
                leaf(&head)?.to_owned(),
                (
                    self.new_bytes.len() as u64,
                    checksum_update(FNV_OFFSET, &self.new_bytes),
                ),
            )
            .is_some()
        {
            return Err(refusal("view rename successor collides with history"));
        }
        let hint = parse_version(&self.new_head)?.to_string();
        files.insert(
            "version-hint.text".into(),
            (
                hint.len() as u64,
                checksum_update(FNV_OFFSET, hint.as_bytes()),
            ),
        );
        Ok(files)
    }
}

fn validate_source(
    root: &NativeRoot,
    plan: &RenamePlan,
    stage_allowed: bool,
    decided: bool,
) -> Result<(), IcebergError> {
    let p = plan.places(root)?;
    if missing(&p.source)? {
        return if decided {
            Ok(())
        } else {
            Err(refusal("view rename source disappeared before decision"))
        };
    }
    for path in children(root, &p.source)? {
        if path != p.metadata && path != p.source.join(OWNER_FILE) {
            return Err(refusal("view rename source has unknown children"));
        }
    }
    let owner = p.source.join(OWNER_FILE);
    match (&plan.old_owner, missing(&owner)?) {
        (Some(expected), false) if root.read(&owner, 8192)? == *expected => {
            valid_owner(root, expected, &plan.source, plan.old.view_uuid)?;
        }
        (None, true) => {}
        (Some(_), true) if decided => {}
        _ => return Err(refusal("view rename source owner marker changed")),
    }
    if missing(&p.metadata)? {
        return if decided {
            Ok(())
        } else {
            Err(refusal("view rename metadata directory disappeared"))
        };
    }
    let expected = plan
        .history
        .iter()
        .map(|receipt| Ok((receipt.path(root)?, receipt)))
        .collect::<Result<HashMap<_, _>, IcebergError>>()?;
    let mut found = 0;
    for path in children(root, &p.metadata)? {
        if path == p.stage && stage_allowed {
            root.sync_directory(&path)?;
            continue;
        }
        let receipt = expected
            .get(&path)
            .ok_or_else(|| refusal("view rename source history changed"))?;
        if inspect_artifact(root, &path, false)? != **receipt {
            return Err(refusal("view rename source history bytes changed"));
        }
        found += 1;
    }
    if !decided && found != expected.len() {
        return Err(refusal("view rename source history is missing"));
    }
    Ok(())
}

fn write_new(root: &NativeRoot, path: &NativePath, bytes: &[u8]) -> Result<(), IcebergError> {
    root.sync_directory(
        path.parent()
            .ok_or_else(|| refusal("view rename write parent"))?,
    )?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}

fn stage(root: &NativeRoot, plan: &RenamePlan) -> Result<(), IcebergError> {
    let p = plan.places(root)?;
    validate_source(root, plan, false, false)?;
    if !missing(&p.destination)? {
        return Err(refusal("view rename destination changed"));
    }
    std::fs::create_dir(&p.stage).map_err(io)?;
    let metadata = p.stage.join("metadata");
    std::fs::create_dir(&metadata).map_err(io)?;
    write_new(root, &p.stage.join(OWNER_FILE), &plan.owner_bytes(root)?)?;
    for receipt in &plan.history {
        let path = receipt.path(root)?;
        if leaf(&path)? != "version-hint.text" {
            root.regular(&path)?;
            std::fs::hard_link(&path, metadata.join(leaf(&path)?)).map_err(io)?;
        }
    }
    write_new(
        root,
        &metadata.join(leaf(&root.native(&plan.new_head)?)?),
        &plan.new_bytes,
    )?;
    write_new(
        root,
        &metadata.join("version-hint.text"),
        parse_version(&plan.new_head)?.to_string().as_bytes(),
    )?;
    validate_staged(root, plan, &p.stage, true)?;
    root.sync_ancestors(&metadata)
}

fn validate_staged(
    root: &NativeRoot,
    plan: &RenamePlan,
    directory: &NativePath,
    sync: bool,
) -> Result<(), IcebergError> {
    let metadata = directory.join("metadata");
    let names = children(root, directory)?;
    if names.len() != 2
        || !names.contains(&metadata)
        || !names.contains(&directory.join(OWNER_FILE))
        || root.read(&directory.join(OWNER_FILE), 8192)? != plan.owner_bytes(root)?
    {
        return Err(refusal(
            "view rename destination container is incomplete or foreign",
        ));
    }
    let files = plan.expected_files(root)?;
    let paths = children(root, &metadata)?;
    if paths.len() != files.len() {
        return Err(refusal("view rename decided history is incomplete"));
    }
    for path in paths {
        let (length, checksum) = files
            .get(leaf(&path)?)
            .ok_or_else(|| refusal("view rename has foreign destination files"))?;
        let receipt = inspect_artifact(root, &path, sync)?;
        if receipt.length != *length || receipt.checksum != *checksum {
            return Err(refusal("view rename destination history bytes changed"));
        }
    }
    if sync {
        root.regular(&directory.join(OWNER_FILE))?
            .sync_all()
            .map_err(io)?;
        root.sync_directory(directory)?;
    }
    Ok(())
}

fn clean_stage(root: &NativeRoot, plan: &RenamePlan) -> Result<(), IcebergError> {
    let p = plan.places(root)?;
    validate_source(root, plan, true, false)?;
    if missing(&p.stage)? {
        return Ok(());
    }
    let metadata = p.stage.join("metadata");
    let names = children(root, &p.stage)?;
    if names
        .iter()
        .any(|path| path != &metadata && path != &p.stage.join(OWNER_FILE))
    {
        return Err(refusal("view rename staging has unknown evidence"));
    }
    let files = plan.expected_files(root)?;
    let paths = if missing(&metadata)? {
        Vec::new()
    } else {
        children(root, &metadata)?
    };
    for path in &paths {
        if !files.contains_key(leaf(path)?) {
            return Err(refusal("view rename staging has unknown files"));
        }
        root.regular(path)?;
    }
    let owner = p.stage.join(OWNER_FILE);
    if !missing(&owner)? {
        root.regular(&owner)?;
    }
    for path in paths {
        std::fs::remove_file(path).map_err(io)?;
    }
    if !missing(&metadata)? {
        root.sync_directory(&metadata)?;
        std::fs::remove_dir(&metadata).map_err(io)?;
    }
    if !missing(&owner)? {
        std::fs::remove_file(owner).map_err(io)?;
    }
    root.sync_directory(&p.stage)?;
    std::fs::remove_dir(&p.stage).map_err(io)?;
    root.sync_directory(&p.metadata)
}

fn install(root: &NativeRoot, plan: &RenamePlan) -> Result<(), IcebergError> {
    let p = plan.places(root)?;
    if !missing(&p.destination)? {
        if !missing(&p.stage)? {
            return Err(refusal(
                "view rename has two competing destination containers",
            ));
        }
        return validate_staged(root, plan, &p.destination, true);
    }
    validate_staged(root, plan, &p.stage, true)?;
    validate_source(root, plan, true, true)?;
    // One root writer and the embedding owner exclude destination creation and
    // outside path swaps between this no-follow absence check and rename.
    if !missing(&p.destination)? {
        return Err(refusal("view rename destination changed"));
    }
    std::fs::rename(&p.stage, &p.destination).map_err(io)?;
    root.sync_directory(&p.metadata)?;
    root.sync_directory(
        p.destination
            .parent()
            .ok_or_else(|| refusal("view rename namespace"))?,
    )
}

fn retire_source(
    root: &NativeRoot,
    plan: &RenamePlan,
    after_first: impl FnOnce() -> Result<(), IcebergError>,
) -> Result<(), IcebergError> {
    let p = plan.places(root)?;
    validate_staged(root, plan, &p.destination, true)?;
    validate_source(root, plan, false, true)?;
    if missing(&p.source)? {
        return Ok(());
    }
    if !missing(&p.metadata)? {
        let mut after_first = Some(after_first);
        for path in children(root, &p.metadata)? {
            std::fs::remove_file(path).map_err(io)?;
            if let Some(checkpoint) = after_first.take() {
                root.sync_directory(&p.metadata)?;
                checkpoint()?;
            }
        }
        root.sync_directory(&p.metadata)?;
        std::fs::remove_dir(&p.metadata).map_err(io)?;
        root.sync_directory(&p.source)?;
    }
    let owner = p.source.join(OWNER_FILE);
    if !missing(&owner)? {
        std::fs::remove_file(owner).map_err(io)?;
    }
    std::fs::remove_dir(&p.source).map_err(io)?;
    root.sync_directory(
        p.source
            .parent()
            .ok_or_else(|| refusal("view rename namespace"))?,
    )
}

fn cache_view(cache: &Cache, plan: &RenamePlan) -> Result<(), IcebergError> {
    let mut cache = cache
        .write()
        .map_err(|_| refusal("view rename cache is poisoned"))?;
    cache.remove(&plan.source);
    cache.insert(
        plan.destination.clone(),
        (plan.new_head.clone(), plan.new.clone().into()),
    );
    Ok(())
}

pub(super) fn recover_record(
    root: &NativeRoot,
    cache: &Cache,
    bytes: &[u8],
) -> Result<(), IcebergError> {
    let (phase, plan) = decode(root, bytes)?;
    match phase.as_str() {
        "complete" | "aborted" => Ok(()),
        "ready" => {
            clean_stage(root, &plan)?;
            root.replace_control(JOURNAL, &plan.encode(root, "aborted")?, &mut false)
        }
        "commit" => {
            install(root, &plan)?;
            retire_source(root, &plan, || Ok(()))?;
            root.replace_control(JOURNAL, &plan.encode(root, "complete")?, &mut false)?;
            cache_view(cache, &plan)
        }
        _ => Err(refusal("unknown view rename recovery phase")),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("local view rename {outcome:?}: {source}")]
pub struct ViewRenameError {
    pub outcome: PairOutcome,
    #[source]
    pub source: IcebergError,
}

/// Opaque preparation that retains the catalog writer and exact history receipts.
pub struct PreparedViewRename {
    catalog: Arc<FileCatalog>,
    _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    root: NativeRoot,
    plan: RenamePlan,
    #[cfg(test)]
    hook: Option<Arc<TestHook>>,
    #[cfg(test)]
    delivery_hook: Option<Arc<TestHook>>,
}

impl std::fmt::Debug for PreparedViewRename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedViewRename")
            .field("source", &self.plan.source)
            .field("destination", &self.plan.destination)
            .field("operation", &self.plan.operation)
            .finish_non_exhaustive()
    }
}

impl FileCatalog {
    pub(crate) fn is_local_view_location(&self, identifier: &Identifier, location: &str) -> bool {
        let ObjectStoreBuilder::Filesystem(store) = &self.object_store else {
            return false;
        };
        let Some(namespace) = identifier.namespace().first() else {
            return false;
        };
        let expected = self.tabular_path(namespace, identifier.name());
        if expected == location {
            return true;
        }
        let same = (|| -> Result<bool, IcebergError> {
            let actual = store
                .path_to_filesystem(&ObjectPath::from(strip_prefix(location)))
                .map_err(IcebergError::from)?;
            let expected = store
                .path_to_filesystem(&ObjectPath::from(strip_prefix(&expected)))
                .map_err(IcebergError::from)?;
            Ok(canonical_future_path(&actual)? == canonical_future_path(&expected)?)
        })();
        same.unwrap_or(false)
    }

    pub async fn prepare_view_rename(
        self: &Arc<Self>,
        source: &Identifier,
        destination: &Identifier,
    ) -> Result<PreparedViewRename, IcebergError> {
        if !matches!(self.object_store, ObjectStoreBuilder::Filesystem(_)) {
            return Err(refusal("view rename requires a local filesystem catalog"));
        }
        if source.namespace().len() != 1
            || source.namespace() != destination.namespace()
            || !plain_component(&source.namespace()[0])
            || !plain_component(source.name())
            || !plain_component(destination.name())
        {
            return Err(refusal(
                "view rename requires plain local names in one namespace",
            ));
        }
        if source == destination {
            return Err(IcebergError::InvalidFormat(
                "View rename destination already exists".into(),
            ));
        }
        let guard = self.catalog_mutation_lock().write_owned().await;
        self.check_pair_health()?;
        let root = self.pair_root()?;
        let old_head = self.metadata_location(source).await?;
        if old_head.ends_with(".gz.metadata.json") {
            return Err(refusal("view rename requires a plain JSON current head"));
        }
        let catalog = Arc::clone(self);
        let source = source.clone();
        let destination = destination.clone();
        // The blocking preparation owns the writer even if its awaiting caller
        // disappears. Preparation performs only bounded metadata reads/syncs.
        tokio::task::spawn_blocking(move || {
            if let Err(error) = validate_existing_controls(&root) {
                catalog.pair_state.0.store(FENCED, Ordering::Release);
                return Err(error);
            }
            let raw = root.read(&root.native(&old_head)?, MAX_METADATA)?;
            let TabularMetadata::View(old) = serde_json::from_slice::<TabularMetadata>(&raw)?
            else {
                return Err(IcebergError::InvalidFormat(
                    "View rename source is not a view".into(),
                ));
            };
            let plan = RenamePlan::capture(&root, source, destination, old_head, old)?;
            Ok(PreparedViewRename {
                catalog,
                _guard: guard,
                root,
                plan,
                #[cfg(test)]
                hook: None,
                #[cfg(test)]
                delivery_hook: None,
            })
        })
        .await
        .map_err(io)?
    }
}

impl FileCatalogReadScope {
    /// Borrow this scope for a namespace listing without recursive fair-lock acquisition.
    pub async fn list_tabulars(
        &self,
        namespace: &Namespace,
    ) -> Result<Vec<Identifier>, IcebergError> {
        self.catalog.list_tabulars_under_scope(namespace).await
    }
    /// Borrow this scope for existence rather than re-entering the Catalog trait.
    pub async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, IcebergError> {
        self.catalog.tabular_exists_under_scope(identifier).await
    }
}

impl PreparedViewRename {
    pub async fn publish(self) -> Result<View, ViewRenameError> {
        let catalog = Arc::clone(&self.catalog);
        let destination = self.plan.destination.clone();
        let state = Arc::clone(&catalog.pair_state);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        drop(tokio::task::spawn_blocking(move || {
            let mut owner = RenameOwner {
                prepared: self,
                finished: false,
            };
            let result = owner.publish();
            let healthy = (result.is_ok()
                || matches!(&result,Err(error) if error.outcome==PairOutcome::NotCommitted))
                && owner.prepared.catalog.pair_state.0.load(Ordering::Acquire) != FENCED;
            if sender.send(result).is_ok() && healthy {
                owner
                    .prepared
                    .catalog
                    .pair_state
                    .0
                    .store(HEALTHY, Ordering::Release);
            } else {
                owner
                    .prepared
                    .catalog
                    .pair_state
                    .0
                    .store(FENCED, Ordering::Release);
            }
            owner.finished = true;
            #[cfg(test)]
            if checkpoint(&owner.prepared.delivery_hook, TestStage::AfterDelivery).is_err() {
                owner
                    .prepared
                    .catalog
                    .pair_state
                    .0
                    .store(FENCED, Ordering::Release);
            }
        }));
        let metadata = receiver.await.map_err(|error| {
            state.0.store(FENCED, Ordering::Release);
            ViewRenameError {
                outcome: PairOutcome::DecisionUnknown,
                source: io(error),
            }
        })??;
        View::new(destination, catalog as Arc<dyn Catalog>, metadata)
            .await
            .map_err(|source| {
                state.0.store(FENCED, Ordering::Release);
                ViewRenameError {
                    outcome: PairOutcome::CommittedNeedsRecovery,
                    source,
                }
            })
    }
}

struct RenameOwner {
    prepared: PreparedViewRename,
    finished: bool,
}
impl Drop for RenameOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.prepared
                .catalog
                .pair_state
                .0
                .store(FENCED, Ordering::Release);
        }
    }
}
impl RenameOwner {
    fn publish(&mut self) -> Result<ViewMetadata, ViewRenameError> {
        let prepared = &self.prepared;
        let root = &prepared.root;
        let plan = &prepared.plan;
        let mut outcome = PairOutcome::NotCommitted;
        let mut witness_accepted = false;
        let result = (|| {
            if let Err(error) = validate_existing_controls(root) {
                prepared
                    .catalog
                    .pair_state
                    .0
                    .store(FENCED, Ordering::Release);
                return Err(error);
            }
            validate_source(root, plan, false, false)?;
            if !missing(&plan.places(root)?.destination)? {
                return Err(refusal("view rename destination changed"));
            }
            if let Err(error) = prepare_witness(root) {
                prepared
                    .catalog
                    .pair_state
                    .0
                    .store(FENCED, Ordering::Release);
                return Err(error);
            }
            witness_accepted = true;
            root.replace_control(JOURNAL, &plan.encode(root, "ready")?, &mut false)?;
            prepared
                .catalog
                .pair_state
                .0
                .store(PENDING, Ordering::Release);
            stage(root, plan)?;
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::BeforeDecision)?;
            let mut attempted = false;
            let decision =
                root.replace_control(JOURNAL, &plan.encode(root, "commit")?, &mut attempted);
            if attempted {
                outcome = PairOutcome::DecisionUnknown;
            }
            decision?;
            outcome = PairOutcome::CommittedNeedsRecovery;
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::AfterDecision)?;
            install(root, plan)?;
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::AfterFirstHead)?;
            retire_source(root, plan, || {
                #[cfg(test)]
                checkpoint(&prepared.hook, TestStage::AfterFirstSourceFile)?;
                Ok(())
            })?;
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::AfterSourceRetired)?;
            root.replace_control(JOURNAL, &plan.encode(root, "complete")?, &mut false)?;
            cache_view(&prepared.catalog.cache, plan)?;
            Ok(plan.new.clone())
        })();
        result.map_err(|source| {
            if outcome == PairOutcome::NotCommitted
                && prepared.catalog.pair_state.0.load(Ordering::Acquire) != FENCED
                && witness_accepted
            {
                match control_exists(&root.file(WITNESS)) {
                    Ok(true) => {
                        let abort = (|| {
                            clean_stage(root, plan)?;
                            root.replace_control(
                                JOURNAL,
                                &plan.encode(root, "aborted")?,
                                &mut false,
                            )
                        })();
                        if abort.is_err() {
                            outcome = PairOutcome::DecisionUnknown;
                        }
                    }
                    _ => {
                        prepared
                            .catalog
                            .pair_state
                            .0
                            .store(FENCED, Ordering::Release);
                        outcome = PairOutcome::DecisionUnknown;
                    }
                }
            }
            ViewRenameError { outcome, source }
        })
    }
}

#[cfg(test)]
#[path = "view_rename_fault_tests.rs"]
mod tests;
