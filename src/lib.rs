//! big_oil — Path of Exile market index.
//!
//! [`client`] polls the public stash stream, [`own`] reads your own tabs,
//! [`ninja`] supplies poe.ninja economy values, [`dump`] reports underpriced
//! items per tab and how long each tab has sat at its price, [`extract`]
//! triages a freshly identified dump tab, and [`exchange`] stores GGG's hourly
//! Currency Exchange digests for later analysis. [`listing`] holds the one
//! listed-versus-value rule they all share.

pub mod auth;
pub mod client;
pub mod config;
pub mod dump;
pub mod exchange;
pub mod extract;
pub mod listing;
pub mod model;
pub mod ninja;
pub mod overlay;
pub mod own;
pub mod price;
pub mod rates;
pub mod render;
pub mod store;
