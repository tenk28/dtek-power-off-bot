//! DTEK power outage schedule bot: fetches the schedule from a public mirror
//! and posts it to a Telegram channel.

pub mod bot;
pub mod config;
pub mod error;
pub mod message;
pub mod schedule;
pub mod source;
pub mod state;
pub mod telegram;
