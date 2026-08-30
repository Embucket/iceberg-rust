pub mod catalog;
pub mod error;
pub mod materialized_view;
mod parquet_metadata_cache;
pub mod planner;
pub mod pruning_statistics;
pub mod statistics;
pub mod table;
mod variant_schema_adapter;

pub use crate::table::DataFusionTable;
