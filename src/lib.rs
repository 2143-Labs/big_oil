//! big_oil — Path of Exile market index.
//!
//! [`client`] polls the public stash stream, [`own`] reads your own tabs, and
//! [`ninja`] supplies poe.ninja economy values. [`store`] persists all of it.

pub mod auth;
pub mod client;
pub mod config;
pub mod model;
pub mod ninja;
pub mod own;
pub mod price;
pub mod render;
pub mod store;
