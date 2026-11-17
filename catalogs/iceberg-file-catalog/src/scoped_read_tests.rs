//! Real-file source-before-implementation optional scoped reads. No SQL acceptance.
use super::*;
use crate::view_rename_tests::Fixture;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn optional_scoped_read_preserves_full_view_and_requires_a_live_namespace() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let Some(Tabular::View(view)) = scope.load_if_present(fixture.view.identifier()).await? else {
        return Err("actual full native view".into());
    };
    assert_eq!(view.identifier(), fixture.view.identifier());
    assert_eq!(view.metadata(), fixture.view.metadata());
    assert!(view.metadata().versions.len() > 1);
    assert!(scope.load_if_present(&fixture.destination).await?.is_none());
    let missing_parent = Identifier::new(&["MISSING".to_owned()], "RV");
    assert!(
        scope.load_if_present(&missing_parent).await.is_err(),
        "missing parent is not a coherent absent endpoint"
    );
    Ok(())
}

#[tokio::test]
async fn optional_scoped_read_keeps_real_table_kind_instead_of_inventing_absence() -> TestResult {
    use iceberg_rust::catalog::create::CreateTableBuilder;
    let fixture = Fixture::new().await?;
    let schema = fixture.view.metadata().current_schema(None)?.clone();
    let namespace = Namespace::try_new(fixture.view.identifier().namespace())?;
    let table = CreateTableBuilder::default()
        .with_name("T")
        .with_schema(schema)
        .build(&namespace, Arc::clone(&fixture.catalog) as Arc<dyn Catalog>)
        .await?;
    let scope = fixture.catalog.read_scope().await?;
    let Some(Tabular::Table(actual)) = scope.load_if_present(table.identifier()).await? else {
        return Err("native table kind must remain explicit".into());
    };
    assert_eq!(actual.metadata(), table.metadata());
    Ok(())
}

#[tokio::test]
async fn existing_empty_unrecognized_and_corrupt_endpoints_never_become_none() -> TestResult {
    for fault in [
        "empty_endpoint",
        "unrecognized_metadata",
        "invalid_json",
        "missing_current_heads",
        "endpoint_file",
        "metadata_file",
    ] {
        let fixture = Fixture::new().await?;
        let root = fixture.catalog.pair_root()?;
        let endpoint = root.native(&fixture.view.metadata().location)?;
        let metadata = endpoint.join("metadata");
        let head = root.native(
            &fixture
                .catalog
                .metadata_location(fixture.view.identifier())
                .await?,
        )?;
        match fault {
            "empty_endpoint" => {
                std::fs::remove_dir_all(&metadata)?;
            }
            "unrecognized_metadata" => {
                std::fs::remove_dir_all(&metadata)?;
                std::fs::create_dir(&metadata)?;
                std::fs::write(metadata.join("foreign.json"), b"{}")?;
            }
            "invalid_json" => {
                std::fs::write(&head, b"not-json")?;
            }
            "missing_current_heads" => {
                for child in std::fs::read_dir(&metadata)? {
                    let child = child?;
                    if child
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(".metadata.json"))
                    {
                        std::fs::remove_file(child.path())?;
                    }
                }
            }
            "endpoint_file" => {
                std::fs::remove_dir_all(&endpoint)?;
                std::fs::write(&endpoint, b"file")?;
            }
            _ => {
                std::fs::remove_dir_all(&metadata)?;
                std::fs::write(&metadata, b"file")?;
            }
        }
        let scope = fixture.catalog.read_scope().await?;
        if fault == "invalid_json" {
            assert!(
                scope.tabular_exists(fixture.view.identifier()).await?,
                "legacy exists does not parse metadata"
            );
        }
        if fault == "unrecognized_metadata" {
            assert!(
                !scope.tabular_exists(fixture.view.identifier()).await?,
                "legacy lookup is not physical absence proof"
            );
        }
        assert!(
            scope
                .load_if_present(fixture.view.identifier())
                .await
                .is_err(),
            "{fault}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn optional_scoped_read_rejects_live_and_dangling_symlink_endpoints_and_metadata(
) -> TestResult {
    for fault in [
        "endpoint",
        "dangling_endpoint",
        "metadata",
        "head",
        "namespace",
    ] {
        let fixture = Fixture::new().await?;
        let root = fixture.catalog.pair_root()?;
        let endpoint = root.native(&fixture.view.metadata().location)?;
        let metadata = endpoint.join("metadata");
        let head = root.native(
            &fixture
                .catalog
                .metadata_location(fixture.view.identifier())
                .await?,
        )?;
        let outside = fixture.root.path().join("outside");
        std::fs::create_dir(&outside)?;
        let path = match fault {
            "endpoint" | "dangling_endpoint" => endpoint.clone(),
            "metadata" => metadata,
            "head" => head,
            _ => endpoint.parent().ok_or("namespace")?.to_owned(),
        };
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        let target = if fault == "dangling_endpoint" {
            outside.join("missing")
        } else {
            outside
        };
        std::os::unix::fs::symlink(target, &path)?;
        let scope = fixture.catalog.read_scope().await?;
        assert!(
            scope
                .load_if_present(fixture.view.identifier())
                .await
                .is_err(),
            "{fault}"
        );
        if fault == "namespace" {
            assert!(scope.load_if_present(&fixture.destination).await.is_err());
        }
    }
    Ok(())
}

#[tokio::test]
async fn optional_scoped_read_refuses_unsafe_identifier_shapes_before_missing_lookup() -> TestResult
{
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    for id in [
        Identifier::new(&["APP".into(), "NESTED".into()], "RV"),
        Identifier::new(&["APP".into()], "../RV"),
        Identifier::new(&["APP".into()], "encoded%2fRV"),
        Identifier::new(&["APP".into()], "."),
        Identifier::new(&["APP".into()], "RV/child"),
    ] {
        assert!(scope.load_if_present(&id).await.is_err(), "{id}");
    }
    Ok(())
}

#[tokio::test]
async fn borrowed_optional_exists_and_listing_do_not_reenter_a_queued_writer() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let source = fixture.view.identifier().clone();
    let destination = fixture.destination.clone();
    let writer = Arc::clone(&fixture.catalog).prepare_view_rename(&source, &destination);
    tokio::pin!(writer);
    assert!(
        matches!(futures::poll!(writer.as_mut()), std::task::Poll::Pending),
        "actual writer's first await queues its native write lock"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), writer.as_mut())
            .await
            .is_err()
    );
    let Some(Tabular::View(view)) = tokio::time::timeout(
        Duration::from_secs(3),
        scope.load_if_present(fixture.view.identifier()),
    )
    .await??
    else {
        return Err("scoped view while writer queued".into());
    };
    assert_eq!(view.metadata(), fixture.view.metadata());
    assert!(tokio::time::timeout(
        Duration::from_secs(3),
        scope.load_if_present(&fixture.destination)
    )
    .await??
    .is_none());
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
            scope.list_tabulars(fixture.view.identifier().namespace())
        )
        .await??,
        vec![fixture.view.identifier().clone()]
    );
    drop(scope);
    drop(tokio::time::timeout(Duration::from_secs(3), writer).await??);
    assert!(
        fixture
            .catalog
            .tabular_exists(fixture.view.identifier())
            .await?
    );
    assert!(!fixture.catalog.tabular_exists(&fixture.destination).await?);
    Ok(())
}

#[tokio::test]
async fn scoped_optional_reads_do_not_write_catalog_files() -> TestResult {
    fn image(
        path: &std::path::Path,
    ) -> std::io::Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
        let mut result = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if entry.file_type()?.is_dir() {
                result.extend(image(&p)?);
            } else {
                result.insert(p, std::fs::read(entry.path())?);
            }
        }
        Ok(result)
    }
    let fixture = Fixture::new().await?;
    let root = fixture.catalog.pair_root()?;
    let before = image(&root.path)?;
    let scope = fixture.catalog.read_scope().await?;
    assert!(scope
        .load_if_present(fixture.view.identifier())
        .await?
        .is_some());
    assert!(scope.load_if_present(&fixture.destination).await?.is_none());
    assert_eq!(image(&root.path)?, before);
    Ok(())
}
