//! Host application, execution policy and authorized provider communication.
mod application;
mod domain;
mod llm;
mod ports;

mod api;
mod bootstrap;
mod devices;
mod infrastructure;
mod rig;
#[cfg(test)]
mod test_support;

mod server;
pub use server::run;
