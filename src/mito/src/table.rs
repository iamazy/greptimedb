// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(any(test, feature = "test"))]
pub mod test_util;

use std::any::Any;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use common_error::ext::BoxedError;
use common_query::logical_plan::Expr;
use common_query::physical_plan::PhysicalPlanRef;
use common_recordbatch::error::{ExternalSnafu, Result as RecordBatchResult};
use common_recordbatch::{RecordBatch, RecordBatchStream};
use common_telemetry::logging;
use datatypes::schema::Schema;
use futures::task::{Context, Poll};
use futures::Stream;
use object_store::ObjectStore;
use snafu::{ensure, OptionExt, ResultExt};
use store_api::manifest::{self, Manifest, ManifestVersion, MetaActionIterator};
use store_api::storage::{
    AddColumn, AlterOperation, AlterRequest, ChunkReader, FlushContext, ReadContext, Region,
    RegionMeta, RegionNumber, ScanRequest, SchemaRef, Snapshot, WriteContext, WriteRequest,
};
use table::error as table_error;
use table::error::{
    InvalidTableSnafu, RegionSchemaMismatchSnafu, Result as TableResult, TableOperationSnafu,
};
use table::metadata::{
    FilterPushDownType, RawTableInfo, TableInfo, TableInfoRef, TableMeta, TableType, TableVersion,
};
use table::requests::{
    AddColumnRequest, AlterKind, AlterTableRequest, DeleteRequest, InsertRequest,
};
use table::table::scan::SimpleTableScan;
use table::table::{AlterContext, RegionStat, Table};
use tokio::sync::Mutex;

use crate::error;
use crate::error::{
    ProjectedColumnNotFoundSnafu, RegionNotFoundSnafu, Result, ScanTableManifestSnafu,
    UpdateTableManifestSnafu,
};
use crate::manifest::action::*;
use crate::manifest::TableManifest;

#[inline]
fn table_manifest_dir(table_dir: &str) -> String {
    format!("{table_dir}/manifest/")
}

/// [Table] implementation.
pub struct MitoTable<R: Region> {
    manifest: TableManifest,
    // guarded by `self.alter_lock`
    table_info: ArcSwap<TableInfo>,
    regions: HashMap<RegionNumber, R>,
    alter_lock: Mutex<()>,
}

#[async_trait]
impl<R: Region> Table for MitoTable<R> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.table_info().meta.schema.clone()
    }

    async fn insert(&self, request: InsertRequest) -> TableResult<usize> {
        if request.columns_values.is_empty() {
            return Ok(0);
        }

        let region = self
            .regions
            .get(&request.region_number)
            .with_context(|| RegionNotFoundSnafu {
                table: common_catalog::format_full_table_name(
                    &request.catalog_name,
                    &request.schema_name,
                    &request.table_name,
                ),
                region: request.region_number,
            })
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;
        let mut write_request = region.write_request();

        let columns_values = request.columns_values;
        // columns_values is not empty, it's safe to unwrap
        let rows_num = columns_values.values().next().unwrap().len();

        logging::trace!(
            "Insert into table {} region {} with data: {:?}",
            self.table_info().name,
            region.id(),
            columns_values
        );

        write_request
            .put(columns_values)
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;

        let _resp = region
            .write(&WriteContext::default(), write_request)
            .await
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;

        Ok(rows_num)
    }

    fn table_type(&self) -> TableType {
        self.table_info().table_type
    }

    fn table_info(&self) -> TableInfoRef {
        self.table_info.load_full()
    }

    async fn scan(
        &self,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        _limit: Option<usize>,
    ) -> TableResult<PhysicalPlanRef> {
        let read_ctx = ReadContext::default();
        let mut readers = Vec::with_capacity(self.regions.len());
        let mut first_schema: Option<Arc<Schema>> = None;

        let table_info = self.table_info.load();
        // TODO(hl): Currently the API between frontend and datanode is under refactoring in
        // https://github.com/GreptimeTeam/greptimedb/issues/597 . Once it's finished, query plan
        // can carry filtered region info to avoid scanning all regions on datanode.
        for region in self.regions.values() {
            let snapshot = region
                .snapshot(&read_ctx)
                .map_err(BoxedError::new)
                .context(table_error::TableOperationSnafu)?;
            let projection = self
                .transform_projection(region, projection.cloned())
                .map_err(BoxedError::new)
                .context(table_error::TableOperationSnafu)?;
            let filters = filters.into();
            let scan_request = ScanRequest {
                projection,
                filters,
                ..Default::default()
            };
            let reader = snapshot
                .scan(&read_ctx, scan_request)
                .await
                .map_err(BoxedError::new)
                .context(table_error::TableOperationSnafu)?
                .reader;

            let schema = reader.user_schema().clone();
            if let Some(first_schema) = &first_schema {
                // TODO(hl): we assume all regions' schemas are the same, but undergoing table altering
                // may make these schemas inconsistent.
                ensure!(
                    first_schema.version() == schema.version(),
                    RegionSchemaMismatchSnafu {
                        table: common_catalog::format_full_table_name(
                            &table_info.catalog_name,
                            &table_info.schema_name,
                            &table_info.name
                        )
                    }
                );
            } else {
                first_schema = Some(schema);
            }
            readers.push(reader);
        }

        // TODO(hl): we assume table contains at least one region, but with region migration this
        // assumption may become invalid.
        let stream_schema = first_schema.context(InvalidTableSnafu {
            table_id: table_info.ident.table_id,
        })?;

        let schema = stream_schema.clone();
        let stream = Box::pin(async_stream::try_stream! {
            for mut reader in readers {
                while let Some(chunk) = reader.next_chunk().await.map_err(BoxedError::new).context(ExternalSnafu)? {
                    let chunk = reader.project_chunk(chunk);
                    yield RecordBatch::new(stream_schema.clone(), chunk.columns)?
                }
            }
        });

        let stream = Box::pin(ChunkStream { schema, stream });
        Ok(Arc::new(SimpleTableScan::new(stream)))
    }

    fn supports_filters_pushdown(&self, filters: &[&Expr]) -> TableResult<Vec<FilterPushDownType>> {
        Ok(vec![FilterPushDownType::Inexact; filters.len()])
    }

    /// Alter table changes the schemas of the table.
    async fn alter(&self, _context: AlterContext, req: &AlterTableRequest) -> TableResult<()> {
        let _lock = self.alter_lock.lock().await;

        let table_info = self.table_info();
        let table_version = table_info.ident.version;
        let (new_info, alter_op) = self.info_and_op_for_alter(&table_info, &req.alter_kind)?;
        let table_name = &table_info.name;

        if let Some(alter_op) = &alter_op {
            self.alter_regions(table_name, table_version, alter_op)
                .await?;
        }

        // Persist the alteration to the manifest.
        logging::debug!(
            "start updating the manifest of table {} with new table info {:?}",
            table_name,
            new_info
        );
        self.manifest
            .update(TableMetaActionList::with_action(TableMetaAction::Change(
                Box::new(TableChange {
                    table_info: RawTableInfo::from(new_info.clone()),
                }),
            )))
            .await
            .context(UpdateTableManifestSnafu {
                table_name: &self.table_info().name,
            })
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;

        // Update in memory metadata of the table.
        self.set_table_info(new_info);

        Ok(())
    }

    async fn delete(&self, request: DeleteRequest) -> TableResult<usize> {
        if request.key_column_values.is_empty() {
            return Ok(0);
        }
        let mut rows_deleted = 0;
        // TODO(hl): Should be tracked by procedure.
        // TODO(hl): Parse delete request into region->keys instead of delete in each region
        for region in self.regions.values() {
            let mut write_request = region.write_request();
            let key_column_values = request.key_column_values.clone();
            // Safety: key_column_values isn't empty.
            let rows_num = key_column_values.values().next().unwrap().len();

            logging::trace!(
                "Delete from table {} where key_columns are: {:?}",
                self.table_info().name,
                key_column_values
            );

            write_request
                .delete(key_column_values)
                .map_err(BoxedError::new)
                .context(table_error::TableOperationSnafu)?;
            region
                .write(&WriteContext::default(), write_request)
                .await
                .map_err(BoxedError::new)
                .context(table_error::TableOperationSnafu)?;
            rows_deleted += rows_num;
        }
        Ok(rows_deleted)
    }

    async fn flush(
        &self,
        region_number: Option<RegionNumber>,
        wait: Option<bool>,
    ) -> TableResult<()> {
        let flush_ctx = wait.map(|wait| FlushContext { wait }).unwrap_or_default();
        if let Some(region_number) = region_number {
            if let Some(region) = self.regions.get(&region_number) {
                region
                    .flush(&flush_ctx)
                    .await
                    .map_err(BoxedError::new)
                    .context(table_error::TableOperationSnafu)?;
            }
        } else {
            futures::future::try_join_all(
                self.regions.values().map(|region| region.flush(&flush_ctx)),
            )
            .await
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;
        }

        Ok(())
    }

    async fn close(&self) -> TableResult<()> {
        futures::future::try_join_all(self.regions.values().map(|region| region.close()))
            .await
            .map_err(BoxedError::new)
            .context(table_error::TableOperationSnafu)?;

        Ok(())
    }

    fn region_stats(&self) -> TableResult<Vec<RegionStat>> {
        Ok(self
            .regions
            .values()
            .map(|region| RegionStat {
                region_id: region.id(),
                disk_usage_bytes: region.disk_usage_bytes(),
            })
            .collect())
    }
}

struct ChunkStream {
    schema: SchemaRef,
    stream: Pin<Box<dyn Stream<Item = RecordBatchResult<RecordBatch>> + Send>>,
}

impl RecordBatchStream for ChunkStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl Stream for ChunkStream {
    type Item = RecordBatchResult<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(ctx)
    }
}

#[inline]
fn column_qualified_name(table_name: &str, region_name: &str, column_name: &str) -> String {
    format!("{table_name}.{region_name}.{column_name}")
}

impl<R: Region> MitoTable<R> {
    pub(crate) fn new(
        table_info: TableInfo,
        regions: HashMap<RegionNumber, R>,
        manifest: TableManifest,
    ) -> Self {
        Self {
            table_info: ArcSwap::new(Arc::new(table_info)),
            regions,
            manifest,
            alter_lock: Mutex::new(()),
        }
    }

    /// Transform projection which is based on table schema
    /// into projection based on region schema.
    fn transform_projection(
        &self,
        region: &R,
        projection: Option<Vec<usize>>,
    ) -> Result<Option<Vec<usize>>> {
        let table_info = self.table_info();
        let table_schema = &table_info.meta.schema;

        let region_meta = region.in_memory_metadata();
        let region_schema = region_meta.schema();

        if projection.is_none() {
            // In fact, datafusion always calls scan with not-none projection
            // generated by table schema right now, but to prevent future compatibility
            // issue, we process this case here.
            let projection: Result<Vec<_>> = table_schema
                .column_schemas()
                .iter()
                .map(|column_schema| &column_schema.name)
                .map(|name| {
                    region_schema.column_index_by_name(name).with_context(|| {
                        ProjectedColumnNotFoundSnafu {
                            column_qualified_name: column_qualified_name(
                                &table_info.name,
                                region.name(),
                                name,
                            ),
                        }
                    })
                })
                .collect();
            return Some(projection).transpose();
        }

        projection
            .map(|p| {
                p.iter()
                    .map(|idx| table_schema.column_name_by_index(*idx))
                    .map(|name| {
                        region_schema.column_index_by_name(name).with_context(|| {
                            ProjectedColumnNotFoundSnafu {
                                column_qualified_name: column_qualified_name(
                                    &table_info.name,
                                    region.name(),
                                    name,
                                ),
                            }
                        })
                    })
                    .collect()
            })
            .transpose()
    }

    pub async fn create(
        table_name: &str,
        table_dir: &str,
        table_info: TableInfo,
        regions: HashMap<RegionNumber, R>,
        object_store: ObjectStore,
    ) -> Result<MitoTable<R>> {
        let manifest = TableManifest::create(&table_manifest_dir(table_dir), object_store);

        let _timer =
            common_telemetry::timer!(crate::metrics::MITO_CREATE_TABLE_UPDATE_MANIFEST_ELAPSED);
        // TODO(dennis): save manifest version into catalog?
        let _manifest_version = manifest
            .update(TableMetaActionList::with_action(TableMetaAction::Change(
                Box::new(TableChange {
                    table_info: RawTableInfo::from(table_info.clone()),
                }),
            )))
            .await
            .context(UpdateTableManifestSnafu { table_name })?;

        Ok(MitoTable::new(table_info, regions, manifest))
    }

    pub(crate) fn build_manifest(table_dir: &str, object_store: ObjectStore) -> TableManifest {
        TableManifest::create(&table_manifest_dir(table_dir), object_store)
    }

    pub(crate) async fn recover_table_info(
        table_name: &str,
        manifest: &TableManifest,
    ) -> Result<Option<TableInfo>> {
        let (start, end) = Self::manifest_scan_range();
        let mut iter = manifest
            .scan(start, end)
            .await
            .context(ScanTableManifestSnafu { table_name })?;

        let mut last_manifest_version = manifest::MIN_VERSION;
        let mut table_info = None;
        while let Some((manifest_version, action_list)) = iter
            .next_action()
            .await
            .context(ScanTableManifestSnafu { table_name })?
        {
            last_manifest_version = manifest_version;

            for action in action_list.actions {
                match action {
                    TableMetaAction::Change(c) => {
                        table_info = Some(
                            TableInfo::try_from(c.table_info).context(error::ConvertRawSnafu)?,
                        );
                    }
                    TableMetaAction::Protocol(_) => {}
                    TableMetaAction::Remove(_) => unimplemented!("Drop table is unimplemented"),
                }
            }
        }

        if table_info.is_some() {
            // update manifest state after recovering
            let protocol = iter.last_protocol();
            manifest.update_state(last_manifest_version + 1, protocol.clone());
        }

        logging::debug!(
            "Recovered table info {:?} for table: {}",
            table_info,
            table_name
        );

        Ok(table_info)
    }

    #[inline]
    pub fn regions(&self) -> &HashMap<RegionNumber, R> {
        &self.regions
    }

    pub fn set_table_info(&self, table_info: TableInfo) {
        self.table_info.swap(Arc::new(table_info));
    }

    #[inline]
    pub fn manifest(&self) -> &TableManifest {
        &self.manifest
    }

    fn manifest_scan_range() -> (ManifestVersion, ManifestVersion) {
        // TODO(dennis): use manifest version in catalog ?
        (manifest::MIN_VERSION, manifest::MAX_VERSION)
    }

    /// For each region, alter it if its version is not updated.
    pub(crate) async fn alter_regions(
        &self,
        table_name: &str,
        table_version: TableVersion,
        alter_op: &AlterOperation,
    ) -> TableResult<()> {
        let regions = self.regions();
        for region in regions.values() {
            let region_meta = region.in_memory_metadata();
            if u64::from(region_meta.version()) > table_version {
                // Region is already altered.
                continue;
            }

            let alter_req = AlterRequest {
                operation: alter_op.clone(),
                version: region_meta.version(),
            };
            // Alter the region.
            logging::debug!(
                "start altering region {} of table {}, with request {:?}",
                region.name(),
                table_name,
                alter_req,
            );
            region
                .alter(alter_req)
                .await
                .map_err(BoxedError::new)
                .context(TableOperationSnafu)?;
        }

        Ok(())
    }

    pub(crate) fn info_and_op_for_alter(
        &self,
        current_info: &TableInfo,
        alter_kind: &AlterKind,
    ) -> TableResult<(TableInfo, Option<AlterOperation>)> {
        let table_name = &current_info.name;
        let mut new_info = TableInfo::clone(current_info);
        // setup new table info
        match &alter_kind {
            AlterKind::RenameTable { new_table_name } => {
                new_info.name = new_table_name.clone();
            }
            AlterKind::AddColumns { .. } | AlterKind::DropColumns { .. } => {
                let table_meta = &current_info.meta;
                let new_meta = table_meta
                    .builder_with_alter_kind(table_name, alter_kind)?
                    .build()
                    .context(error::BuildTableMetaSnafu { table_name })
                    .map_err(BoxedError::new)
                    .context(table_error::TableOperationSnafu)?;
                new_info.meta = new_meta;
            }
        }
        // Increase version of the table.
        new_info.ident.version = current_info.ident.version + 1;

        // Do create_alter_operation first to bump next_column_id in meta.
        let alter_op = create_alter_operation(table_name, alter_kind, &mut new_info.meta)?;

        Ok((new_info, alter_op))
    }
}

/// Create [`AlterOperation`] according to given `alter_kind`.
pub(crate) fn create_alter_operation(
    table_name: &str,
    alter_kind: &AlterKind,
    table_meta: &mut TableMeta,
) -> TableResult<Option<AlterOperation>> {
    match alter_kind {
        AlterKind::AddColumns { columns } => {
            create_add_columns_operation(table_name, columns, table_meta)
        }
        AlterKind::DropColumns { names } => Ok(Some(AlterOperation::DropColumns {
            names: names.clone(),
        })),
        // No need to build alter operation when reaming tables.
        AlterKind::RenameTable { .. } => Ok(None),
    }
}

fn create_add_columns_operation(
    table_name: &str,
    requests: &[AddColumnRequest],
    table_meta: &mut TableMeta,
) -> TableResult<Option<AlterOperation>> {
    let columns = requests
        .iter()
        .map(|request| {
            let new_column = &request.column_schema;
            let desc = table_meta.alloc_new_column(table_name, new_column)?;

            Ok(AddColumn {
                desc,
                is_key: request.is_key,
            })
        })
        .collect::<TableResult<Vec<_>>>()?;

    Ok(Some(AlterOperation::AddColumns { columns }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_manifest_dir() {
        assert_eq!("demo/manifest/", table_manifest_dir("demo"));
        assert_eq!("numbers/manifest/", table_manifest_dir("numbers"));
    }
}
