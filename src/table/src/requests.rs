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

//! Table and TableEngine requests

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use common_base::readable_size::ReadableSize;
use datatypes::prelude::VectorRef;
use datatypes::schema::{ColumnSchema, RawSchema};
use serde::{Deserialize, Serialize};
use store_api::storage::RegionNumber;

use crate::engine::TableReference;
use crate::error;
use crate::error::ParseTableOptionSnafu;
use crate::metadata::TableId;

pub const IMMUTABLE_TABLE_META_KEY: &str = "IMMUTABLE_TABLE_META";
pub const IMMUTABLE_TABLE_LOCATION_KEY: &str = "LOCATION";
pub const IMMUTABLE_TABLE_PATTERN_KEY: &str = "PATTERN";
pub const IMMUTABLE_TABLE_FORMAT_KEY: &str = "FORMAT";

#[derive(Debug, Clone)]
pub struct CreateDatabaseRequest {
    pub db_name: String,
    pub create_if_not_exists: bool,
}

/// Create table request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTableRequest {
    pub id: TableId,
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub desc: Option<String>,
    pub schema: RawSchema,
    pub region_numbers: Vec<u32>,
    pub primary_key_indices: Vec<usize>,
    pub create_if_not_exists: bool,
    pub table_options: TableOptions,
    pub engine: String,
}

impl CreateTableRequest {
    pub fn table_ref(&self) -> TableReference {
        TableReference {
            catalog: &self.catalog_name,
            schema: &self.schema_name,
            table: &self.table_name,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TableOptions {
    /// Memtable size of memtable.
    pub write_buffer_size: Option<ReadableSize>,
    /// Time-to-live of table. Expired data will be automatically purged.
    #[serde(with = "humantime_serde")]
    pub ttl: Option<Duration>,
    /// Extra options that may not applicable to all table engines.
    pub extra_options: HashMap<String, String>,
    /// Time window for compaction
    pub compaction_time_window: Option<i64>,
}

pub const WRITE_BUFFER_SIZE_KEY: &str = "write_buffer_size";
pub const TTL_KEY: &str = "ttl";
pub const REGIONS_KEY: &str = "regions";
pub const COMPACTION_TIME_WINDOW_KEY: &str = "compaction_time_window";

impl TryFrom<&HashMap<String, String>> for TableOptions {
    type Error = error::Error;

    fn try_from(value: &HashMap<String, String>) -> Result<Self, Self::Error> {
        let mut options = TableOptions::default();
        if let Some(write_buffer_size) = value.get(WRITE_BUFFER_SIZE_KEY) {
            let size = ReadableSize::from_str(write_buffer_size).map_err(|_| {
                ParseTableOptionSnafu {
                    key: WRITE_BUFFER_SIZE_KEY,
                    value: write_buffer_size,
                }
                .build()
            })?;
            options.write_buffer_size = Some(size)
        }

        if let Some(ttl) = value.get(TTL_KEY) {
            let ttl_value = ttl
                .parse::<humantime::Duration>()
                .map_err(|_| {
                    ParseTableOptionSnafu {
                        key: TTL_KEY,
                        value: ttl,
                    }
                    .build()
                })?
                .into();
            options.ttl = Some(ttl_value);
        }
        if let Some(compaction_time_window) = value.get(COMPACTION_TIME_WINDOW_KEY) {
            options.compaction_time_window = match compaction_time_window.parse::<i64>() {
                Ok(t) => Some(t),
                Err(_) => {
                    return ParseTableOptionSnafu {
                        key: COMPACTION_TIME_WINDOW_KEY,
                        value: compaction_time_window,
                    }
                    .fail()
                }
            };
        }
        options.extra_options = HashMap::from_iter(value.iter().filter_map(|(k, v)| {
            if k != WRITE_BUFFER_SIZE_KEY
                && k != REGIONS_KEY
                && k != TTL_KEY
                && k != COMPACTION_TIME_WINDOW_KEY
            {
                Some((k.clone(), v.clone()))
            } else {
                None
            }
        }));
        Ok(options)
    }
}

impl From<&TableOptions> for HashMap<String, String> {
    fn from(opts: &TableOptions) -> Self {
        let mut res = HashMap::with_capacity(2 + opts.extra_options.len());
        if let Some(write_buffer_size) = opts.write_buffer_size {
            res.insert(
                WRITE_BUFFER_SIZE_KEY.to_string(),
                write_buffer_size.to_string(),
            );
        }
        if let Some(ttl) = opts.ttl {
            let ttl_str = humantime::format_duration(ttl).to_string();
            res.insert(TTL_KEY.to_string(), ttl_str);
        }
        if let Some(compaction_time_window) = opts.compaction_time_window {
            res.insert(
                COMPACTION_TIME_WINDOW_KEY.to_string(),
                compaction_time_window.to_string(),
            );
        }
        res.extend(
            opts.extra_options
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        res
    }
}

/// Open table request
#[derive(Debug, Clone)]
pub struct OpenTableRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub table_id: TableId,
}

/// Alter table request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterTableRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub alter_kind: AlterKind,
}

impl AlterTableRequest {
    pub fn table_ref(&self) -> TableReference {
        TableReference {
            catalog: &self.catalog_name,
            schema: &self.schema_name,
            table: &self.table_name,
        }
    }

    pub fn is_rename_table(&self) -> bool {
        matches!(self.alter_kind, AlterKind::RenameTable { .. })
    }
}

/// Add column request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddColumnRequest {
    pub column_schema: ColumnSchema,
    pub is_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlterKind {
    AddColumns { columns: Vec<AddColumnRequest> },
    DropColumns { names: Vec<String> },
    RenameTable { new_table_name: String },
}

/// Drop table request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropTableRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
}

impl DropTableRequest {
    pub fn table_ref(&self) -> TableReference {
        TableReference {
            catalog: &self.catalog_name,
            schema: &self.schema_name,
            table: &self.table_name,
        }
    }
}

#[derive(Debug)]
pub struct InsertRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub columns_values: HashMap<String, VectorRef>,
    pub region_number: RegionNumber,
}

/// Delete (by primary key) request
#[derive(Debug)]
pub struct DeleteRequest {
    /// Values of each column in this table's primary key and time index.
    ///
    /// The key is the column name, and the value is the column value.
    pub key_column_values: HashMap<String, VectorRef>,
}

#[derive(Debug)]
pub enum CopyDirection {
    Export,
    Import,
}

/// Copy table request
#[derive(Debug)]
pub struct CopyTableRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub location: String,
    pub with: HashMap<String, String>,
    pub connection: HashMap<String, String>,
    pub pattern: Option<String>,
    pub direction: CopyDirection,
}

#[derive(Debug, Clone, Default)]
pub struct FlushTableRequest {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: Option<String>,
    pub region_number: Option<RegionNumber>,
    /// Wait until the flush is done.
    pub wait: Option<bool>,
}

#[macro_export]
macro_rules! meter_insert_request {
    ($req: expr) => {
        meter_macros::write_meter!(
            $req.catalog_name.to_string(),
            $req.schema_name.to_string(),
            $req.table_name.to_string(),
            $req.region_number,
            $req
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_table_options() {
        let options = TableOptions {
            write_buffer_size: None,
            ttl: Some(Duration::from_secs(1000)),
            extra_options: HashMap::new(),
            compaction_time_window: Some(1677652502),
        };
        let serialized = serde_json::to_string(&options).unwrap();
        let deserialized: TableOptions = serde_json::from_str(&serialized).unwrap();
        assert_eq!(options, deserialized);
    }

    #[test]
    fn test_convert_hashmap_between_table_options() {
        let options = TableOptions {
            write_buffer_size: Some(ReadableSize::mb(128)),
            ttl: Some(Duration::from_secs(1000)),
            extra_options: HashMap::new(),
            compaction_time_window: Some(1677652502),
        };
        let serialized_map = HashMap::from(&options);
        let serialized = TableOptions::try_from(&serialized_map).unwrap();
        assert_eq!(options, serialized);

        let options = TableOptions {
            write_buffer_size: None,
            ttl: None,
            extra_options: HashMap::new(),
            compaction_time_window: None,
        };
        let serialized_map = HashMap::from(&options);
        let serialized = TableOptions::try_from(&serialized_map).unwrap();
        assert_eq!(options, serialized);

        let options = TableOptions {
            write_buffer_size: Some(ReadableSize::mb(128)),
            ttl: Some(Duration::from_secs(1000)),
            extra_options: HashMap::from([("a".to_string(), "A".to_string())]),
            compaction_time_window: Some(1677652502),
        };
        let serialized_map = HashMap::from(&options);
        let serialized = TableOptions::try_from(&serialized_map).unwrap();
        assert_eq!(options, serialized);
    }
}
