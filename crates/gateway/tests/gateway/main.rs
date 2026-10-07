//! Gateway integration tests: the router in process, Alpaca and Google key
//! servers mocked.

mod account;
mod client;
#[cfg(test)]
mod common;
mod core;
mod issuer;
mod orders;
mod tokenization;
mod wallet;
