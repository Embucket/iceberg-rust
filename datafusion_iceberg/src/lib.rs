pub mod catalog;
pub mod error;
pub mod materialized_view;
mod parquet_metadata_cache;
pub mod planner;
mod pruning_statistics;
mod statistics;
pub mod table;

pub use crate::table::DataFusionTable;
