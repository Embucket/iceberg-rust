//! Source-before-implementation controls for one existing-property V3 rename.
//! These controls are authored, not compiled or executed, at the first checkpoint.

use super::*;
use crate::view_rename_tests::Fixture;
use iceberg_rust::catalog::ExistingViewPropertyReplacement;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const KEY: &str = "embucket.sql.view-definition";
const BEFORE: &str = "CREATE VIEW RV AS SELECT 1 AS X";
const AFTER: &str = "CREATE VIEW RV2 AS SELECT 1 AS X";

fn replacement() -> Result<ExistingViewPropertyReplacement, IcebergError> {
    ExistingViewPropertyReplacement::try_new(KEY.into(), BEFORE.into(), AFTER.into())
}

async fn prepared(fixture: &Fixture) -> TestResult<PreparedViewRename> {
    Ok(fixture
        .catalog
        .prepare_view_rename_with_property(
            fixture.view.identifier(),
            &fixture.destination,
            fixture.view.metadata(),
            replacement()?,
        )
        .await?)
}

async fn fresh(fixture: &Fixture) -> TestResult<Arc<FileCatalog>> {
    Ok(Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(fixture.root.path()),
        )
        .await?,
    ))
}

fn expected(fixture: &Fixture) -> ViewMetadata {
    let mut image = fixture.view.metadata().clone();
    image.location.push('2');
    image.properties.insert(KEY.into(), AFTER.into());
    image
}

fn files(path: &NativePath) -> TestResult<Vec<(PathBuf, Vec<u8>)>> {
    fn visit(
        root: &NativePath,
        path: &NativePath,
        out: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> TestResult {
        for entry in std::fs::read_dir(path)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(root, &path, out)?;
            } else if metadata.is_file() {
                out.push((path.strip_prefix(root)?.into(), std::fs::read(path)?));
            } else {
                return Err("unexpected fixture path kind".into());
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(path, path, &mut result)?;
    result.sort();
    Ok(result)
}

async fn destination(fixture: &Fixture, catalog: &Arc<FileCatalog>) -> TestResult {
    let scope = catalog.read_scope().await?;
    assert!(!scope.tabular_exists(fixture.view.identifier()).await?);
    let Tabular::View(view) = scope.load(&fixture.destination).await? else {
        return Err("renamed destination is not a view".into());
    };
    assert_eq!(view.metadata(), &expected(fixture));
    Ok(())
}

#[tokio::test]
async fn checked_property_rename_dyn_dispatch_preserves_every_other_field_and_reopens() -> TestResult
{
    let fixture = Fixture::new().await?;
    let root = fixture.catalog.pair_root()?;
    let before = files(&root.path)?;
    let catalog: Arc<dyn Catalog> = Arc::clone(&fixture.catalog) as Arc<dyn Catalog>;
    let view = catalog
        .rename_view_from_metadata_with_property(
            fixture.view.identifier(),
            &fixture.destination,
            fixture.view.metadata(),
            replacement()?,
        )
        .await?;
    assert_eq!(view.identifier(), &fixture.destination);
    assert_eq!(view.metadata(), &expected(&fixture));
    // Historical JSON remains byte-identical; only the new current image carries the edit.
    for (path, bytes) in before {
        if path.parent() == Some(NativePath::new("APP/RV/metadata"))
            && path
                .file_name()
                .is_some_and(|name| name != "version-hint.text")
        {
            let renamed = root
                .path
                .join("APP/RV2/metadata")
                .join(path.file_name().ok_or("history leaf")?);
            assert_eq!(std::fs::read(renamed)?, bytes);
        }
    }
    destination(&fixture, &fresh(&fixture).await?).await?;
    Ok(())
}

#[test]
fn property_replacement_refuses_unbounded_or_ambiguous_descriptions() {
    for (key, old, new) in [
        ("".into(), BEFORE.into(), AFTER.into()),
        ("x".repeat(256), BEFORE.into(), AFTER.into()),
        ("bad/key".into(), BEFORE.into(), AFTER.into()),
        ("é".into(), BEFORE.into(), AFTER.into()),
        (KEY.into(), "".into(), AFTER.into()),
        (KEY.into(), BEFORE.into(), "".into()),
        (KEY.into(), "x".repeat(65_537), AFTER.into()),
        (KEY.into(), BEFORE.into(), "x".repeat(65_537)),
        (KEY.into(), "before\0suffix".into(), AFTER.into()),
        (KEY.into(), BEFORE.into(), "after\0suffix".into()),
        (KEY.into(), BEFORE.into(), BEFORE.into()),
    ] {
        assert!(ExistingViewPropertyReplacement::try_new(key, old, new).is_err());
    }
    // SQL whitespace is an opaque property value, not native SQL parsing.
    assert!(ExistingViewPropertyReplacement::try_new(
        KEY.into(),
        "before\n\t".into(),
        "after\n\t".into()
    )
    .is_ok());
    assert!(ExistingViewPropertyReplacement::try_new(
        "x".repeat(255),
        "a".repeat(65_536),
        "b".repeat(65_536)
    )
    .is_ok());
}

#[tokio::test]
async fn property_preparation_rejects_wrong_full_before_key_and_value_without_effects() -> TestResult
{
    let fixture = Fixture::new().await?;
    let original = files(fixture.root.path())?;
    for change in 0..7 {
        let mut before = fixture.view.metadata().clone();
        let edit = match change {
            0 => ExistingViewPropertyReplacement::try_new(
                "absent".into(),
                BEFORE.into(),
                AFTER.into(),
            )?,
            1 => {
                ExistingViewPropertyReplacement::try_new(KEY.into(), "stale".into(), AFTER.into())?
            }
            2 => {
                before.view_uuid = Uuid::new_v4();
                replacement()?
            }
            3 => {
                before
                    .properties
                    .insert("unrelated".into(), "forged".into());
                replacement()?
            }
            4 => {
                before.location.push_str("foreign");
                replacement()?
            }
            5 => {
                before.current_version_id += 1;
                replacement()?
            }
            _ => {
                before.schemas.clear();
                replacement()?
            }
        };
        let result = fixture
            .catalog
            .prepare_view_rename_with_property(
                fixture.view.identifier(),
                &fixture.destination,
                &before,
                edit,
            )
            .await;
        assert!(result.is_err(), "change {change}");
        assert_eq!(files(fixture.root.path())?, original, "change {change}");
        assert!(fixture.catalog.read_scope().await.is_ok());
    }
    Ok(())
}

#[tokio::test]
async fn checked_property_rename_rejects_an_actual_intervening_update_from_a_warm_image(
) -> TestResult {
    let fixture = Fixture::new().await?;
    let old = fixture.view.metadata().clone();
    let peer = fresh(&fixture).await?;
    let Tabular::View(mut view) = Arc::clone(&peer)
        .load_tabular(fixture.view.identifier())
        .await?
    else {
        return Err("peer view".into());
    };
    view.new_transaction(None)
        .update_properties(vec![("unrelated".into(), "actual-new-value".into())])
        .commit()
        .await?;
    assert_eq!(view.metadata().view_uuid, old.view_uuid);
    let original = files(fixture.root.path())?;
    let catalog: Arc<dyn Catalog> = Arc::clone(&fixture.catalog) as Arc<dyn Catalog>;
    assert!(matches!(
        catalog
            .rename_view_from_metadata_with_property(
                fixture.view.identifier(),
                &fixture.destination,
                &old,
                replacement()?
            )
            .await,
        Err(IcebergError::CommitConflict(_))
    ));
    assert_eq!(files(fixture.root.path())?, original);
    assert!(fixture.catalog.read_scope().await.is_ok());
    let memory: Arc<dyn Catalog> =
        Arc::new(FileCatalog::new("memory://catalog", ObjectStoreBuilder::memory()).await?);
    assert!(matches!(
        memory
            .rename_view_from_metadata_with_property(
                fixture.view.identifier(),
                &fixture.destination,
                &old,
                replacement()?
            )
            .await,
        Err(IcebergError::NotSupported(_))
    ));
    Ok(())
}

#[tokio::test]
async fn v3_descriptor_binds_exact_edit_and_rejects_other_successor_changes() -> TestResult {
    let fixture = Fixture::new().await?;
    let p = prepared(&fixture).await?;
    let original = p.plan.encode(&p.root, "commit")?;
    let encoded: Value = serde_json::from_slice(&original)?;
    assert_eq!(encoded["version"], 3);
    assert_eq!(encoded["kind"], "view-rename-property");
    assert_eq!(
        encoded["property_replacement"],
        json!({"key":KEY,"expected":BEFORE,"replacement":AFTER})
    );
    assert!(is_record(&original)?);
    assert_eq!(decode(&p.root, &original)?.1.new, expected(&fixture));
    for change in 0..13 {
        let mut mutated = encoded.clone();
        match change {
            0 => mutated["version"] = json!(4),
            1 => mutated["kind"] = json!("unknown"),
            2 => mutated["property_replacement"]["key"] = json!("absent"),
            3 => mutated["property_replacement"]["expected"] = json!("stale"),
            4 => mutated["property_replacement"]["replacement"] = json!("different"),
            5 => {
                mutated
                    .as_object_mut()
                    .ok_or("object")?
                    .remove("property_replacement");
            }
            6 => mutated["property_replacement"]["extra"] = json!(true),
            7 => mutated["version"] = json!(2),
            8..=12 => {
                let mut image = expected(&fixture);
                match change {
                    8 => image.view_uuid = Uuid::new_v4(),
                    9 => {
                        image.properties.insert("unrelated".into(), "forged".into());
                    }
                    10 => image.schemas.clear(),
                    11 => image.current_version_id += 1,
                    _ => image.version_log.push(
                        iceberg_rust::spec::view_metadata::VersionLogStruct {
                            timestamp_ms: 0,
                            version_id: image.current_version_id,
                        },
                    ),
                }
                mutated["new_json"] = json!(serde_json::to_string(&image)?);
            }
            _ => unreachable!(),
        }
        assert_ne!(mutated, encoded, "change {change} must alter the record");
        assert!(
            decode(&p.root, &serde_json::to_vec(&mutated)?).is_err(),
            "change {change}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn v2_default_encoder_and_location_only_semantics_remain_exact() -> TestResult {
    let fixture = Fixture::new().await?;
    let p = fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
        .await?;
    let plan = &p.plan;
    let artifacts = plan
        .history
        .iter()
        .map(|r| {
            json!({"path":r.relative,"length":r.length,
        "checksum_fnv1a64":format!("{:016x}",r.checksum)})
        })
        .collect::<Vec<_>>();
    let golden = serde_json::to_vec(
        &json!({"version":2,"kind":"view-rename","catalog":p.root.key,
        "operation_id":plan.operation,"phase":"commit","source":plan.source,"destination":plan.destination,
        "old_head":plan.old_head,"new_head":plan.new_head,"old_hex":hex(&plan.old_bytes),
        "new_json":std::str::from_utf8(&plan.new_bytes)?,
        "source_owner_hex":plan.old_owner.as_ref().map(|bytes|hex(bytes)),
        "artifacts":artifacts,"artifact_count":plan.history.len(),
        "artifact_receipts_fnv1a64":format!("{:016x}",artifact_receipt_checksum(&plan.history))}),
    )?;
    assert_eq!(plan.encode(&p.root, "commit")?, golden);
    assert_eq!(
        decode(&p.root, &golden)?.1.new.properties,
        fixture.view.metadata().properties
    );
    p.publish().await?;
    let Tabular::View(view) = fresh(&fixture)
        .await?
        .load_tabular(&fixture.destination)
        .await?
    else {
        return Err("old API destination view".into());
    };
    assert_eq!(
        view.metadata().properties,
        fixture.view.metadata().properties
    );
    Ok(())
}

#[tokio::test]
async fn terminal_v3_allows_a_later_location_only_v2_without_undoing_the_edit() -> TestResult {
    let fixture = Fixture::new().await?;
    prepared(&fixture).await?.publish().await?;
    let next = Identifier::new(fixture.destination.namespace(), "RV3");
    let p = fixture
        .catalog
        .prepare_view_rename(&fixture.destination, &next)
        .await?;
    let mut image = expected(&fixture);
    image.location.truncate(image.location.len() - 1);
    image.location.push('3');
    let bytes = p.plan.encode(&p.root, "commit")?;
    let value: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(value["version"], 2);
    assert_eq!(value["kind"], "view-rename");
    assert!(value.get("property_replacement").is_none());
    p.publish().await?;
    let reopened = fresh(&fixture).await?;
    let Tabular::View(view) = reopened.load_tabular(&next).await? else {
        return Err("second rename view".into());
    };
    assert_eq!(view.metadata(), &image);
    Ok(())
}

fn fail(stage: TestStage) -> Arc<TestHook> {
    Arc::new(TestHook {
        stage,
        fail: true,
        crash: false,
        pause: StdMutex::new(None),
    })
}

#[tokio::test]
async fn property_fault_boundaries_recover_exact_decided_image_and_preserve_history() -> TestResult
{
    for stage in [
        TestStage::BeforeDecision,
        TestStage::AfterDecision,
        TestStage::AfterFirstHead,
        TestStage::AfterFirstSourceFile,
        TestStage::AfterSourceRetired,
    ] {
        let fixture = Fixture::new().await?;
        let mut p = prepared(&fixture).await?;
        let plan = p.plan.clone();
        let root = fixture.catalog.pair_root()?;
        let paths = plan.places(&root)?;
        let history = plan
            .history
            .iter()
            .filter(|r| r.relative.rsplit('/').next() != Some("version-hint.text"))
            .map(|r| {
                let path = r.path(&root)?;
                Ok((leaf(&path)?.to_owned(), root.read(&path, MAX_METADATA)?))
            })
            .collect::<Result<Vec<_>, IcebergError>>()?;
        p.hook = Some(fail(stage));
        let error = p.publish().await.expect_err("injected real worker fault");
        let committed = stage != TestStage::BeforeDecision;
        assert_eq!(
            error.outcome,
            if committed {
                PairOutcome::CommittedNeedsRecovery
            } else {
                PairOutcome::NotCommitted
            }
        );
        if committed {
            assert!(fixture.catalog.read_scope().await.is_err());
        }
        let reopened = fresh(&fixture).await?;
        for _ in 0..2 {
            reopened.recover_append_pair().await?;
            if committed {
                destination(&fixture, &reopened).await?;
            } else {
                assert!(missing(&paths.destination)?);
                let Tabular::View(view) = Arc::clone(&reopened)
                    .load_tabular(fixture.view.identifier())
                    .await?
                else {
                    return Err("old source".into());
                };
                assert_eq!(view.metadata(), fixture.view.metadata());
            }
        }
        let metadata = if committed {
            paths.destination.join("metadata")
        } else {
            paths.metadata
        };
        for (name, bytes) in history {
            assert_eq!(std::fs::read(metadata.join(name))?, bytes);
        }
    }
    Ok(())
}

#[tokio::test]
async fn property_waiter_cancellation_retains_actual_worker_and_fences_lost_response() -> TestResult
{
    let fixture = Fixture::new().await?;
    let mut p = prepared(&fixture).await?;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    p.hook = Some(Arc::new(TestHook {
        stage: TestStage::AfterFirstHead,
        fail: false,
        crash: false,
        pause: StdMutex::new(Some((entered, release))),
    }));
    let caller = tokio::spawn(p.publish());
    tokio::time::timeout(Duration::from_secs(5), waiting).await??;
    caller.abort();
    assert!(caller.await.expect_err("cancelled waiter").is_cancelled());
    let catalog = Arc::clone(&fixture.catalog);
    let mut reader = tokio::spawn(async move { catalog.read_scope().await });
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut reader)
        .await
        .is_err());
    resume.send(())?;
    assert!(tokio::time::timeout(Duration::from_secs(5), reader)
        .await??
        .is_err());
    fixture.catalog.recover_append_pair().await?;
    destination(&fixture, &fresh(&fixture).await?).await?;
    Ok(())
}

#[tokio::test]
async fn pending_property_recovery_refuses_changed_descriptor_or_successor_before_cleanup(
) -> TestResult {
    for change in 0..4 {
        let fixture = Fixture::new().await?;
        let mut p = prepared(&fixture).await?;
        let root = fixture.catalog.pair_root()?;
        let paths = p.plan.places(&root)?;
        p.hook = Some(fail(TestStage::AfterDecision));
        assert_eq!(
            p.publish().await.expect_err("decided fault").outcome,
            PairOutcome::CommittedNeedsRecovery
        );
        let journal = root.read(&root.file(JOURNAL), MAX_JOURNAL)?;
        let original = files(&root.path)?;
        let mut value: Value = serde_json::from_slice(&journal)?;
        match change {
            0 => value["version"] = json!(4),
            1 => value["kind"] = json!("unknown"),
            2 => value["property_replacement"]["replacement"] = json!("forged"),
            _ => {
                let mut image = expected(&fixture);
                image.properties.insert("unrelated".into(), "forged".into());
                value["new_json"] = json!(serde_json::to_string(&image)?);
            }
        }
        let corrupt = serde_json::to_vec(&value)?;
        std::fs::write(root.file(JOURNAL), &corrupt)?;
        let changed = files(&root.path)?;
        assert!(fixture.catalog.recover_append_pair().await.is_err());
        assert!(fresh(&fixture).await.is_err());
        assert_eq!(
            files(&root.path)?,
            changed,
            "no evidence cleanup on refusal"
        );
        assert!(missing(&paths.destination)?);
        assert_eq!(root.read(&root.file(JOURNAL), MAX_JOURNAL)?, corrupt);
        std::fs::write(root.file(JOURNAL), journal)?;
        assert_eq!(files(&root.path)?, original);
        fixture.catalog.recover_append_pair().await?;
        destination(&fixture, &fresh(&fixture).await?).await?;
    }
    Ok(())
}

// Inert under ordinary discovery. The parent invokes this exact test in a child
// and terminates only from the actual blocking publisher's fault checkpoint.
#[tokio::test]
async fn view_property_crash_helper() -> TestResult {
    let Some(root) = std::env::var_os("ICEBERG_VIEW_PROPERTY_TEST_ROOT") else {
        return Ok(());
    };
    let catalog = Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(PathBuf::from(root)),
        )
        .await?,
    );
    let namespace = Namespace::try_new(&["APP".to_owned()])?;
    let source = Identifier::new(&namespace, "RV");
    let destination = Identifier::new(&namespace, "RV2");
    let Tabular::View(view) = Arc::clone(&catalog).load_tabular(&source).await? else {
        return Err("child source view".into());
    };
    let mut p = catalog
        .prepare_view_rename_with_property(&source, &destination, view.metadata(), replacement()?)
        .await?;
    let stage = match std::env::var("ICEBERG_VIEW_PROPERTY_TEST_STAGE")?.as_str() {
        "ready" => TestStage::BeforeDecision,
        "decision" => TestStage::AfterDecision,
        "destination" => TestStage::AfterFirstHead,
        "retiring" => TestStage::AfterFirstSourceFile,
        "retired" => TestStage::AfterSourceRetired,
        _ => return Err("unknown child stage".into()),
    };
    p.hook = Some(Arc::new(TestHook {
        stage,
        fail: false,
        crash: true,
        pause: StdMutex::new(None),
    }));
    p.publish().await?;
    Err("actual property publisher did not exit".into())
}

#[tokio::test]
async fn property_process_exit_recovers_the_same_decision_and_edit_idempotently() -> TestResult {
    for stage in ["ready", "decision", "destination", "retiring", "retired"] {
        let fixture = Fixture::new().await?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "prepared_pair::view_rename::property_tests::view_property_crash_helper",
                "--nocapture",
            ])
            .env("ICEBERG_VIEW_PROPERTY_TEST_ROOT", fixture.root.path())
            .env("ICEBERG_VIEW_PROPERTY_TEST_STAGE", stage)
            .spawn()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(format!("property publisher child timeout: {stage}").into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(status.code(), Some(86), "{stage}");
        let reopened = fresh(&fixture).await?;
        for _ in 0..2 {
            reopened.recover_append_pair().await?;
            if stage == "ready" {
                let scope = reopened.read_scope().await?;
                assert!(!scope.tabular_exists(&fixture.destination).await?);
                let Tabular::View(view) = scope.load(fixture.view.identifier()).await? else {
                    return Err("source after ready recovery".into());
                };
                assert_eq!(view.metadata(), fixture.view.metadata());
            } else {
                destination(&fixture, &reopened).await?;
            }
        }
    }
    Ok(())
}
