//! big_oil — Path of Exile market index.
//!
//! [`client`] polls the public stash stream, [`own`] reads your own tabs, and
//! [`ninja`] supplies poe.ninja economy values. [`dump`] turns that into the
//! two things that actually cost money: underpriced items and stale prices.

pub mod auth;
pub mod client;
pub mod config;
pub mod dump;
pub mod model;
pub mod ninja;
pub mod own;
pub mod price;
pub mod rates;
pub mod render;
pub mod store;
