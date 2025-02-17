use super::{
    connector_tags::LOCAL_IMAGE_TAG, draft, jobs, logs, CatalogType, Handler, HandlerStatus, Id,
};
use agent_sql::discovers::Row;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::types::Uuid;

mod specs;

/// JobStatus is the possible outcomes of a handled discover operation.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum JobStatus {
    Queued,
    WrongProtocol,
    TagFailed,
    ImageForbidden,
    PullFailed,
    DiscoverFailed,
    MergeFailed,
    Success {
        #[serde(skip_serializing_if = "Option::is_none")]
        publication_id: Option<Id>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        specs_unchanged: bool,
    },
}

/// A DiscoverHandler is a Handler which performs discovery operations.
pub struct DiscoverHandler {
    connector_network: String,
    bindir: String,
    logs_tx: logs::Tx,
}

impl DiscoverHandler {
    pub fn new(connector_network: &str, bindir: &str, logs_tx: &logs::Tx) -> Self {
        Self {
            connector_network: connector_network.to_string(),
            bindir: bindir.to_string(),
            logs_tx: logs_tx.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Handler for DiscoverHandler {
    async fn handle(&mut self, pg_pool: &sqlx::PgPool) -> anyhow::Result<HandlerStatus> {
        let mut txn = pg_pool.begin().await?;

        let row: Row = match agent_sql::discovers::dequeue(&mut txn).await? {
            None => return Ok(HandlerStatus::Idle),
            Some(row) => row,
        };

        let (id, status) = self.process(row, &mut txn).await?;
        tracing::info!(%id, ?status, "finished");

        agent_sql::discovers::resolve(id, status, &mut txn).await?;
        txn.commit().await?;

        Ok(HandlerStatus::Active)
    }

    fn table_name(&self) -> &'static str {
        "discovers"
    }
}

impl DiscoverHandler {
    #[tracing::instrument(err, skip_all, fields(id=?row.id))]
    async fn process(
        &mut self,
        row: Row,
        txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> anyhow::Result<(Id, JobStatus)> {
        tracing::info!(
            %row.capture_name,
            %row.connector_tag_id,
            %row.connector_tag_job_success,
            %row.created_at,
            %row.draft_id,
            %row.image_name,
            %row.image_tag,
            %row.logs_token,
            %row.protocol,
            %row.updated_at,
            %row.user_id,
            "processing discover",
        );

        // Various pre-flight checks.
        if !row.connector_tag_job_success {
            return Ok((row.id, JobStatus::TagFailed));
        } else if row.protocol != "capture" {
            return Ok((row.id, JobStatus::WrongProtocol));
        } else if !agent_sql::connector_tags::does_connector_exist(&row.image_name, txn).await? {
            return Ok((row.id, JobStatus::ImageForbidden));
        }

        let image_composed = format!("{}{}", row.image_name, row.image_tag);

        if row.image_tag != LOCAL_IMAGE_TAG {
            // Pull the image.
            let pull = jobs::run(
                "pull",
                &self.logs_tx,
                row.logs_token,
                async_process::Command::new("docker")
                    .arg("pull")
                    .arg("--quiet")
                    .arg(&image_composed),
            )
            .await?;

            if !pull.success() {
                return Ok((row.id, JobStatus::PullFailed));
            }
        }

        // Remove draft errors from a previous attempt.
        agent_sql::drafts::delete_errors(row.draft_id, txn)
            .await
            .context("clearing old errors")?;

        let (discover, discover_output) = jobs::run_with_input_output(
            "discover",
            &self.logs_tx,
            row.logs_token,
            row.endpoint_config.0.get().as_bytes(),
            async_process::Command::new(format!("{}/flowctl-go", &self.bindir))
                .arg("api")
                .arg("discover")
                .arg("--config=/dev/stdin")
                .arg("--image")
                .arg(&image_composed)
                .arg("--network")
                .arg(&self.connector_network)
                .arg("--output=json")
                .arg("--log.level=warn")
                .arg("--log.format=color"),
        )
        .await?;

        if !discover.success() {
            let error = draft::Error {
                catalog_name: row.capture_name,
                scope: None,
                detail: String::from_utf8(discover_output)
                    .context("discover error output is not UTF-8")?,
            };
            draft::insert_errors(row.draft_id, vec![error], txn).await?;

            return Ok((row.id, JobStatus::DiscoverFailed));
        }

        let result = Self::build_merged_catalog(
            &row.capture_name,
            &discover_output,
            row.draft_id,
            &row.endpoint_config.0,
            &row.image_name,
            &row.image_tag,
            row.update_only,
            row.user_id,
            txn,
        )
        .await?;

        let catalog = match result {
            Ok(cat) => cat,
            Err(errors) => {
                draft::insert_errors(row.draft_id, errors, txn).await?;
                return Ok((row.id, JobStatus::MergeFailed));
            }
        };

        let drafted_spec_count = catalog.spec_count();
        draft::upsert_specs(row.draft_id, catalog, &Default::default(), txn)
            .await
            .context("inserting draft specs")?;

        let publication_id = if row.auto_publish {
            // Delete any draft specs that are identical to their live specs,
            // but only if we're going to create a publication automatically.
            // In the interactive case, these specs are still currently needed
            // by the UI. In the future, we may be able to unconditionally prune
            // these specs after doing some additional UI work.
            let pruned_specs =
                agent_sql::drafts::prune_unchanged_draft_specs(row.draft_id, txn).await?;

            tracing::info!(
                drafted_spec_count,
                n_pruned = pruned_specs.len(),
                "pruned draft"
            );
            tracing::debug!(?pruned_specs, "pruned unchanged draft specs");

            if pruned_specs.len() == drafted_spec_count {
                return Ok((
                    row.id,
                    JobStatus::Success {
                        publication_id: None,
                        specs_unchanged: true,
                    },
                ));
            }

            let detail = format!(
                "system created publication in response to discover: {}",
                row.id
            );
            let id = agent_sql::publications::create(
                txn,
                row.user_id,
                row.draft_id,
                row.auto_evolve,
                detail,
            )
            .await?;
            Some(id)
        } else {
            None
        };

        Ok((
            row.id,
            JobStatus::Success {
                publication_id,
                specs_unchanged: false,
            },
        ))
    }

    async fn build_merged_catalog(
        capture_name: &str,
        discover_output: &[u8],
        draft_id: Id,
        endpoint_config: &serde_json::value::RawValue,
        image_name: &str,
        image_tag: &str,
        update_only: bool,
        user_id: Uuid,
        txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> anyhow::Result<Result<models::Catalog, Vec<draft::Error>>> {
        let (endpoint, discovered_bindings) =
            specs::parse_response(endpoint_config, image_name, image_tag, discover_output)
                .context("converting discovery response into specs")?;

        // Catalog we'll build up with the merged capture and collections.
        let mut catalog = models::Catalog::default();

        // Resolve the current capture, if one exists.
        let resolved = agent_sql::discovers::resolve_merge_target_specs(
            &[capture_name],
            CatalogType::Capture,
            draft_id,
            user_id,
            txn,
        )
        .await
        .context("resolving the current capture")?;

        let errors = draft::extend_catalog(
            &mut catalog,
            resolved
                .iter()
                .map(|r| (CatalogType::Capture, capture_name, r.spec.0.as_ref())),
        );
        if !errors.is_empty() {
            return Ok(Err(errors));
        }

        // TODO: As of 2023-11, resource_path_pointers are allowed to be empty.
        // `merge_capture` will just log a warning if they are. But we plan to
        // soon require that they are never empty.
        let resource_path_pointers =
            agent_sql::connector_tags::fetch_resource_path_pointers(image_name, image_tag, txn)
                .await?;
        if resource_path_pointers.is_empty() {
            tracing::warn!(%image_name, %image_tag, %capture_name, "merging bindings using legacy behavior because resource_path_pointers are missing");
        }

        // Deeply merge the capture and its bindings.
        let capture_name = models::Capture::new(capture_name);
        let merge_result = specs::merge_capture(
            &capture_name,
            endpoint,
            discovered_bindings,
            catalog.captures.remove(&capture_name),
            update_only,
            &resource_path_pointers,
        );
        let (merged_capture, discovered_bindings) = match merge_result {
            Ok(ok) => ok,
            Err(invalid_resource) => {
                return Ok(Err(vec![draft::Error {
                    catalog_name: capture_name.to_string(),
                    scope: None,
                    detail: invalid_resource.to_string(),
                }]))
            }
        };
        let targets = merged_capture
            .bindings
            .iter()
            .map(|models::CaptureBinding { target, .. }| target.clone())
            .collect::<Vec<_>>();

        catalog.captures.insert(capture_name, merged_capture); // Replace merged capture.

        // Now resolve all targeted collections, if they exist.
        let resolved = agent_sql::discovers::resolve_merge_target_specs(
            &targets.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            CatalogType::Collection,
            draft_id,
            user_id,
            txn,
        )
        .await
        .context("resolving the current capture")?;

        let errors = draft::extend_catalog(
            &mut catalog,
            resolved.iter().map(|r| {
                (
                    CatalogType::Collection,
                    r.catalog_name.as_str(),
                    r.spec.0.as_ref(),
                )
            }),
        );
        if !errors.is_empty() {
            return Ok(Err(errors));
        }

        // Now deeply merge all captured collections.
        // Post-condition: `catalog` reflects the final outcome of our operation.
        catalog.collections =
            specs::merge_collections(discovered_bindings, catalog.collections, targets);

        Ok(Ok(catalog))
    }
}

#[cfg(test)]
mod test {

    use super::{Id, Uuid};
    use serde_json::json;
    use sqlx::Connection;
    use std::str::FromStr;

    const FIXED_DATABASE_URL: &str = "postgresql://postgres:postgres@localhost:5432/postgres";

    #[tokio::test]
    async fn test_catalog_merge_ok() {
        let mut conn = sqlx::postgres::PgConnection::connect(&FIXED_DATABASE_URL)
            .await
            .unwrap();
        let mut txn = conn.begin().await.unwrap();

        sqlx::query(
            r#"
            with
            p1 as (
                insert into user_grants(user_id, object_role, capability) values
                ('11111111-1111-1111-1111-111111111111', 'aliceCo/', 'admin')
            ),
            p2 as (
                insert into drafts (id, user_id) values
                ('dddddddddddddddd', '11111111-1111-1111-1111-111111111111')
            ),
            p3 as (
                insert into live_specs (catalog_name, spec_type, spec) values
                -- Existing collection which is deeply merged.
                ('aliceCo/existing-collection', 'collection', '{
                    "key": ["/old/key"],
                    "writeSchema": false,
                    "readSchema": {"const": "read!"}
                }')
            ),
            p4 as (
                insert into draft_specs (draft_id, catalog_name, spec_type, spec) values
                -- Capture which is deeply merged (modified resource config and `interval` are preserved).
                ('dddddddddddddddd', 'aliceCo/dir/source-thingy', 'capture', '{
                    "bindings": [
                        { "resource": { "table": "foo", "modified": 1 }, "target": "aliceCo/existing-collection" }
                    ],
                    "endpoint": { "connector": { "config": { "fetched": 1 }, "image": "old/image" } },
                    "interval": "10m"
                }'),
                -- Drafted collection which isn't (yet) linked to the capture, but collides
                -- with a binding being added. Expect `projections` are preserved in the merge.
                ('dddddddddddddddd', 'aliceCo/dir/quz', 'collection', '{
                    "key": ["/old/key"],
                    "schema": false,
                    "projections": {"a-field": "/some/ptr"}
                }')
            )
            select 1;
            "#,
        )
        .execute(&mut txn)
        .await
        .unwrap();

        let discover_output = json!({
            "bindings": [
                {"documentSchema": {"const": "write!"}, "key": ["/foo"], "recommendedName": "foo", "resourceConfig": {"table": "foo"}},
                {"documentSchema": {"const": "bar"}, "key": ["/bar"], "recommendedName": "bar", "resourceConfig": {"table": "bar"}},
                {"documentSchema": {"const": "quz"}, "key": ["/quz"], "recommendedName": "quz", "resourceConfig": {"table": "quz"}},
            ],
        }).to_string();

        let endpoint_config =
            serde_json::value::to_raw_value(&json!({"some": "endpoint-config"})).unwrap();

        let result = super::DiscoverHandler::build_merged_catalog(
            "aliceCo/dir/source-thingy",
            discover_output.as_bytes(),
            Id::from_hex("dddddddddddddddd").unwrap(),
            &endpoint_config,
            "ghcr.io/estuary/source-thingy",
            ":v1",
            false,
            Uuid::from_str("11111111-1111-1111-1111-111111111111").unwrap(),
            &mut txn,
        )
        .await;

        let catalog = result.unwrap().unwrap();
        insta::assert_json_snapshot!(json!(catalog));
    }

    #[tokio::test]
    async fn test_catalog_merge_bad_spec() {
        let mut conn = sqlx::postgres::PgConnection::connect(&FIXED_DATABASE_URL)
            .await
            .unwrap();
        let mut txn = conn.begin().await.unwrap();

        sqlx::query(
            r#"
            with
            p1 as (
                insert into drafts (id, user_id) values
                ('dddddddddddddddd', '11111111-1111-1111-1111-111111111111')
            ),
            p2 as (
                insert into draft_specs (draft_id, catalog_name, spec_type, spec) values
                ('dddddddddddddddd', 'aliceCo/bad', 'collection', '{"key": "invalid"}')
            )
            select 1;
            "#,
        )
        .execute(&mut txn)
        .await
        .unwrap();

        let discover_output = json!({
            "bindings": [
                {"documentSchema": {"const": 42}, "key": ["/key"], "recommendedName": "bad", "resourceConfig": {"table": "bad"}},
            ],
        }).to_string();

        let result = super::DiscoverHandler::build_merged_catalog(
            "aliceCo/source-thingy",
            discover_output.as_bytes(),
            Id::from_hex("dddddddddddddddd").unwrap(),
            &serde_json::value::to_raw_value(&json!({"some": "endpoint-config"})).unwrap(),
            "ghcr.io/estuary/source-thingy",
            ":v1",
            false,
            Uuid::from_str("11111111-1111-1111-1111-111111111111").unwrap(),
            &mut txn,
        )
        .await;

        let errors = result.unwrap().unwrap_err();
        insta::assert_debug_snapshot!(errors, @r###"
        [
            Error {
                catalog_name: "aliceCo/bad",
                scope: None,
                detail: "parsing collection aliceCo/bad: invalid type: string \"invalid\", expected a sequence at line 1 column 17",
            },
        ]
        "###);
    }
}
