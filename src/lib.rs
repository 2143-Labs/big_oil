//! big_oil — Path of Exile public stash market listener.
//!
//! [`client`] polls `GET /public-stash-tabs`, [`store`] persists every stash
//! and item plus the listing events derived by diffing successive snapshots.

pub mod auth;
pub mod client;
pub mod config;
pub mod model;
pub mod price;
pub mod render;
pub mod store;
