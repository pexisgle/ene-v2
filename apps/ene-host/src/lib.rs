//! Host application, execution policy and authorized provider communication.
pub mod application;
pub mod domain;
pub mod llm;
pub mod ports;

mod api;
mod audio;
mod bootstrap;
mod devices;
mod infrastructure;
#[cfg(test)]
mod test_support;

mod server;
pub use server::run;
