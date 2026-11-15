//! Owning controls for the actual V2 blocking publisher and recovery dispatcher.

use super::*;
use crate::view_rename_tests::Fixture;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn failure(stage: TestStage) -> Arc<TestHook> {
    Arc::new(TestHook {
        stage,
        fail: true,
        crash: false,
        pause: StdMutex::new(None),
    })
}

async fn preparation(fixture: &Fixture) -> TestResult<PreparedViewRename> {
    Ok(fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
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

async fn assert_destination(fixture: &Fixture, catalog: &Arc<FileCatalog>) -> TestResult {
    let scope = catalog.read_scope().await?;
    assert!(!scope.tabular_exists(fixture.view.identifier()).await?);
    assert_eq!(
        scope.list_tabulars(fixture.destination.namespace()).await?,
        vec![fixture.destination.clone()]
    );
    let Tabular::View(view) = scope.load(&fixture.destination).await? else {
        return Err("renamed view unavailable".into());
    };
    let mut expected = fixture.view.metadata().clone();
    expected.location.push('2');
    assert_eq!(view.metadata(), &expected);
    Ok(())
}

#[tokio::test]
async fn dropping_preparation_publishes_nothing_and_releases_writer() -> TestResult {
    let fixture = Fixture::new().await?;
    let prepared = preparation(&fixture).await?;
    let p = prepared.plan.places(&prepared.root)?;
    assert!(missing(&p.stage)?);
    assert!(missing(&p.destination)?);
    assert!(missing(&prepared.root.file(JOURNAL))?);
    assert!(missing(&prepared.root.file(WITNESS))?);
    drop(prepared);
    let scope =
        tokio::time::timeout(Duration::from_secs(3), fixture.catalog.read_scope()).await??;
    let Tabular::View(view) = scope.load(fixture.view.identifier()).await? else {
        return Err("source view".into());
    };
    assert_eq!(view.metadata(), fixture.view.metadata());
    Ok(())
}

#[tokio::test]
async fn predecision_failure_cleans_only_private_stage_and_keeps_exact_source() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = preparation(&fixture).await?;
    let plan = prepared.plan.clone();
    let root = fixture.catalog.pair_root()?;
    let p = plan.places(&root)?;
    let original = plan
        .history
        .iter()
        .map(|receipt| {
            Ok((
                receipt.path(&root)?,
                root.read(&receipt.path(&root)?, MAX_METADATA)?,
            ))
        })
        .collect::<Result<Vec<_>, IcebergError>>()?;
    prepared.hook = Some(failure(TestStage::BeforeDecision));
    let error = prepared.publish().await.expect_err("actual worker failure");
    assert_eq!(error.outcome, PairOutcome::NotCommitted);
    assert!(missing(&p.stage)?);
    assert!(missing(&p.destination)?);
    assert_eq!(
        decode(&root, &root.read(&root.file(JOURNAL), MAX_JOURNAL)?)?.0,
        "aborted"
    );
    for (path, bytes) in original {
        assert_eq!(std::fs::read(path)?, bytes);
    }
    let scope = fixture.catalog.read_scope().await?;
    let Tabular::View(view) = scope.load(fixture.view.identifier()).await? else {
        return Err("source view".into());
    };
    assert_eq!(view.metadata(), fixture.view.metadata());
    drop(scope);
    fresh(&fixture).await?.recover_append_pair().await?;
    assert!(missing(&p.destination)?);
    Ok(())
}

#[tokio::test]
async fn all_postdecision_boundaries_fence_then_recover_exact_image_and_raw_history() -> TestResult
{
    for stage in [
        TestStage::AfterDecision,
        TestStage::AfterFirstHead,
        TestStage::AfterFirstSourceFile,
        TestStage::AfterSourceRetired,
    ] {
        let fixture = Fixture::new().await?;
        let mut prepared = preparation(&fixture).await?;
        let root = fixture.catalog.pair_root()?;
        let plan = prepared.plan.clone();
        let p = plan.places(&root)?;
        let history = plan
            .history
            .iter()
            .filter(|receipt| receipt.relative.rsplit('/').next() != Some("version-hint.text"))
            .map(|receipt| {
                let path = receipt.path(&root)?;
                Ok((leaf(&path)?.to_owned(), root.read(&path, MAX_METADATA)?))
            })
            .collect::<Result<Vec<_>, IcebergError>>()?;
        prepared.hook = Some(failure(stage));
        assert_eq!(
            prepared
                .publish()
                .await
                .expect_err("worker failure")
                .outcome,
            PairOutcome::CommittedNeedsRecovery
        );
        assert!(fixture.catalog.read_scope().await.is_err());
        assert!(fixture
            .catalog
            .tabular_exists(&fixture.destination)
            .await
            .is_err());
        assert!(fixture
            .catalog
            .list_tabulars(fixture.destination.namespace())
            .await
            .is_err());
        assert_eq!(!missing(&p.destination)?, stage != TestStage::AfterDecision);
        let reopened = fresh(&fixture).await?;
        for _ in 0..2 {
            reopened.recover_append_pair().await?;
            assert_destination(&fixture, &reopened).await?;
        }
        for (name, bytes) in history {
            assert_eq!(
                std::fs::read(p.destination.join("metadata").join(name))?,
                bytes
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn pending_recovery_refuses_missing_or_corrupt_history_before_source_retirement() -> TestResult
{
    for stage in [TestStage::AfterDecision, TestStage::AfterFirstHead] {
        for remove in [false, true] {
            let fixture = Fixture::new().await?;
            let mut prepared = preparation(&fixture).await?;
            let root = fixture.catalog.pair_root()?;
            let plan = prepared.plan.clone();
            let p = plan.places(&root)?;
            prepared.hook = Some(failure(stage));
            assert!(prepared.publish().await.is_err());
            let container = if stage == TestStage::AfterDecision {
                &p.stage
            } else {
                &p.destination
            };
            let path = container
                .join("metadata")
                .join(leaf(&root.native(&plan.new_head)?)?);
            let bytes = std::fs::read(&path)?;
            if remove {
                std::fs::remove_file(&path)?;
            } else {
                std::fs::write(&path, b"corrupt view image")?;
            }
            assert!(fixture.catalog.recover_append_pair().await.is_err());
            assert!(fresh(&fixture).await.is_err());
            assert!(fixture.catalog.read_scope().await.is_err());
            assert_eq!(
                root.read(&root.native(&plan.old_head)?, MAX_METADATA)?,
                plan.old_bytes
            );
            assert_eq!(
                decode(&root, &root.read(&root.file(JOURNAL), MAX_JOURNAL)?)?.0,
                "commit"
            );
            if stage == TestStage::AfterDecision {
                assert!(missing(&p.destination)?);
            }
            std::fs::write(path, bytes)?;
            fixture.catalog.recover_append_pair().await?;
            assert_destination(&fixture, &fixture.catalog).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_waiter_keeps_actual_worker_writer_until_drain_and_fences_delivery() -> TestResult
{
    let fixture = Fixture::new().await?;
    let mut prepared = preparation(&fixture).await?;
    let root = fixture.catalog.pair_root()?;
    let p = prepared.plan.places(&root)?;
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
    assert!(caller.await.expect_err("cancelled waiter").is_cancelled());
    let reader_catalog = Arc::clone(&fixture.catalog);
    let mut reader = tokio::spawn(async move { reader_catalog.read_scope().await });
    let writer_catalog = Arc::clone(&fixture.catalog);
    let mut writer = tokio::spawn(async move {
        writer_catalog
            .create_namespace(&Namespace::try_new(&["OTHER".to_owned()])?, None)
            .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut reader)
        .await
        .is_err());
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut writer)
        .await
        .is_err());
    assert!(!missing(&p.source)? && !missing(&p.destination)?);
    resume.send(())?;
    assert!(tokio::time::timeout(Duration::from_secs(5), reader)
        .await??
        .is_err());
    assert!(tokio::time::timeout(Duration::from_secs(5), writer)
        .await??
        .is_err());
    fixture.catalog.recover_append_pair().await?;
    assert_destination(&fixture, &fixture.catalog).await?;
    Ok(())
}

#[tokio::test]
async fn corrupt_historical_image_fences_before_any_remaining_source_cleanup() -> TestResult {
    for stage in [TestStage::AfterDecision, TestStage::AfterFirstHead] {
        let fixture = Fixture::new().await?;
        let mut prepared = preparation(&fixture).await?;
        let root = fixture.catalog.pair_root()?;
        let p = prepared.plan.places(&root)?;
        prepared.hook = Some(failure(stage));
        assert!(prepared.publish().await.is_err());
        let container = if stage == TestStage::AfterDecision {
            &p.stage
        } else {
            &p.destination
        };
        let path = container.join("metadata/v0.metadata.json");
        let bytes = std::fs::read(&path)?;
        let source_names = children(&root, &p.metadata)?;
        std::fs::write(&path, b"corrupt historical image")?;
        assert!(fixture.catalog.recover_append_pair().await.is_err());
        assert_eq!(children(&root, &p.metadata)?, source_names);
        assert!(fixture.catalog.read_scope().await.is_err());
        assert!(fresh(&fixture).await.is_err());
        if stage == TestStage::AfterDecision {
            assert!(missing(&p.destination)?);
        }
        std::fs::write(path, bytes)?;
        fixture.catalog.recover_append_pair().await?;
        assert_destination(&fixture, &fixture.catalog).await?;
    }
    Ok(())
}

#[tokio::test]
async fn abandoning_an_already_queued_view_receipt_can_leave_a_healthy_commit() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut prepared = preparation(&fixture).await?;
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
        assert_destination(&fixture, &fixture.catalog),
    )
    .await??;
    assert_destination(&fixture, &fresh(&fixture).await?).await?;
    Ok(())
}

#[tokio::test]
async fn predecision_cleanup_preserves_unknown_evidence_and_requires_explicit_repair() -> TestResult
{
    let fixture = Fixture::new().await?;
    let mut prepared = preparation(&fixture).await?;
    let root = fixture.catalog.pair_root()?;
    let p = prepared.plan.places(&root)?;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    prepared.hook = Some(Arc::new(TestHook {
        stage: TestStage::BeforeDecision,
        fail: true,
        crash: false,
        pause: StdMutex::new(Some((entered, release))),
    }));
    let caller = tokio::spawn(prepared.publish());
    tokio::time::timeout(Duration::from_secs(5), waiting).await??;
    let evidence = p.stage.join("metadata/unowned-evidence");
    std::fs::write(&evidence, b"preserve me")?;
    resume.send(())?;
    let error = tokio::time::timeout(Duration::from_secs(5), caller)
        .await??
        .expect_err("unsafe cleanup refused");
    assert_eq!(error.outcome, PairOutcome::DecisionUnknown);
    assert!(fixture.catalog.recover_append_pair().await.is_err());
    assert_eq!(std::fs::read(&evidence)?, b"preserve me");
    assert!(fixture.catalog.read_scope().await.is_err());
    assert!(missing(&p.destination)?);
    std::fs::remove_file(evidence)?;
    fixture.catalog.recover_append_pair().await?;
    let scope = fixture.catalog.read_scope().await?;
    assert!(scope.tabular_exists(fixture.view.identifier()).await?);
    assert!(!scope.tabular_exists(&fixture.destination).await?);
    Ok(())
}

#[tokio::test]
async fn required_control_corruption_is_not_a_healthy_not_committed_result() -> TestResult {
    let fixture = Fixture::new().await?;
    let prepared = preparation(&fixture).await?;
    let root = fixture.catalog.pair_root()?;
    std::fs::write(root.file(WITNESS), b"foreign witness")?;
    let error = prepared.publish().await.expect_err("corrupt root control");
    assert_eq!(error.outcome, PairOutcome::NotCommitted);
    assert_eq!(std::fs::read(root.file(WITNESS))?, b"foreign witness");
    assert!(missing(&root.file(JOURNAL))?);
    assert!(fixture.catalog.read_scope().await.is_err());
    assert!(fresh(&fixture).await.is_err());
    assert!(missing(&root.path.join("APP/RV2"))?);
    Ok(())
}

#[tokio::test]
async fn required_control_loss_after_decision_keeps_fence_and_preserves_evidence() -> TestResult {
    for name in [JOURNAL, WITNESS] {
        let fixture = Fixture::new().await?;
        let mut prepared = preparation(&fixture).await?;
        let root = fixture.catalog.pair_root()?;
        prepared.hook = Some(failure(TestStage::AfterFirstHead));
        assert!(prepared.publish().await.is_err());
        let bytes = std::fs::read(root.file(name))?;
        std::fs::remove_file(root.file(name))?;
        assert!(fixture.catalog.recover_append_pair().await.is_err());
        assert!(fresh(&fixture).await.is_err());
        assert!(fixture.catalog.read_scope().await.is_err());
        assert!(missing(&root.file(name))?);
        std::fs::write(root.file(name), bytes)?;
        fixture.catalog.recover_append_pair().await?;
        assert_destination(&fixture, &fixture.catalog).await?;
    }
    Ok(())
}

#[tokio::test]
async fn terminal_rename_does_not_undo_updates_drop_or_old_name_recreation() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut renamed = preparation(&fixture).await?.publish().await?;
    renamed
        .new_transaction(None)
        .update_properties(vec![("later".into(), "kept".into())])
        .commit()
        .await?;
    let expected = renamed.metadata().clone();
    fixture.catalog.recover_append_pair().await?;
    let reopened = fresh(&fixture).await?;
    let Tabular::View(view) = Arc::clone(&reopened)
        .load_tabular(&fixture.destination)
        .await?
    else {
        return Err("view".into());
    };
    assert_eq!(view.metadata(), &expected);
    let original = View::builder()
        .with_name("RV")
        .with_schema(fixture.view.metadata().current_schema(None)?.clone())
        .with_view_version(
            iceberg_rust::spec::view_metadata::Version::builder()
                .with_representation(iceberg_rust::spec::view_metadata::ViewRepresentation::sql(
                    "SELECT 99 AS X",
                    Some("snowflake"),
                ))
                .build()?,
        )
        .build(
            fixture.destination.namespace(),
            Arc::clone(&reopened) as Arc<dyn Catalog>,
        )
        .await?;
    reopened.drop_view(&fixture.destination).await?;
    reopened.recover_append_pair().await?;
    let reopened = fresh(&fixture).await?;
    assert!(!reopened.tabular_exists(&fixture.destination).await?);
    let Tabular::View(view) = Arc::clone(&reopened)
        .load_tabular(original.identifier())
        .await?
    else {
        return Err("recreated view".into());
    };
    assert_eq!(view.metadata(), original.metadata());
    assert_ne!(view.metadata().view_uuid, fixture.view.metadata().view_uuid);
    Ok(())
}

#[tokio::test]
async fn a_second_rename_preserves_opaque_marker_and_full_predecessor_history() -> TestResult {
    let fixture = Fixture::new().await?;
    let first = preparation(&fixture).await?.publish().await?;
    let destination = Identifier::new(fixture.destination.namespace(), "RV3");
    let second = Arc::clone(&fixture.catalog)
        .rename_view(&fixture.destination, &destination)
        .await?;
    let mut expected = first.metadata().clone();
    expected.location = expected.location.trim_end_matches('2').to_owned() + "3";
    assert_eq!(second.metadata(), &expected);
    let fresh = fresh(&fixture).await?;
    assert!(!fresh.tabular_exists(fixture.view.identifier()).await?);
    assert!(!fresh.tabular_exists(&fixture.destination).await?);
    let Tabular::View(view) = fresh.load_tabular(&destination).await? else {
        return Err("twice renamed view".into());
    };
    assert_eq!(view.metadata(), &expected);
    Ok(())
}

#[tokio::test]
async fn borrowed_listing_and_existence_do_not_reenter_a_queued_fair_writer() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let catalog = Arc::clone(&fixture.catalog);
    let source = fixture.view.identifier().clone();
    let destination = fixture.destination.clone();
    let mut writer =
        tokio::spawn(async move { catalog.prepare_view_rename(&source, &destination).await });
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut writer)
        .await
        .is_err());
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            scope.tabular_exists(fixture.view.identifier())
        )
        .await??
    );
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(3),
            scope.list_tabulars(fixture.destination.namespace())
        )
        .await??,
        vec![fixture.view.identifier().clone()]
    );
    drop(scope);
    drop(tokio::time::timeout(Duration::from_secs(3), writer).await???);
    Ok(())
}

#[tokio::test]
async fn wrong_kind_collisions_and_unsupported_backends_have_no_decision_or_source_effects(
) -> TestResult {
    use iceberg_rust::catalog::create::CreateTableBuilder;
    let fixture = Fixture::new().await?;
    let root = fixture.catalog.pair_root()?;
    let table = CreateTableBuilder::default()
        .with_name("BUSY")
        .with_schema(fixture.view.metadata().current_schema(None)?.clone())
        .build(
            fixture.destination.namespace(),
            Arc::clone(&fixture.catalog) as Arc<dyn Catalog>,
        )
        .await?;
    assert!(fixture
        .catalog
        .prepare_view_rename(table.identifier(), &fixture.destination)
        .await
        .is_err());
    assert!(fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), table.identifier())
        .await
        .is_err());
    assert!(fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), fixture.view.identifier())
        .await
        .is_err());
    let other = Namespace::try_new(&["OTHER".to_owned()])?;
    assert!(fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), &Identifier::new(&other, "RV2"))
        .await
        .is_err());
    let memory =
        Arc::new(FileCatalog::new("memory://catalog", ObjectStoreBuilder::memory()).await?);
    assert!(memory
        .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
        .await
        .is_err());
    assert!(missing(&root.file(JOURNAL))? && missing(&root.file(WITNESS))?);
    let Tabular::View(view) = Arc::clone(&fixture.catalog)
        .load_tabular(fixture.view.identifier())
        .await?
    else {
        return Err("source view".into());
    };
    assert_eq!(view.metadata(), fixture.view.metadata());
    let Tabular::Table(unchanged) = Arc::clone(&fixture.catalog)
        .load_tabular(table.identifier())
        .await?
    else {
        return Err("busy table".into());
    };
    assert_eq!(unchanged.metadata(), table.metadata());
    assert!(!fixture.catalog.tabular_exists(&fixture.destination).await?);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn source_and_destination_symlinks_do_not_gain_external_directory_authority() -> TestResult {
    for source_alias in [false, true] {
        let fixture = Fixture::new().await?;
        let root = fixture.catalog.pair_root()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("evidence"), b"untouched")?;
        let source = root.path.join("APP/RV");
        if source_alias {
            std::fs::rename(&source, outside.path().join("real-view"))?;
            std::os::unix::fs::symlink(outside.path().join("real-view"), &source)?;
        } else {
            std::os::unix::fs::symlink(outside.path(), root.path.join("APP/RV2"))?;
        }
        assert!(fixture
            .catalog
            .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(outside.path().join("evidence"))?,
            b"untouched"
        );
        assert!(missing(&root.file(JOURNAL))? && missing(&root.file(WITNESS))?);
    }
    Ok(())
}

// The parent launches this exact test in a separate process. Ordinary discovery
// performs no IO and leaves environment/process state unchanged.
#[tokio::test]
async fn view_rename_crash_helper() -> TestResult {
    let Some(root) = std::env::var_os("ICEBERG_VIEW_RENAME_TEST_ROOT") else {
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
    let mut prepared = catalog
        .prepare_view_rename(
            &Identifier::new(&namespace, "RV"),
            &Identifier::new(&namespace, "RV2"),
        )
        .await?;
    let stage = match std::env::var("ICEBERG_VIEW_RENAME_TEST_STAGE")?.as_str() {
        "ready" => TestStage::BeforeDecision,
        "decision" => TestStage::AfterDecision,
        "destination" => TestStage::AfterFirstHead,
        "retiring" => TestStage::AfterFirstSourceFile,
        "retired" => TestStage::AfterSourceRetired,
        _ => return Err("unknown crash stage".into()),
    };
    prepared.hook = Some(Arc::new(TestHook {
        stage,
        fail: false,
        crash: true,
        pause: StdMutex::new(None),
    }));
    prepared.publish().await?;
    Err("actual blocking worker did not exit".into())
}

#[tokio::test]
async fn process_exit_recovers_ready_or_committed_view_rename_idempotently() -> TestResult {
    for stage in ["ready", "decision", "destination", "retiring", "retired"] {
        let fixture = Fixture::new().await?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "prepared_pair::view_rename::tests::view_rename_crash_helper",
                "--nocapture",
            ])
            .env("ICEBERG_VIEW_RENAME_TEST_ROOT", fixture.root.path())
            .env("ICEBERG_VIEW_RENAME_TEST_STAGE", stage)
            .spawn()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(format!("view rename child timeout: {stage}").into());
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
                    return Err("source view".into());
                };
                assert_eq!(view.metadata(), fixture.view.metadata());
            } else {
                assert_destination(&fixture, &reopened).await?;
            }
        }
    }
    Ok(())
}
