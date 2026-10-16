#![allow(clippy::extra_unused_lifetimes)]
include!(concat!(env!("OUT_DIR"), "/schemas.rs"));
pub mod ids;
mod import;
mod keybase;
pub mod registry;
pub mod rpc;
pub mod spk;
mod work;
