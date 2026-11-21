//! Generate utcp HTTP manuals for the Azure DevOps REST API from Microsoft's archived specs.

pub mod archive;
pub mod bodies;
pub mod cli;
pub mod config;
pub mod generate;
pub mod http;
pub mod readtest;
pub mod routes;
pub mod text;
pub mod utcp;
pub mod writetest;

#[cfg(test)]
mod tests;
