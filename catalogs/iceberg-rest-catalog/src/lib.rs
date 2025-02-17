#![allow(unused_imports)]
#![allow(clippy::too_many_arguments)]

#[macro_use]
extern crate serde_derive;
extern crate serde_repr;
extern crate reqwest;
extern crate serde;
extern crate serde_json;
extern crate url;

#[allow(clippy::all)]
pub mod apis;
pub mod catalog;
pub mod error;
#[allow(clippy::all)]
pub mod models;
