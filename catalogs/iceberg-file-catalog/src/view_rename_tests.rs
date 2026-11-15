//! Test-first view rename contract. The initial native implementation remains
//! the Catalog default Unsupported stub; no runtime qualification is claimed.

use super::*;
use iceberg_rust::spec::schema::Schema;
use iceberg_rust::spec::types::{PrimitiveType, StructField, Type};
use iceberg_rust::spec::view_metadata::{Version, ViewRepresentation};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

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
        let marker = uuid::Uuid::new_v4();
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
            .with_property((
                "embucket.sql.view-definition".to_owned(),
                "CREATE VIEW RV AS SELECT 1 AS X".to_owned(),
            ))
            .with_property((
                "embucket.sql.view-options.v1".to_owned(),
                format!("{{\"relation_incarnation\":\"{marker}\"}}"),
            ))
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

#[tokio::test]
async fn view_rename_preserves_uuid_all_versions_properties_and_reopens() -> TestResult {
    let fixture = Fixture::new().await?;
    let original = fixture.view.metadata().clone();
    assert!(original.versions.len() > 1);
    let renamed = Arc::clone(&fixture.catalog)
        .rename_view(fixture.view.identifier(), &fixture.destination)
        .await?;
    let mut expected = original;
    expected.location.push('2');
    assert_eq!(renamed.identifier(), &fixture.destination);
    assert_eq!(renamed.metadata(), &expected);
    assert!(
        !fixture
            .catalog
            .tabular_exists(fixture.view.identifier())
            .await?
    );
    assert!(fixture.catalog.tabular_exists(&fixture.destination).await?);
    let fresh = Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(fixture.root.path()),
        )
        .await?,
    );
    assert!(!fresh.tabular_exists(fixture.view.identifier()).await?);
    let Tabular::View(reopened) = fresh.load_tabular(&fixture.destination).await? else {
        return Err("destination is not a view".into());
    };
    assert_eq!(reopened.metadata(), &expected);
    Ok(())
}

#[tokio::test]
async fn view_rename_waits_for_an_existing_catalog_reader() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let rename =
        Arc::clone(&fixture.catalog).rename_view(fixture.view.identifier(), &fixture.destination);
    tokio::pin!(rename);
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut rename)
        .await
        .is_err());
    let Tabular::View(still_old) = scope.load(fixture.view.identifier()).await? else {
        return Err("old view unavailable to its retained reader".into());
    };
    assert_eq!(still_old.metadata(), fixture.view.metadata());
    drop(scope);
    tokio::time::timeout(Duration::from_secs(3), &mut rename).await??;
    Ok(())
}

#[tokio::test]
async fn retained_old_name_view_cannot_resurrect_after_rename() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut retained = fixture.view.clone();
    let renamed = Arc::clone(&fixture.catalog)
        .rename_view(fixture.view.identifier(), &fixture.destination)
        .await?;
    let changed = retained
        .new_transaction(None)
        .update_properties(vec![("late".to_owned(), "must-not-publish".to_owned())])
        .commit()
        .await;
    assert!(changed.is_err(), "old-name writer must be fenced");
    assert!(
        !fixture
            .catalog
            .tabular_exists(fixture.view.identifier())
            .await?
    );
    let Tabular::View(current) = Arc::clone(&fixture.catalog)
        .load_tabular(&fixture.destination)
        .await?
    else {
        return Err("destination is not a view".into());
    };
    assert_eq!(current.metadata(), renamed.metadata());
    Ok(())
}
