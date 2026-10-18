#![allow(clippy::extra_unused_lifetimes)]
include!(concat!(env!("OUT_DIR"), "/schemas.rs"));
pub mod audit;
mod catalog;
pub mod ids;
mod import;
mod keybase;
mod notifications;
pub mod registry;
pub mod replacements;
pub mod rpc;
pub mod spk;
mod work;

mod database;
