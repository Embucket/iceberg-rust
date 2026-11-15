use super::*;
use iceberg_rust::catalog::commit::TableRequirement;
use iceberg_rust::catalog::create::CreateTableBuilder;
use iceberg_rust::spec::{
    manifest::{Content, DataFile, FileFormat},
    schema::Schema,
    types::{PrimitiveType, StructField, Type},
    values::{Struct, Value},
};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Fixture {
    root: tempfile::TempDir,
    catalog: Arc<FileCatalog>,
    tables: [Table; 2],
    commits: [CommitTable; 2],
}

impl Fixture {
    async fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let catalog = Arc::new(
            FileCatalog::new(
                "file:///catalog",
                ObjectStoreBuilder::filesystem(root.path()),
            )
            .await?,
        );
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
            .build()?;
        let namespace = Namespace::try_new(&["APP".to_owned()])?;
        catalog.create_namespace(&namespace, None).await?;
        let mut tables = Vec::new();
        let mut commits = Vec::new();
        for name in ["A", "B"] {
            let mut builder = CreateTableBuilder::default();
            let mut table = builder
                .with_name(name)
                .with_schema(schema.clone())
                .build(&namespace, Arc::clone(&catalog) as Arc<dyn Catalog>)
                .await?;
            // The publisher tests metadata/manifest publication. This is deliberately
            // not a Parquet decoding or SQL row acceptance fixture.
            let path = format!("{}/data/input.parquet", table.metadata().location);
            table
                .object_store()
                .put(&strip_prefix(&path).into(), b"PAR1".to_vec().into())
                .await?;
            let file = DataFile::builder()
                .with_content(Content::Data)
                .with_file_path(path)
                .with_file_format(FileFormat::Parquet)
                .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
                .with_record_count(1)
                .with_file_size_in_bytes(4)
                .with_column_sizes(None)
                .with_value_counts(None)
                .with_null_value_counts(None)
                .with_nan_value_counts(None)
                .with_distinct_counts(None)
                .with_lower_bounds(None)
                .with_upper_bounds(None)
                .build()?;
            commits.push(
                table
                    .new_transaction(None)
                    .append_data(vec![file])
                    .prepare_commit()
                    .await?
                    .ok_or("prepared commit")?,
            );
            tables.push(table);
        }
        Ok(Self {
            root,
            catalog,
            tables: tables.try_into().map_err(|_| "two tables")?,
            commits: commits.try_into().map_err(|_| "two commits")?,
        })
    }

    async fn snapshots(&self) -> TestResult<[Option<i64>; 2]> {
        let scope = self.catalog.read_scope().await?;
        let mut snapshots = Vec::new();
        for table in &self.tables {
            let Tabular::Table(table) = scope.load(table.identifier()).await? else {
                return Err("table".into());
            };
            snapshots.push(table.metadata().current_snapshot_id);
        }
        Ok(snapshots.try_into().map_err(|_| "two snapshots")?)
    }
}

#[tokio::test]
async fn publishes_both_native_prepared_heads_and_reopens() -> TestResult {
    let fixture = Fixture::new().await?;
    assert_eq!(fixture.snapshots().await?, [None, None]);
    let prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    let receipt = prepared.publish().await?;
    assert!(!receipt.operation_id.is_nil());
    let snapshots = fixture.snapshots().await?;
    assert!(snapshots.iter().all(Option::is_some));
    for (index, (identifier, metadata)) in receipt.tables.iter().enumerate() {
        assert_eq!(identifier, fixture.tables[index].identifier());
        assert_eq!(
            metadata.table_uuid,
            fixture.tables[index].metadata().table_uuid
        );
        assert_eq!(metadata.current_snapshot_id, snapshots[index]);
    }
    let reopened = Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(fixture.root.path()),
        )
        .await?,
    );
    let scope = reopened.read_scope().await?;
    for (index, table) in fixture.tables.iter().enumerate() {
        let Tabular::Table(current) = scope.load(table.identifier()).await? else {
            return Err("table".into());
        };
        assert_eq!(current.metadata().current_snapshot_id, snapshots[index]);
        assert_eq!(current.metadata().table_uuid, table.metadata().table_uuid);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_second_requirement_never_publishes_first_head() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut commits = fixture.commits.clone();
    commits[1]
        .requirements
        .push(TableRequirement::AssertTableUuid {
            uuid: Uuid::new_v4(),
        });
    assert!(fixture.catalog.prepare_append_pair(commits).await.is_err());
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn dropping_a_prepared_pair_changes_no_head() -> TestResult {
    let fixture = Fixture::new().await?;
    let prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    drop(prepared);
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn reader_scope_excludes_pair_preparation() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let catalog = Arc::clone(&fixture.catalog);
    let commits = fixture.commits.clone();
    let mut pending = tokio::spawn(async move { catalog.prepare_append_pair(commits).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    for table in &fixture.tables {
        let Tabular::Table(current) = scope.load(table.identifier()).await? else {
            return Err("table".into());
        };
        assert_eq!(current.metadata().current_snapshot_id, None);
    }
    drop(scope);
    let prepared = tokio::time::timeout(Duration::from_secs(5), pending).await???;
    prepared.publish().await?;
    assert!(fixture.snapshots().await?.iter().all(Option::is_some));
    Ok(())
}

fn failure(stage: TestStage) -> Arc<TestHook> {
    Arc::new(TestHook {
        stage,
        fail: true,
        crash: false,
        pause: StdMutex::new(None),
    })
}

#[tokio::test]
async fn before_decision_failure_keeps_both_old_heads_readable() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    prepared.hook = Some(failure(TestStage::BeforeDecision));
    let error = prepared
        .publish()
        .await
        .expect_err("injected predecision failure");
    assert_eq!(error.outcome, PairOutcome::NotCommitted);
    assert_eq!(fixture.snapshots().await?, [None, None]);
    fixture.catalog.recover_append_pair().await?;
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn failure_after_first_head_fences_then_redoes_both_exact_heads() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    prepared.hook = Some(failure(TestStage::AfterFirstHead));
    let error = prepared
        .publish()
        .await
        .expect_err("injected postdecision failure");
    assert_eq!(error.outcome, PairOutcome::CommittedNeedsRecovery);
    assert!(fixture
        .root
        .path()
        .join("catalog/APP/A/metadata/v1.metadata.json")
        .exists());
    assert!(!fixture
        .root
        .path()
        .join("catalog/APP/B/metadata/v1.metadata.json")
        .exists());
    assert!(fixture.catalog.read_scope().await.is_err());
    assert!(Arc::clone(&fixture.catalog)
        .load_tabular(fixture.tables[0].identifier())
        .await
        .is_err());
    assert!(fixture
        .catalog
        .metadata_location(fixture.tables[1].identifier())
        .await
        .is_err());
    let reopened = Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(fixture.root.path()),
        )
        .await?,
    );
    reopened.recover_append_pair().await?;
    reopened.recover_append_pair().await?;
    let scope = reopened.read_scope().await?;
    for table in &fixture.tables {
        let Tabular::Table(current) = scope.load(table.identifier()).await? else {
            return Err("table".into());
        };
        assert!(current.metadata().current_snapshot_id.is_some());
        assert_eq!(current.metadata().table_uuid, table.metadata().table_uuid);
        assert_eq!(current.metadata().snapshots.len(), 1);
    }
    drop(scope);
    // A terminal journal must not roll back a later ordinary commit.
    let Tabular::Table(mut current) = Arc::clone(&reopened)
        .load_tabular(fixture.tables[0].identifier())
        .await?
    else {
        return Err("table".into());
    };
    current
        .new_transaction(None)
        .update_properties(vec![("ordinary-after-pair".to_owned(), "1".to_owned())])
        .commit()
        .await?;
    reopened.recover_append_pair().await?;
    let Tabular::Table(current) = Arc::clone(&reopened)
        .load_tabular(fixture.tables[0].identifier())
        .await?
    else {
        return Err("table".into());
    };
    assert_eq!(
        current
            .metadata()
            .properties
            .get("ordinary-after-pair")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(current.metadata().snapshots.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_waiter_retains_actual_worker_exclusion_and_requires_recovery() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    prepared.hook = Some(Arc::new(TestHook {
        stage: TestStage::AfterFirstHead,
        fail: false,
        crash: false,
        pause: StdMutex::new(Some((entered, release))),
    }));
    let caller = tokio::spawn(prepared.publish());
    tokio::time::timeout(Duration::from_secs(5), waiting).await??;
    caller.abort();
    assert!(caller.await.expect_err("caller aborted").is_cancelled());
    let catalog = Arc::clone(&fixture.catalog);
    let mut reader = tokio::spawn(async move { catalog.read_scope().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut reader)
            .await
            .is_err(),
        "cancelled waiter released the actual worker's writer guard"
    );
    assert!(!fixture
        .root
        .path()
        .join("catalog/APP/B/metadata/v1.metadata.json")
        .exists());
    resume.send(())?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), reader)
            .await??
            .is_err(),
        "unacknowledged publication must remain fenced"
    );
    fixture.catalog.recover_append_pair().await?;
    assert!(fixture.snapshots().await?.iter().all(Option::is_some));
    Ok(())
}

#[tokio::test]
async fn required_control_loss_and_foreign_successor_are_not_legacy_fallback() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    prepared.hook = Some(failure(TestStage::AfterFirstHead));
    assert!(prepared.publish().await.is_err());
    let first = fixture
        .root
        .path()
        .join("catalog/APP/A/metadata/v1.metadata.json");
    let exact = std::fs::read(&first)?;
    std::fs::write(&first, b"foreign-successor")?;
    assert!(fixture.catalog.recover_append_pair().await.is_err());
    assert!(fixture.catalog.read_scope().await.is_err());
    std::fs::write(&first, exact)?;
    fixture.catalog.recover_append_pair().await?;
    let journal = fixture.root.path().join("catalog").join(JOURNAL);
    std::fs::remove_file(journal)?;
    assert!(FileCatalog::new(
        "file:///catalog",
        ObjectStoreBuilder::filesystem(fixture.root.path())
    )
    .await
    .is_err());
    assert!(fixture.catalog.read_scope().await.is_err());
    Ok(())
}

#[tokio::test]
async fn duplicate_targets_and_nonfilesystem_pairs_are_explicit_refusals() -> TestResult {
    let fixture = Fixture::new().await?;
    assert!(fixture
        .catalog
        .prepare_append_pair([fixture.commits[0].clone(), fixture.commits[0].clone()])
        .await
        .is_err());
    assert_eq!(fixture.snapshots().await?, [None, None]);
    let memory =
        Arc::new(FileCatalog::new("memory://catalog", ObjectStoreBuilder::memory()).await?);
    assert!(memory
        .prepare_append_pair(fixture.commits.clone())
        .await
        .is_err());
    assert!(!fixture.root.path().join("catalog").join(WITNESS).exists());
    Ok(())
}

// Invoked only by the bounded child-process control below. Default test discovery
// returns without changing process state.
#[tokio::test]
async fn prepared_pair_crash_helper() -> TestResult {
    let Some(root) = std::env::var_os("ICEBERG_PAIR_TEST_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let commits: [CommitTable; 2] =
        serde_json::from_slice(&std::fs::read(root.join("pair-input.json"))?)?;
    let catalog =
        Arc::new(FileCatalog::new("file:///catalog", ObjectStoreBuilder::filesystem(&root)).await?);
    let mut prepared = catalog.prepare_append_pair(commits).await?;
    let stage = match std::env::var("ICEBERG_PAIR_TEST_STAGE")?.as_str() {
        "ready" => TestStage::BeforeDecision,
        "decision" => TestStage::AfterDecision,
        "first" => TestStage::AfterFirstHead,
        _ => return Err("unknown child crash stage".into()),
    };
    prepared.hook = Some(Arc::new(TestHook {
        stage,
        fail: false,
        crash: true,
        pause: StdMutex::new(None),
    }));
    prepared.publish().await?;
    Err("child did not terminate at its actual worker checkpoint".into())
}

#[tokio::test]
async fn process_exit_recovers_exact_old_or_complete_new_pair() -> TestResult {
    for stage in ["ready", "decision", "first"] {
        let fixture = Fixture::new().await?;
        std::fs::write(
            fixture.root.path().join("pair-input.json"),
            serde_json::to_vec(&fixture.commits)?,
        )?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "prepared_pair::tests::prepared_pair_crash_helper",
                "--nocapture",
            ])
            .env("ICEBERG_PAIR_TEST_ROOT", fixture.root.path())
            .env("ICEBERG_PAIR_TEST_STAGE", stage)
            .spawn()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(format!("prepared-pair child timed out at {stage}").into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(status.code(), Some(86), "{stage}");
        let reopened = Arc::new(
            FileCatalog::new(
                "file:///catalog",
                ObjectStoreBuilder::filesystem(fixture.root.path()),
            )
            .await?,
        );
        for _ in 0..2 {
            reopened.recover_append_pair().await?;
            let scope = reopened.read_scope().await?;
            for table in &fixture.tables {
                let Tabular::Table(current) = scope.load(table.identifier()).await? else {
                    return Err("table".into());
                };
                assert_eq!(
                    current.metadata().snapshots.len(),
                    usize::from(stage != "ready"),
                    "{stage}"
                );
                assert_eq!(
                    current.metadata().current_snapshot_id.is_some(),
                    stage != "ready",
                    "{stage}"
                );
                assert_eq!(current.metadata().table_uuid, table.metadata().table_uuid);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn configured_native_root_owns_the_fence_and_exclusion_identity() -> TestResult {
    let first = Fixture::new().await?;
    let other = Fixture::new().await?;
    let alias = Arc::new(
        FileCatalog::new(
            &format!("file://{}/catalog", first.root.path().display()),
            ObjectStoreBuilder::filesystem("/"),
        )
        .await?,
    );
    let prepared = first
        .catalog
        .prepare_append_pair(first.commits.clone())
        .await?;
    // Same native root through a different store prefix/URI spelling must share
    // actual mutation exclusion; another native root using file:///catalog must not.
    let namespace = Namespace::try_new(&["WAITING".to_owned()])?;
    let alias_job = Arc::clone(&alias);
    let mut waiting =
        tokio::spawn(async move { alias_job.create_namespace(&namespace, None).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiting)
            .await
            .is_err()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), other.snapshots()).await??,
        [None, None]
    );
    drop(prepared);
    tokio::time::timeout(Duration::from_secs(5), waiting).await???;

    let mut prepared = first
        .catalog
        .prepare_append_pair(first.commits.clone())
        .await?;
    prepared.hook = Some(failure(TestStage::AfterFirstHead));
    assert!(prepared.publish().await.is_err());
    assert!(alias.read_scope().await.is_err());
    assert_eq!(other.snapshots().await?, [None, None]);
    first.catalog.recover_append_pair().await?;
    assert!(alias.read_scope().await.is_ok());
    Ok(())
}

async fn candidate_artifact_paths(
    fixture: &Fixture,
    prepared: &PreparedAppendPair,
) -> TestResult<[PathBuf; 3]> {
    let table = Table::new(
        fixture.tables[1].identifier().clone(),
        Arc::clone(&fixture.catalog) as Arc<dyn Catalog>,
        fixture.tables[1].object_store(),
        prepared.entries[1].new_metadata.clone(),
    )
    .await?;
    let snapshot = table
        .metadata()
        .current_snapshot(None)?
        .ok_or("candidate snapshot")?;
    let list = fixture.catalog.pair_native(snapshot.manifest_list())?;
    let manifests = table.manifests(None, None).await?;
    let manifest = fixture
        .catalog
        .pair_native(&manifests.first().ok_or("candidate manifest")?.manifest_path)?;
    Ok([
        fixture.root.path().join("catalog/APP/B/data/input.parquet"),
        manifest,
        list,
    ])
}

#[tokio::test]
async fn pending_commit_recovery_validates_every_artifact_before_any_install() -> TestResult {
    for stage in [TestStage::AfterDecision, TestStage::AfterFirstHead] {
        for kind in 0..3 {
            for missing in [true, false] {
                let fixture = Fixture::new().await?;
                let mut prepared = fixture
                    .catalog
                    .prepare_append_pair(fixture.commits.clone())
                    .await?;
                let path = candidate_artifact_paths(&fixture, &prepared).await?[kind].clone();
                prepared.hook = Some(failure(stage));
                assert_eq!(
                    prepared
                        .publish()
                        .await
                        .expect_err("pending decision")
                        .outcome,
                    PairOutcome::CommittedNeedsRecovery
                );
                let exact = std::fs::read(&path)?;
                if missing {
                    std::fs::remove_file(&path)?;
                } else {
                    let mut corrupt = exact.clone();
                    *corrupt.first_mut().ok_or("nonempty artifact")? ^= 1;
                    std::fs::write(&path, corrupt)?;
                }
                assert!(
                    fixture.catalog.recover_append_pair().await.is_err(),
                    "stage={stage:?}, kind={kind}, missing={missing}"
                );
                assert!(fixture.catalog.read_scope().await.is_err());
                assert!(
                    !fixture
                        .root
                        .path()
                        .join("catalog/APP/B/metadata/v1.metadata.json")
                        .exists(),
                    "bad artifact must be checked before installing second head"
                );
                if stage == TestStage::AfterDecision {
                    assert!(
                        !fixture
                            .root
                            .path()
                            .join("catalog/APP/A/metadata/v1.metadata.json")
                            .exists(),
                        "bad artifact must be checked before installing first head"
                    );
                }
                std::fs::write(path, exact)?;
                fixture.catalog.recover_append_pair().await?;
                assert!(fixture.snapshots().await?.iter().all(Option::is_some));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn journal_typed_predecessor_must_match_its_exact_byte_image() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    prepared.hook = Some(failure(TestStage::AfterFirstHead));
    assert!(prepared.publish().await.is_err());
    let path = fixture.root.path().join("catalog").join(JOURNAL);
    let exact = std::fs::read(&path)?;
    let mut journal: serde_json::Value = serde_json::from_slice(&exact)?;
    let entry = &mut journal["entries"][1];
    entry["old_metadata"]["properties"]["forged-annotation"] =
        serde_json::json!("not-in-predecessor-bytes");
    let mut next: serde_json::Value =
        serde_json::from_str(entry["new_json"].as_str().ok_or("new JSON")?)?;
    next["properties"]["forged-annotation"] = serde_json::json!("not-in-predecessor-bytes");
    entry["new_json"] = serde_json::to_string(&next)?.into();
    std::fs::write(&path, serde_json::to_vec(&journal)?)?;
    assert!(
        fixture.catalog.recover_append_pair().await.is_err(),
        "a consistent typed old/new transition cannot invent predecessor properties"
    );
    assert!(!fixture
        .root
        .path()
        .join("catalog/APP/B/metadata/v1.metadata.json")
        .exists());
    assert!(fixture.catalog.read_scope().await.is_err());
    std::fs::write(path, exact)?;
    fixture.catalog.recover_append_pair().await?;
    Ok(())
}

#[tokio::test]
async fn pair_v1_refuses_gzipped_predecessors_before_control_effects() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut table = fixture.tables[0].clone();
    table
        .new_transaction(None)
        .update_properties(vec![(
            "write.metadata.compression-codec".to_owned(),
            "gzip".to_owned(),
        )])
        .commit()
        .await?;
    assert!(fixture
        .catalog
        .metadata_location(table.identifier())
        .await?
        .ends_with(".gz.metadata.json"));
    assert!(fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await
        .is_err());
    assert!(!fixture.root.path().join("catalog").join(WITNESS).exists());
    assert!(!fixture.root.path().join("catalog").join(JOURNAL).exists());
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn duplicate_physical_uuid_is_refused_before_a_prepared_capability() -> TestResult {
    let fixture = Fixture::new().await?;
    let uuid = fixture.tables[0].metadata().table_uuid;
    let path = fixture
        .root
        .path()
        .join("catalog/APP/B/metadata/v0.metadata.json");
    let mut metadata: TableMetadata = serde_json::from_slice(&std::fs::read(&path)?)?;
    metadata.table_uuid = uuid;
    std::fs::write(path, serde_json::to_vec(&metadata)?)?;
    std::fs::write(
        fixture
            .root
            .path()
            .join("catalog/APP/B/metadata/table-uuid"),
        uuid.as_bytes(),
    )?;
    let mut commits = fixture.commits.clone();
    for requirement in &mut commits[1].requirements {
        if let TableRequirement::AssertTableUuid { uuid: required } = requirement {
            *required = uuid;
        }
    }
    assert!(fixture.catalog.prepare_append_pair(commits).await.is_err());
    assert!(!fixture.root.path().join("catalog").join(WITNESS).exists());
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn ordinary_nonfilesystem_load_keeps_its_existing_unlocked_path() -> TestResult {
    let memory =
        Arc::new(FileCatalog::new("/pair-memory-control", ObjectStoreBuilder::memory()).await?);
    let namespace = Namespace::try_new(&["APP".to_owned()])?;
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
        .build()?;
    let mut builder = CreateTableBuilder::default();
    let table = builder
        .with_name("T")
        .with_schema(schema)
        .build(&namespace, Arc::clone(&memory) as Arc<dyn Catalog>)
        .await?;
    let writer = memory.catalog_mutation_lock().write_owned().await;
    let fresh = tokio::time::timeout(
        Duration::from_secs(1),
        FileCatalog::new("/pair-memory-control", memory.object_store.clone()),
    )
    .await??;
    drop(fresh);
    let loaded = tokio::time::timeout(
        Duration::from_secs(1),
        Arc::clone(&memory).load_tabular(table.identifier()),
    )
    .await??;
    let Tabular::Table(loaded) = loaded else {
        return Err("table".into());
    };
    assert_eq!(loaded.metadata().table_uuid, table.metadata().table_uuid);
    drop(writer);
    assert!(
        memory.read_scope().await.is_err(),
        "new pair snapshot scopes are explicitly filesystem-only"
    );
    Ok(())
}

#[tokio::test]
async fn publisher_rechecks_distinct_identities_before_control_effects() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    // A private-candidate invariant must still be checked at the publication
    // boundary, independently of preparation and recovery decoding.
    prepared.entries[1] = prepared.entries[0].clone();
    let error = prepared
        .publish()
        .await
        .expect_err("duplicate publication candidate");
    assert_eq!(error.outcome, PairOutcome::NotCommitted);
    assert_eq!(fixture.snapshots().await?, [None, None]);
    assert!(!fixture.root.path().join("catalog").join(WITNESS).exists());
    Ok(())
}
