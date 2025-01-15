use log::{info, warn};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use deltalake::arrow::array::RecordBatch as ArrowRecordBatch;
use deltalake::datafusion::parquet::file::reader::SerializedFileReader as DeltaLakeParquetReader;
use deltalake::kernel::Action as DeltaLakeAction;
use deltalake::kernel::DataType as DeltaTableKernelType;
use deltalake::kernel::PrimitiveType as DeltaTablePrimitiveType;
use deltalake::kernel::StructField as DeltaTableStructField;
use deltalake::operations::create::CreateBuilder as DeltaTableCreateBuilder;
use deltalake::parquet::file::reader::FileReader as DeltaLakeParquetFileReader;
use deltalake::parquet::record::reader::RowIter as ParquetRowIterator;
use deltalake::parquet::record::Row as ParquetRow;
use deltalake::protocol::SaveMode as DeltaTableSaveMode;
use deltalake::table::PeekCommit as DeltaLakePeekCommit;
use deltalake::writer::{DeltaWriter, RecordBatchWriter as DTRecordBatchWriter};
use deltalake::{open_table_with_storage_options as open_delta_table, DeltaTable, TableProperty};
use s3::bucket::Bucket as S3Bucket;
use tempfile::tempfile;

use super::{parquet_row_into_values_map, LakeBatchWriter, SPECIAL_OUTPUT_FIELDS};
use crate::async_runtime::create_async_tokio_runtime;
use crate::connectors::data_storage::ConnectorMode;
use crate::connectors::scanner::S3Scanner;
use crate::connectors::{
    DataEventType, OffsetKey, OffsetValue, ReadError, ReadResult, Reader, ReaderContext,
    StorageType, WriteError,
};
use crate::engine::Type;
use crate::persistence::frontier::OffsetAntichain;
use crate::persistence::PersistentId;
use crate::python_api::ValueField;

#[allow(clippy::module_name_repetitions)]
pub struct DeltaBatchWriter {
    table: DeltaTable,
    writer: DTRecordBatchWriter,
}

impl DeltaBatchWriter {
    pub fn new(
        path: &str,
        value_fields: &Vec<ValueField>,
        storage_options: HashMap<String, String>,
    ) -> Result<Self, WriteError> {
        let table = Self::open_table(path, value_fields, storage_options)?;
        let writer = DTRecordBatchWriter::for_table(&table)?;
        Ok(Self { table, writer })
    }

    pub fn open_table(
        path: &str,
        schema_fields: &Vec<ValueField>,
        storage_options: HashMap<String, String>,
    ) -> Result<DeltaTable, WriteError> {
        let mut struct_fields = Vec::new();
        for field in schema_fields {
            struct_fields.push(DeltaTableStructField::new(
                field.name.clone(),
                Self::delta_table_primitive_type(&field.type_)?,
                field.type_.can_be_none(),
            ));
        }
        for (field, type_) in SPECIAL_OUTPUT_FIELDS {
            struct_fields.push(DeltaTableStructField::new(
                field,
                Self::delta_table_primitive_type(&type_)?,
                false,
            ));
        }

        let runtime = create_async_tokio_runtime()?;
        let table: DeltaTable = runtime
            .block_on(async {
                let builder = DeltaTableCreateBuilder::new()
                    .with_location(path)
                    .with_save_mode(DeltaTableSaveMode::Append)
                    .with_columns(struct_fields)
                    .with_configuration_property(TableProperty::AppendOnly, Some("true"))
                    .with_storage_options(storage_options.clone());

                builder.await
            })
            .or_else(
                |e| {
                    warn!("Unable to create DeltaTable for output: {e}. Trying to open the existing one by this path.");
                    runtime.block_on(async {
                        open_delta_table(path, storage_options).await
                    })
                }
            )?;

        Ok(table)
    }

    fn delta_table_primitive_type(type_: &Type) -> Result<DeltaTableKernelType, WriteError> {
        Ok(DeltaTableKernelType::Primitive(match type_ {
            Type::Bool => DeltaTablePrimitiveType::Boolean,
            Type::Float => DeltaTablePrimitiveType::Double,
            Type::String | Type::Json => DeltaTablePrimitiveType::String,
            Type::Bytes => DeltaTablePrimitiveType::Binary,
            Type::DateTimeNaive => DeltaTablePrimitiveType::TimestampNtz,
            Type::DateTimeUtc => DeltaTablePrimitiveType::Timestamp,
            Type::Int | Type::Duration => DeltaTablePrimitiveType::Long,
            Type::Optional(wrapped) => return Self::delta_table_primitive_type(wrapped),
            Type::Any
            | Type::Array(_, _)
            | Type::Tuple(_)
            | Type::List(_)
            | Type::PyObjectWrapper
            | Type::Pointer => return Err(WriteError::UnsupportedType(type_.clone())),
        }))
    }
}

impl LakeBatchWriter for DeltaBatchWriter {
    fn write_batch(&mut self, batch: ArrowRecordBatch) -> Result<(), WriteError> {
        create_async_tokio_runtime()?.block_on(async {
            self.writer.write(batch).await?;
            self.writer.flush_and_commit(&mut self.table).await?;
            Ok::<(), WriteError>(())
        })
    }
}

pub enum ObjectDownloader {
    Local,
    S3(Box<S3Bucket>),
}

impl ObjectDownloader {
    fn download_object(&self, path: &str) -> Result<File, ReadError> {
        let obj = match self {
            Self::Local => File::open(path)?,
            Self::S3(bucket) => {
                let contents = S3Scanner::download_object_from_path_and_bucket(path, bucket)?;
                let mut tempfile = tempfile()?;
                tempfile.write_all(contents.bytes())?;
                tempfile.flush()?;
                tempfile.seek(SeekFrom::Start(0))?;
                tempfile
            }
        };
        Ok(obj)
    }
}

#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct DeltaReaderAction {
    action_type: DataEventType,
    path: String,
}

impl DeltaReaderAction {
    pub fn new(action_type: DataEventType, path: String) -> Self {
        Self { action_type, path }
    }
}

#[allow(clippy::module_name_repetitions)]
pub struct DeltaTableReader {
    table: DeltaTable,
    streaming_mode: ConnectorMode,
    column_types: HashMap<String, Type>,
    persistent_id: Option<PersistentId>,
    base_path: String,
    object_downloader: ObjectDownloader,

    reader: Option<ParquetRowIterator<'static>>,
    current_version: i64,
    last_fully_read_version: Option<i64>,
    rows_read_within_version: i64,
    parquet_files_queue: VecDeque<DeltaReaderAction>,
    current_event_type: DataEventType,
}

const DELTA_LAKE_INITIAL_POLL_DURATION: Duration = Duration::from_millis(5);
const DELTA_LAKE_MAX_POLL_DURATION: Duration = Duration::from_millis(100);
const DELTA_LAKE_POLL_BACKOFF: u32 = 2;

impl DeltaTableReader {
    pub fn new(
        path: &str,
        object_downloader: ObjectDownloader,
        storage_options: HashMap<String, String>,
        column_types: HashMap<String, Type>,
        streaming_mode: ConnectorMode,
        persistent_id: Option<PersistentId>,
    ) -> Result<Self, ReadError> {
        let runtime = create_async_tokio_runtime()?;
        let table = runtime.block_on(async { open_delta_table(path, storage_options).await })?;
        let current_version = table.version();
        let parquet_files_queue = Self::get_reader_actions(&table, path)?;

        Ok(Self {
            table,
            column_types,
            streaming_mode,
            persistent_id,
            base_path: path.to_string(),

            current_version,
            object_downloader,
            last_fully_read_version: None,
            reader: None,
            parquet_files_queue,
            rows_read_within_version: 0,
            current_event_type: DataEventType::Insert,
        })
    }

    fn get_reader_actions(
        table: &DeltaTable,
        base_path: &str,
    ) -> Result<VecDeque<DeltaReaderAction>, ReadError> {
        Ok(table
            .snapshot()?
            .file_actions()?
            .into_iter()
            .map(|action| {
                DeltaReaderAction::new(
                    DataEventType::Insert,
                    Self::ensure_absolute_path_with_base(&action.path, base_path),
                )
            })
            .collect())
    }

    fn ensure_absolute_path(&self, path: &str) -> String {
        Self::ensure_absolute_path_with_base(path, &self.base_path)
    }

    fn ensure_absolute_path_with_base(path: &str, base_path: &str) -> String {
        if path.starts_with(base_path) {
            return path.to_string();
        }
        if base_path.ends_with('/') {
            format!("{base_path}{path}")
        } else {
            format!("{base_path}/{path}")
        }
    }

    fn upgrade_table_version(&mut self, is_polling_enabled: bool) -> Result<(), ReadError> {
        let runtime = create_async_tokio_runtime()?;
        runtime.block_on(async {
            self.parquet_files_queue.clear();
            let mut sleep_duration = DELTA_LAKE_INITIAL_POLL_DURATION;
            while self.parquet_files_queue.is_empty() {
                let diff = self
                    .table
                    .log_store()
                    .peek_next_commit(self.current_version)
                    .await?;
                let DeltaLakePeekCommit::New(next_version, txn_actions) = diff else {
                    if !is_polling_enabled {
                        break;
                    }
                    // Fully up to date, no changes yet
                    sleep(sleep_duration);
                    sleep_duration *= DELTA_LAKE_POLL_BACKOFF;
                    if sleep_duration > DELTA_LAKE_MAX_POLL_DURATION {
                        sleep_duration = DELTA_LAKE_MAX_POLL_DURATION;
                    }
                    continue;
                };

                let mut added_blocks = VecDeque::new();
                let mut data_changed = false;
                for action in txn_actions {
                    // Protocol description for Delta Lake actions:
                    // https://github.com/delta-io/delta/blob/master/PROTOCOL.md#actions
                    let action = match action {
                        DeltaLakeAction::Remove(action) => {
                            if action.deletion_vector.is_some() {
                                return Err(ReadError::DeltaDeletionVectorsNotSupported);
                            }
                            data_changed |= action.data_change;
                            let action_path = self.ensure_absolute_path(&action.path);
                            DeltaReaderAction::new(DataEventType::Delete, action_path)
                        }
                        DeltaLakeAction::Add(action) => {
                            data_changed |= action.data_change;
                            let action_path = self.ensure_absolute_path(&action.path);
                            DeltaReaderAction::new(DataEventType::Insert, action_path)
                        }
                        _ => continue,
                    };
                    added_blocks.push_back(action);
                }

                self.last_fully_read_version = Some(self.current_version);
                self.current_version = next_version;
                self.rows_read_within_version = 0;
                if data_changed {
                    self.parquet_files_queue = added_blocks;
                }
            }
            Ok(())
        })
    }

    fn read_next_row_native(&mut self, is_polling_enabled: bool) -> Result<ParquetRow, ReadError> {
        loop {
            if let Some(ref mut reader) = &mut self.reader {
                match reader.next() {
                    Some(Ok(row)) => return Ok(row),
                    Some(Err(parquet_err)) => return Err(ReadError::Parquet(parquet_err)),
                    None => self.reader = None,
                };
            } else {
                if self.parquet_files_queue.is_empty() {
                    self.upgrade_table_version(is_polling_enabled)?;
                    if self.parquet_files_queue.is_empty() {
                        return Err(ReadError::NoObjectsToRead);
                    }
                }
                let next_action = self.parquet_files_queue.pop_front().unwrap();
                let local_object = self.object_downloader.download_object(&next_action.path)?;
                self.current_event_type = next_action.action_type;
                self.reader = Some(DeltaLakeParquetReader::try_from(local_object)?.into_iter());
            }
        }
    }

    fn rows_in_file_count(path: &str) -> Result<i64, ReadError> {
        let reader = DeltaLakeParquetReader::try_from(Path::new(path))?;
        let metadata = reader.metadata();
        let mut n_rows = 0;
        for row_group in metadata.row_groups() {
            n_rows += row_group.num_rows();
        }
        Ok(n_rows)
    }
}

impl Reader for DeltaTableReader {
    fn read(&mut self) -> Result<ReadResult, ReadError> {
        let parquet_row = match self.read_next_row_native(self.streaming_mode.is_polling_enabled())
        {
            Ok(row) => row,
            Err(ReadError::NoObjectsToRead) => return Ok(ReadResult::Finished),
            Err(other) => return Err(other),
        };
        let row_map = parquet_row_into_values_map(&parquet_row, &self.column_types);

        self.rows_read_within_version += 1;
        Ok(ReadResult::Data(
            ReaderContext::from_diff(self.current_event_type, None, row_map),
            (
                OffsetKey::Empty,
                OffsetValue::DeltaTablePosition {
                    version: self.current_version,
                    rows_read_within_version: self.rows_read_within_version,
                    last_fully_read_version: self.last_fully_read_version,
                },
            ),
        ))
    }

    fn seek(&mut self, frontier: &OffsetAntichain) -> Result<(), ReadError> {
        // The offset denotes the last fully processed Delta Table version.
        // Then, the `seek` loads this checkpoint and ensures that no diffs
        // from the current version will be applied.
        let offset_value = frontier.get_offset(&OffsetKey::Empty);
        let Some(OffsetValue::DeltaTablePosition {
            version,
            rows_read_within_version: n_rows_to_rewind,
            last_fully_read_version,
        }) = offset_value
        else {
            if offset_value.is_some() {
                warn!("Incorrect type of offset value in DeltaLake frontier: {offset_value:?}");
            }
            return Ok(());
        };

        self.reader = None;
        let runtime = create_async_tokio_runtime()?;
        if let Some(last_fully_read_version) = last_fully_read_version {
            // The offset is based on the diff between `last_fully_read_version` and `version`
            self.current_version = *last_fully_read_version;
            runtime.block_on(async { self.table.load_version(self.current_version).await })?;
            self.upgrade_table_version(false)?;
        } else {
            // The offset is based on the full set of files present for `version`
            self.current_version = *version;
            runtime.block_on(async { self.table.load_version(self.current_version).await })?;
            self.parquet_files_queue = Self::get_reader_actions(&self.table, &self.base_path)?;
        }

        self.rows_read_within_version = 0;
        while !self.parquet_files_queue.is_empty() {
            let next_block = self.parquet_files_queue.front().unwrap();
            let block_size = Self::rows_in_file_count(&next_block.path)?;
            if self.rows_read_within_version + block_size <= *n_rows_to_rewind {
                info!(
                    "Skipping parquet block with the size of {block_size} entries: {next_block:?}"
                );
                self.rows_read_within_version += block_size;
                self.parquet_files_queue.pop_front();
            } else {
                break;
            }
        }

        let rows_left_to_rewind = *n_rows_to_rewind - self.rows_read_within_version;
        info!("Not quickly-rewindable entries count: {rows_left_to_rewind}");
        for _ in 0..rows_left_to_rewind {
            let _ = self.read_next_row_native(false)?;
        }

        Ok(())
    }

    fn update_persistent_id(&mut self, persistent_id: Option<PersistentId>) {
        self.persistent_id = persistent_id;
    }

    fn persistent_id(&self) -> Option<PersistentId> {
        self.persistent_id
    }

    fn storage_type(&self) -> StorageType {
        StorageType::DeltaLake
    }
}
