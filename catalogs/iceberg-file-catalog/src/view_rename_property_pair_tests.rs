//! Test-first source controls for exactly two existing-property V4 replacements.
//! No compilation or execution is claimed at the source checkpoint.

use super::*;
use iceberg_rust::catalog::{ExistingViewPropertyPairReplacement, ExistingViewPropertyReplacement};
use iceberg_rust::spec::schema::Schema;
use iceberg_rust::spec::types::{PrimitiveType, StructField, Type};
use iceberg_rust::spec::view_metadata::{Version, ViewRepresentation};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const KEY: &str = "embucket.sql.view-source-sql.v1";
const OPTIONS_KEY: &str = "embucket.sql.view-options.v1";
const OPTIONS_BEFORE: &str = r#"{"last_altered_ms":17,"last_ddl_ms":17,"opaque":"stable"}"#;
const OPTIONS_AFTER: &str = r#"{"last_altered_ms":29,"last_ddl_ms":29,"opaque":"stable"}"#;
const BEFORE: &str = "CREATE VIEW RV AS SELECT 1 AS X";
const AFTER: &str = "CREATE VIEW RV2 AS SELECT 1 AS X";

fn source_edit(
    key: &str,
    old: &str,
    new: &str,
) -> Result<ExistingViewPropertyReplacement, IcebergError> {
    ExistingViewPropertyReplacement::try_new(key.into(), old.into(), new.into())
}

fn replacement() -> Result<ExistingViewPropertyPairReplacement, IcebergError> {
    ExistingViewPropertyPairReplacement::try_new([
        source_edit(KEY, BEFORE, AFTER)?,
        source_edit(OPTIONS_KEY, OPTIONS_BEFORE, OPTIONS_AFTER)?,
    ])
}

struct Fixture {
    root: tempfile::TempDir,
    catalog: Arc<FileCatalog>,
    view: View,
    destination: Identifier,
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
        let namespace = Namespace::try_new(&["APP".to_owned()])?;
        catalog.create_namespace(&namespace, None).await?;
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "X".to_owned(),
                required: false,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()?;
        let mut view = View::builder()
            .with_name("RV")
            .with_schema(schema.clone())
            .with_view_version(
                Version::builder()
                    .with_representation(ViewRepresentation::sql(
                        "SELECT 1 AS X",
                        Some("snowflake"),
                    ))
                    .build()?,
            )
            .with_property((KEY.to_owned(), BEFORE.to_owned()))
            .with_property((
                "embucket.sql.view-options.v1".to_owned(),
                OPTIONS_BEFORE.to_owned(),
            ))
            .with_property((
                "embucket.sql.view-definition".into(),
                "SELECT 1 AS X /* unchanged body */".into(),
            ))
            .with_property(("unrelated".into(), "preserve-me".into()))
            .build(&namespace, Arc::clone(&catalog) as Arc<dyn Catalog>)
            .await?;
        view.new_transaction(None)
            .update_representations(
                vec![ViewRepresentation::sql("SELECT 2 AS X", Some("snowflake"))],
                schema.fields().clone(),
            )
            .commit()
            .await?;
        Ok(Self {
            root,
            catalog,
            view,
            destination: Identifier::new(&namespace, "RV2"),
        })
    }
}

async fn prepared(fixture: &Fixture) -> TestResult<PreparedViewRename> {
    Ok(fixture
        .catalog
        .prepare_view_rename_with_property_pair(
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
        .properties
        .insert(OPTIONS_KEY.into(), OPTIONS_AFTER.into());
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
        .rename_view_from_metadata_with_property_pair(
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
fn property_pair_requires_two_distinct_canonical_bounded_descriptions() -> TestResult {
    let pair = replacement()?;
    assert_eq!(pair.replacements()[0].key(), OPTIONS_KEY);
    assert_eq!(pair.replacements()[1].key(), KEY);
    assert!(ExistingViewPropertyPairReplacement::try_new([
        source_edit(KEY, BEFORE, AFTER)?,
        source_edit(KEY, "old", "new")?
    ])
    .is_err());
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
    let maximum = ExistingViewPropertyPairReplacement::try_new([
        source_edit(&"a".repeat(255), &"x".repeat(65_536), &"y".repeat(65_536))?,
        source_edit(&"b".repeat(255), "before\n\t", "after\n\t")?,
    ])?;
    assert_eq!(maximum.replacements().len(), 2);
    Ok(())
}

#[tokio::test]
async fn property_preparation_rejects_wrong_full_before_key_and_value_without_effects() -> TestResult
{
    let fixture = Fixture::new().await?;
    let original = files(fixture.root.path())?;
    for change in 0..9 {
        let mut before = fixture.view.metadata().clone();
        let edit = match change {
            0 => ExistingViewPropertyPairReplacement::try_new([
                source_edit("absent", BEFORE, AFTER)?,
                source_edit(OPTIONS_KEY, OPTIONS_BEFORE, OPTIONS_AFTER)?,
            ])?,
            1 => ExistingViewPropertyPairReplacement::try_new([
                source_edit(KEY, "stale", AFTER)?,
                source_edit(OPTIONS_KEY, OPTIONS_BEFORE, OPTIONS_AFTER)?,
            ])?,
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
            6 => {
                before.schemas.clear();
                replacement()?
            }
            7 => ExistingViewPropertyPairReplacement::try_new([
                source_edit(KEY, BEFORE, AFTER)?,
                source_edit(OPTIONS_KEY, "stale", OPTIONS_AFTER)?,
            ])?,
            8 => ExistingViewPropertyPairReplacement::try_new([
                source_edit(KEY, BEFORE, AFTER)?,
                source_edit("absent", OPTIONS_BEFORE, OPTIONS_AFTER)?,
            ])?,
            _ => unreachable!(),
        };
        let result = fixture
            .catalog
            .prepare_view_rename_with_property_pair(
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
            .rename_view_from_metadata_with_property_pair(
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
            .rename_view_from_metadata_with_property_pair(
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
async fn v4_pair_descriptor_refuses_noncanonical_malformed_or_extra_edits() -> TestResult {
    let fixture = Fixture::new().await?;
    let p = prepared(&fixture).await?;
    let original = p.plan.encode(&p.root, "commit")?;
    let encoded: Value = serde_json::from_slice(&original)?;
    assert_eq!(encoded["version"], 4);
    assert_eq!(encoded["kind"], "view-rename-property-pair");
    assert_eq!(
        encoded["property_replacements"],
        json!([
            {"key":OPTIONS_KEY,"expected":OPTIONS_BEFORE,"replacement":OPTIONS_AFTER},
            {"key":KEY,"expected":BEFORE,"replacement":AFTER}
        ])
    );
    assert!(is_record(&original)?);
    assert_eq!(decode(&p.root, &original)?.1.new, expected(&fixture));
    for change in 0..39 {
        let mut mutated = encoded.clone();
        match change {
            0 => mutated["version"] = json!(5),
            1 => mutated["kind"] = json!("unknown"),
            2 => {
                mutated
                    .as_object_mut()
                    .ok_or("object")?
                    .remove("property_replacements");
            }
            3 => mutated["property_replacements"] = json!([]),
            4 => mutated["property_replacements"] = json!([encoded["property_replacements"][0]]),
            5 => {
                mutated["property_replacements"] = json!([
                    encoded["property_replacements"][0],
                    encoded["property_replacements"][1],
                    encoded["property_replacements"][0]
                ])
            }
            6 => {
                mutated["property_replacements"] = json!([
                    encoded["property_replacements"][1],
                    encoded["property_replacements"][0]
                ])
            }
            7 => mutated["property_replacements"][1]["key"] = json!(OPTIONS_KEY),
            8 => mutated["property_replacements"][0]["key"] = json!("absent"),
            9 => mutated["property_replacements"][0]["expected"] = json!("stale"),
            10 => mutated["property_replacements"][1]["expected"] = json!("stale"),
            11 => mutated["property_replacements"][0]["replacement"] = json!("forged"),
            12 => mutated["property_replacements"][1]["replacement"] = json!("forged"),
            13 => mutated["property_replacements"][0]["extra"] = json!(true),
            14 => mutated["extra"] = json!(true),
            15 => mutated["property_replacements"][0]["expected"] = json!(17),
            16 => mutated["property_replacements"][0]["key"] = json!("bad/key"),
            17 => mutated["property_replacements"][0]["replacement"] = json!(""),
            18 => mutated["property_replacements"] = Value::Null,
            19 => {
                mutated["version"] = json!(3);
                mutated["kind"] = json!("view-rename-property");
            }
            20..=25 => {
                let mut image = expected(&fixture);
                match change {
                    20 => {
                        image
                            .properties
                            .insert(OPTIONS_KEY.into(), OPTIONS_BEFORE.into());
                    }
                    21 => {
                        image.properties.insert(KEY.into(), BEFORE.into());
                    }
                    22 => image.version_log.push(
                        iceberg_rust::spec::view_metadata::VersionLogStruct {
                            timestamp_ms: 0,
                            version_id: image.current_version_id,
                        },
                    ),
                    23 => {
                        image.properties.insert("unrelated".into(), "forged".into());
                    }
                    24 => image.schemas.clear(),
                    _ => image.view_uuid = Uuid::new_v4(),
                }
                mutated["new_json"] = json!(serde_json::to_string(&image)?);
            }
            26 => mutated["property_replacement"] = encoded["property_replacements"][0].clone(),
            27 => {
                mutated["property_replacements"][0]
                    .as_object_mut()
                    .ok_or("object")?
                    .remove("replacement");
            }
            28 => mutated["property_replacements"][0]["key"] = json!("é"),
            29 => mutated["property_replacements"][0]["replacement"] = json!("x".repeat(65_537)),
            30 => mutated["property_replacements"][0]["replacement"] = json!(OPTIONS_BEFORE),
            31 => mutated["property_replacements"][0]["replacement"] = json!("bad\0value"),
            32 => mutated["property_replacements"][1] = encoded["property_replacements"][0].clone(),
            33 => mutated["property_replacements"][1] = json!("not-an-object"),
            34 => {
                let mut image = expected(&fixture);
                image.current_version_id += 1;
                mutated["new_json"] = json!(serde_json::to_string(&image)?);
            }
            35 => {
                mutated["version"] = json!(2);
                mutated["kind"] = json!("view-rename");
            }
            36 => {
                mutated["version"] = json!(3);
                mutated["kind"] = json!("view-rename-property");
                mutated["property_replacement"] =
                    json!({"key":KEY,"expected":BEFORE,"replacement":AFTER});
            }
            37 => {
                let mut image = serde_json::to_value(expected(&fixture))?;
                image["unexpected-native-field"] = json!(true);
                mutated["new_json"] = json!(serde_json::to_string(&image)?);
            }
            38 => {
                let mut old = serde_json::to_value(fixture.view.metadata())?;
                old["unexpected-native-before-field"] = json!(true);
                mutated["old_hex"] = json!(hex(&serde_json::to_vec(&old)?));
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
async fn v2_and_v3_descriptor_bytes_and_single_edit_semantics_remain_exact() -> TestResult {
    for single in [false, true] {
        let fixture = Fixture::new().await?;
        let p = if single {
            fixture
                .catalog
                .prepare_view_rename_with_property(
                    fixture.view.identifier(),
                    &fixture.destination,
                    fixture.view.metadata(),
                    source_edit(KEY, BEFORE, AFTER)?,
                )
                .await?
        } else {
            fixture
                .catalog
                .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
                .await?
        };
        let plan = &p.plan;
        let artifacts = plan
            .history
            .iter()
            .map(|r| {
                json!({"path":r.relative,"length":r.length,
            "checksum_fnv1a64":format!("{:016x}",r.checksum)})
            })
            .collect::<Vec<_>>();
        let mut golden = json!({"version":2,"kind":"view-rename","catalog":p.root.key,
            "operation_id":plan.operation,"phase":"commit","source":plan.source,"destination":plan.destination,
            "old_head":plan.old_head,"new_head":plan.new_head,"old_hex":hex(&plan.old_bytes),
            "new_json":std::str::from_utf8(&plan.new_bytes)?,
            "source_owner_hex":plan.old_owner.as_ref().map(|bytes|hex(bytes)),
            "artifacts":artifacts,"artifact_count":plan.history.len(),
            "artifact_receipts_fnv1a64":format!("{:016x}",artifact_receipt_checksum(&plan.history))});
        if single {
            golden["version"] = json!(3);
            golden["kind"] = json!("view-rename-property");
            golden["property_replacement"] =
                json!({"key":KEY,"expected":BEFORE,"replacement":AFTER});
        }
        let bytes = serde_json::to_vec(&golden)?;
        assert_eq!(plan.encode(&p.root, "commit")?, bytes);
        assert_eq!(decode(&p.root, &bytes)?.1.new, plan.new);
        assert!(golden.get("property_replacements").is_none());
        let mut image = fixture.view.metadata().clone();
        image.location.push('2');
        if single {
            image.properties.insert(KEY.into(), AFTER.into());
        }
        p.publish().await?;
        let Tabular::View(view) = fresh(&fixture)
            .await?
            .load_tabular(&fixture.destination)
            .await?
        else {
            return Err("legacy destination".into());
        };
        assert_eq!(view.metadata(), &image);
    }
    Ok(())
}

#[tokio::test]
async fn terminal_v4_then_v3_then_v2_preserves_both_prior_edits() -> TestResult {
    let fixture = Fixture::new().await?;
    prepared(&fixture).await?.publish().await?;
    let third = Identifier::new(fixture.destination.namespace(), "RV3");
    let image = expected(&fixture);
    let p = fixture
        .catalog
        .prepare_view_rename_with_property(
            &fixture.destination,
            &third,
            &image,
            source_edit(KEY, AFTER, "CREATE VIEW RV3 AS SELECT 1 AS X")?,
        )
        .await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&p.plan.encode(&p.root, "commit")?)?["version"],
        3
    );
    p.publish().await?;
    let fourth = Identifier::new(fixture.destination.namespace(), "RV4");
    let p = fixture.catalog.prepare_view_rename(&third, &fourth).await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&p.plan.encode(&p.root, "commit")?)?["version"],
        2
    );
    p.publish().await?;
    let Tabular::View(view) = fresh(&fixture).await?.load_tabular(&fourth).await? else {
        return Err("fourth view".into());
    };
    let mut final_image = image;
    final_image.location.pop();
    final_image.location.push('4');
    final_image
        .properties
        .insert(KEY.into(), "CREATE VIEW RV3 AS SELECT 1 AS X".into());
    assert_eq!(view.metadata(), &final_image);
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
            0 => value["version"] = json!(5),
            1 => value["kind"] = json!("unknown"),
            2 => value["property_replacements"][1]["replacement"] = json!("forged"),
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
async fn view_property_pair_crash_helper() -> TestResult {
    let Some(root) = std::env::var_os("ICEBERG_VIEW_PROPERTY_PAIR_TEST_ROOT") else {
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
        .prepare_view_rename_with_property_pair(
            &source,
            &destination,
            view.metadata(),
            replacement()?,
        )
        .await?;
    let stage = match std::env::var("ICEBERG_VIEW_PROPERTY_PAIR_TEST_STAGE")?.as_str() {
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
                "prepared_pair::view_rename::property_pair_tests::view_property_pair_crash_helper",
                "--nocapture",
            ])
            .env("ICEBERG_VIEW_PROPERTY_PAIR_TEST_ROOT", fixture.root.path())
            .env("ICEBERG_VIEW_PROPERTY_PAIR_TEST_STAGE", stage)
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

#[tokio::test]
async fn pair_abandoned_queued_receipt_keeps_exact_two_edit_commit_healthy() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = prepared(&fixture).await?;
    let (before, waiting) = tokio::sync::oneshot::channel();
    let (begin, begin_waiting) = std::sync::mpsc::channel();
    let (delivered, delivery_waiting) = tokio::sync::oneshot::channel();
    let (finish, finish_waiting) = std::sync::mpsc::channel();
    prepared.hook = Some(Arc::new(TestHook {
        stage: TestStage::BeforeDecision,
        fail: false,
        crash: false,
        pause: StdMutex::new(Some((before, begin_waiting))),
    }));
    prepared.delivery_hook = Some(Arc::new(TestHook {
        stage: TestStage::AfterDelivery,
        fail: false,
        crash: false,
        pause: StdMutex::new(Some((delivered, finish_waiting))),
    }));
    let mut publication = Box::pin(prepared.publish());
    assert!(futures::poll!(publication.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(5), waiting).await??;
    begin.send(())?;
    tokio::time::timeout(Duration::from_secs(5), delivery_waiting).await??;
    drop(publication);
    finish.send(())?;
    tokio::time::timeout(
        Duration::from_secs(5),
        destination(&fixture, &fixture.catalog),
    )
    .await??;
    destination(&fixture, &fresh(&fixture).await?).await?;
    Ok(())
}

#[tokio::test]
async fn mixed_private_v3_v4_plan_refuses_without_publication() -> TestResult {
    let fixture = Fixture::new().await?;
    let original = files(fixture.root.path())?;
    let mut p = prepared(&fixture).await?;
    p.plan.property = Some(PropertyReplacement::from(source_edit(KEY, BEFORE, AFTER)?));
    assert!(p.plan.places(&p.root).is_err());
    assert!(decode(&p.root, &p.plan.encode(&p.root, "commit")?).is_err());
    drop(p);
    assert_eq!(files(fixture.root.path())?, original);
    assert!(fixture.catalog.read_scope().await.is_ok());
    Ok(())
}

#[tokio::test]
async fn v4_raw_before_refuses_unmodeled_fields_but_accepts_json_whitespace() -> TestResult {
    for unknown in [false, true] {
        let fixture = Fixture::new().await?;
        let original = fixture
            .catalog
            .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
            .await?;
        let head = original.root.native(&original.plan.old_head)?;
        let mut value: Value = serde_json::from_slice(&original.plan.old_bytes)?;
        drop(original);
        if unknown {
            value["unexpected-native-before-field"] = json!(true);
        }
        let raw = serde_json::to_vec_pretty(&value)?;
        std::fs::write(&head, &raw)?;
        let before = files(fixture.root.path())?;
        let result = fixture
            .catalog
            .prepare_view_rename_with_property_pair(
                fixture.view.identifier(),
                &fixture.destination,
                fixture.view.metadata(),
                replacement()?,
            )
            .await;
        if unknown {
            assert!(
                result.is_err(),
                "unknown Before fields must not disappear in V4 After"
            );
        } else {
            let prepared = result?;
            assert_eq!(prepared.plan.old_bytes, raw);
            assert_eq!(prepared.source_metadata(), fixture.view.metadata());
            drop(prepared);
        }
        assert_eq!(files(fixture.root.path())?, before);
        assert!(fixture.catalog.read_scope().await.is_ok());
    }
    Ok(())
}

#[path = "view_rename_property_pair_order_tests.rs"]
mod id_order_tests;
