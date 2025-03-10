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

use std::sync::Arc;

use arrow_schema::SchemaRef as ArrowSchemaRef;
use common_query::physical_plan::TaskContext;
use common_recordbatch::RecordBatch;
use datafusion::datasource::streaming::PartitionStream as DfPartitionStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter as DfRecordBatchStreamAdapter;
use datafusion::physical_plan::SendableRecordBatchStream as DfSendableRecordBatchStream;
use datatypes::prelude::{ConcreteDataType, DataType};
use datatypes::scalars::ScalarVectorBuilder;
use datatypes::schema::{ColumnSchema, Schema, SchemaRef};
use datatypes::vectors::{StringVectorBuilder, VectorRef};
use snafu::ResultExt;

use crate::error::{CreateRecordBatchSnafu, Result};
use crate::CatalogProviderRef;

pub(super) struct InformationSchemaColumns {
    schema: SchemaRef,
    catalog_name: String,
    catalog_provider: CatalogProviderRef,
}

const TABLE_CATALOG: &str = "table_catalog";
const TABLE_SCHEMA: &str = "table_schema";
const TABLE_NAME: &str = "table_name";
const COLUMN_NAME: &str = "column_name";
const DATA_TYPE: &str = "data_type";

impl InformationSchemaColumns {
    pub(super) fn new(catalog_name: String, catalog_provider: CatalogProviderRef) -> Self {
        let schema = Arc::new(Schema::new(vec![
            ColumnSchema::new(TABLE_CATALOG, ConcreteDataType::string_datatype(), false),
            ColumnSchema::new(TABLE_SCHEMA, ConcreteDataType::string_datatype(), false),
            ColumnSchema::new(TABLE_NAME, ConcreteDataType::string_datatype(), false),
            ColumnSchema::new(COLUMN_NAME, ConcreteDataType::string_datatype(), false),
            ColumnSchema::new(DATA_TYPE, ConcreteDataType::string_datatype(), false),
        ]));
        Self {
            schema,
            catalog_name,
            catalog_provider,
        }
    }

    fn builder(&self) -> InformationSchemaColumnsBuilder {
        InformationSchemaColumnsBuilder::new(
            self.schema.clone(),
            self.catalog_name.clone(),
            self.catalog_provider.clone(),
        )
    }
}

struct InformationSchemaColumnsBuilder {
    schema: SchemaRef,
    catalog_name: String,
    catalog_provider: CatalogProviderRef,

    catalog_names: StringVectorBuilder,
    schema_names: StringVectorBuilder,
    table_names: StringVectorBuilder,
    column_names: StringVectorBuilder,
    data_types: StringVectorBuilder,
}

impl InformationSchemaColumnsBuilder {
    fn new(schema: SchemaRef, catalog_name: String, catalog_provider: CatalogProviderRef) -> Self {
        Self {
            schema,
            catalog_name,
            catalog_provider,
            catalog_names: StringVectorBuilder::with_capacity(42),
            schema_names: StringVectorBuilder::with_capacity(42),
            table_names: StringVectorBuilder::with_capacity(42),
            column_names: StringVectorBuilder::with_capacity(42),
            data_types: StringVectorBuilder::with_capacity(42),
        }
    }

    /// Construct the `information_schema.tables` virtual table
    async fn make_tables(&mut self) -> Result<RecordBatch> {
        let catalog_name = self.catalog_name.clone();

        for schema_name in self.catalog_provider.schema_names().await? {
            let Some(schema) = self.catalog_provider.schema(&schema_name).await? else { continue };
            for table_name in schema.table_names().await? {
                let Some(table) = schema.table(&table_name).await? else { continue };
                let schema = table.schema();
                for column in schema.column_schemas() {
                    self.add_column(
                        &catalog_name,
                        &schema_name,
                        &table_name,
                        &column.name,
                        column.data_type.name(),
                    );
                }
            }
        }

        self.finish()
    }

    fn add_column(
        &mut self,
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        column_name: &str,
        data_type: &str,
    ) {
        self.catalog_names.push(Some(catalog_name));
        self.schema_names.push(Some(schema_name));
        self.table_names.push(Some(table_name));
        self.column_names.push(Some(column_name));
        self.data_types.push(Some(data_type));
    }

    fn finish(&mut self) -> Result<RecordBatch> {
        let columns: Vec<VectorRef> = vec![
            Arc::new(self.catalog_names.finish()),
            Arc::new(self.schema_names.finish()),
            Arc::new(self.table_names.finish()),
            Arc::new(self.column_names.finish()),
            Arc::new(self.data_types.finish()),
        ];
        RecordBatch::new(self.schema.clone(), columns).context(CreateRecordBatchSnafu)
    }
}

impl DfPartitionStream for InformationSchemaColumns {
    fn schema(&self) -> &ArrowSchemaRef {
        self.schema.arrow_schema()
    }

    fn execute(&self, _: Arc<TaskContext>) -> DfSendableRecordBatchStream {
        let schema = self.schema().clone();
        let mut builder = self.builder();
        Box::pin(DfRecordBatchStreamAdapter::new(
            schema,
            futures::stream::once(async move {
                builder
                    .make_tables()
                    .await
                    .map(|x| x.into_df_record_batch())
                    .map_err(Into::into)
            }),
        ))
    }
}
