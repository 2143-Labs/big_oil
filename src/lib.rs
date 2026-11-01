//! big_oil — Path of Exile market index.
//!
//! [`client`] polls the public stash stream, [`own`] reads your own tabs,
//! [`ninja`] supplies poe.ninja economy values, [`dump`] watches underpricing
//! and price decay, and [`extract`] triages a freshly identified dump tab.

pub mod auth;
pub mod client;
pub mod config;
pub mod dump;
pub mod extract;
pub mod model;
pub mod ninja;
pub mod overlay;
pub mod own;
pub mod price;
pub mod rates;
pub mod render;
pub mod store;
