pub mod catalog;
pub mod error;
pub mod materialized_view;
mod parquet_metadata_cache;
pub mod planner;
mod pruning_statistics;
mod statistics;
pub mod table;
mod variant_schema_adapter;

pub use crate::table::DataFusionTable;
