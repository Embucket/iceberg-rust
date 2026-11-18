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
    for change in 0..10 {
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
            8 | 9 => {
                let mut image = expected(&fixture);
                if change == 8 {
                    image.view_uuid = Uuid::new_v4();
                } else {
                    image.properties.insert("unrelated".into(), "forged".into());
                }
                mutated["new_json"] = json!(serde_json::to_string(&image)?);
            }
            _ => unreachable!(),
        }
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
