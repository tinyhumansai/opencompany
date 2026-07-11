//! OpenHuman integration: process launcher plus the JSON-RPC seam.
//!
//! [`launcher`] shells out to a sibling OpenHuman checkout. [`rpc`] defines the
//! transport trait and an offline mock; [`tools`] and [`channel`] build the
//! OpenHuman-backed [`ToolProvider`](crate::ports::ToolProvider) and
//! [`ChannelAdapter`](crate::ports::ChannelAdapter) on top of it. The real HTTP
//! client in [`http_client`] compiles only under the `openhuman-rpc` feature.

mod launcher;

pub mod channel;
pub mod rpc;
pub mod tools;

#[cfg(feature = "openhuman-rpc")]
pub mod http_client;

pub use channel::OpenHumanChannelAdapter;
pub use launcher::{LaunchMode, OpenHumanLaunch};
pub use rpc::{MockOpenHumanRpc, OpenHumanRpc, rpc_method};
pub use tools::OpenHumanToolProvider;

#[cfg(feature = "openhuman-rpc")]
pub use http_client::HttpOpenHumanRpc;
