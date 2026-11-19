//! Test-first causal controls: only native ID-keyed arrays are unordered.
use super::*;
use iceberg_rust::spec::view_metadata::VersionLogStruct;

async fn fixture_with_multiple_ids_and_ordered_log() -> TestResult<Fixture> {
    let mut fixture = Fixture::new().await?;
    let schema = Schema::builder()
        .with_struct_field(StructField {
            id: 1,
            name: "X".into(),
            required: false,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 2,
            name: "Y".into(),
            required: false,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .build()?;
    fixture
        .view
        .new_transaction(None)
        .update_representations(
            vec![ViewRepresentation::sql(
                "SELECT 1 AS X, 2 AS Y",
                Some("snowflake"),
            )],
            schema.fields().clone(),
        )
        .commit()
        .await?;
    // The fixture owns this complete native typed image. Current view updates
    // do not populate version_log; seed two real, distinct version IDs so a
    // reversed chronological array is an observable mutation rather than clear().
    let capture = fixture
        .catalog
        .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
        .await?;
    let head = capture.root.native(&capture.plan.old_head)?;
    let mut metadata = capture.source_metadata().clone();
    drop(capture);
    assert!(metadata.versions.len() >= 3 && metadata.schemas.len() >= 2);
    let first = *metadata.versions.keys().min().ok_or("first version")?;
    metadata.version_log = vec![
        VersionLogStruct {
            timestamp_ms: 17,
            version_id: first,
        },
        VersionLogStruct {
            timestamp_ms: 29,
            version_id: metadata.current_version_id,
        },
    ];
    std::fs::write(head, serde_json::to_vec(&metadata)?)?;
    let Tabular::View(view) = fresh(&fixture)
        .await?
        .load_tabular(fixture.view.identifier())
        .await?
    else {
        return Err("seeded native view".into());
    };
    fixture.view = view;
    Ok(fixture)
}

fn ordered_ids(value: &mut Value, field: &str, id: &str, reverse: bool) -> TestResult {
    let entries = value
        .get_mut(field)
        .and_then(Value::as_array_mut)
        .ok_or("native ID array")?;
    let mut keyed = Vec::new();
    for entry in std::mem::take(entries) {
        keyed.push((
            entry
                .get(id)
                .and_then(Value::as_i64)
                .ok_or("native integral ID")?,
            entry,
        ));
    }
    keyed.sort_by_key(|(id, _)| *id);
    if reverse {
        keyed.reverse();
    }
    *entries = keyed.into_iter().map(|(_, entry)| entry).collect();
    Ok(())
}

#[tokio::test]
async fn v4_before_accepts_all_forced_id_array_permutations_without_effects() -> TestResult {
    for reverse_versions in [false, true] {
        for reverse_schemas in [false, true] {
            let fixture = fixture_with_multiple_ids_and_ordered_log().await?;
            let capture = fixture
                .catalog
                .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
                .await?;
            let head = capture.root.native(&capture.plan.old_head)?;
            let mut value: Value = serde_json::from_slice(&capture.plan.old_bytes)?;
            drop(capture);
            ordered_ids(&mut value, "versions", "version-id", reverse_versions)?;
            ordered_ids(&mut value, "schemas", "schema-id", reverse_schemas)?;
            let raw = serde_json::to_vec_pretty(&value)?;
            std::fs::write(head, &raw)?;
            let before = files(fixture.root.path())?;
            let prepared = prepared(&fixture).await?;
            assert_eq!(prepared.plan.old_bytes, raw);
            assert_eq!(prepared.source_metadata(), fixture.view.metadata());
            assert_eq!(prepared.plan.new, expected(&fixture));
            drop(prepared);
            assert_eq!(files(fixture.root.path())?, before);
            assert!(fixture.catalog.read_scope().await.is_ok());
        }
    }
    Ok(())
}

#[tokio::test]
async fn v4_before_refuses_duplicate_missing_and_nonintegral_ids_without_effects() -> TestResult {
    for (field, id) in [("versions", "version-id"), ("schemas", "schema-id")] {
        for invalid in 0..3 {
            let fixture = fixture_with_multiple_ids_and_ordered_log().await?;
            let capture = fixture
                .catalog
                .prepare_view_rename(fixture.view.identifier(), &fixture.destination)
                .await?;
            let head = capture.root.native(&capture.plan.old_head)?;
            let mut value: Value = serde_json::from_slice(&capture.plan.old_bytes)?;
            drop(capture);
            let original = value.clone();
            let entries = value[field].as_array_mut().ok_or("array")?;
            match invalid {
                0 => entries.push(entries[0].clone()),
                1 => {
                    entries[0].as_object_mut().ok_or("object")?.remove(id);
                }
                _ => entries[0][id] = json!(1.5),
            }
            assert_ne!(
                value, original,
                "{field}/{invalid} must change the raw Before"
            );
            std::fs::write(head, serde_json::to_vec(&value)?)?;
            let before = files(fixture.root.path())?;
            assert!(prepared(&fixture).await.is_err(), "{field}/{invalid}");
            assert_eq!(files(fixture.root.path())?, before);
            assert!(fixture.catalog.read_scope().await.is_ok());
        }
    }
    Ok(())
}

#[tokio::test]
async fn v4_after_accepts_all_forced_id_array_permutations() -> TestResult {
    let fixture = fixture_with_multiple_ids_and_ordered_log().await?;
    let prepared = prepared(&fixture).await?;
    let original: Value = serde_json::from_slice(&prepared.plan.encode(&prepared.root, "commit")?)?;
    for reverse_versions in [false, true] {
        for reverse_schemas in [false, true] {
            let mut record = original.clone();
            let mut value: Value =
                serde_json::from_str(record["new_json"].as_str().ok_or("new JSON")?)?;
            ordered_ids(&mut value, "versions", "version-id", reverse_versions)?;
            ordered_ids(&mut value, "schemas", "schema-id", reverse_schemas)?;
            record["new_json"] = json!(serde_json::to_string_pretty(&value)?);
            assert_eq!(
                decode(&prepared.root, &serde_json::to_vec(&record)?)?.1.new,
                expected(&fixture)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn v4_after_refuses_ambiguous_ids_unknowns_and_ordered_log_reversal() -> TestResult {
    let fixture = fixture_with_multiple_ids_and_ordered_log().await?;
    let prepared = prepared(&fixture).await?;
    let original: Value = serde_json::from_slice(&prepared.plan.encode(&prepared.root, "commit")?)?;
    let source: Value = serde_json::from_str(original["new_json"].as_str().ok_or("new JSON")?)?;
    for change in 0..10 {
        let mut value = source.clone();
        match change {
            0 | 1 => {
                let field = if change == 0 { "versions" } else { "schemas" };
                let entries = value[field].as_array_mut().ok_or("array")?;
                entries.push(entries[0].clone());
            }
            2 | 3 => {
                let (field, id) = if change == 2 {
                    ("versions", "version-id")
                } else {
                    ("schemas", "schema-id")
                };
                value[field][0].as_object_mut().ok_or("object")?.remove(id);
            }
            4 | 5 => {
                let (field, id) = if change == 4 {
                    ("versions", "version-id")
                } else {
                    ("schemas", "schema-id")
                };
                value[field][0][id] = json!(1.5);
            }
            6 => value["versions"][0]["unmodeled-version-field"] = json!(true),
            7 => value["schemas"][0]["unmodeled-schema-field"] = json!(true),
            8 => value["unmodeled-native-field"] = json!(true),
            _ => {
                let log = value["version-log"].as_array_mut().ok_or("version log")?;
                assert_eq!(log.len(), 2);
                log.reverse();
            }
        }
        assert_ne!(value, source, "change {change} must change the After");
        let mut record = original.clone();
        record["new_json"] = json!(serde_json::to_string(&value)?);
        assert!(
            decode(&prepared.root, &serde_json::to_vec(&record)?).is_err(),
            "change {change}"
        );
    }
    Ok(())
}
