//! Generate utcp HTTP manuals for the Azure DevOps REST API from Microsoft's archived specs.

pub mod analytics;
pub mod archive;
pub mod bodies;
pub mod cli;
pub mod config;
pub mod generate;
pub mod http;
pub mod live;
pub mod pat;
pub mod readtest;
pub mod routes;
pub mod sdk;
pub mod sdk_go;
pub mod sdk_node;
pub mod sdkops;
pub mod searchtest;
pub mod text;
pub mod utcp;
pub mod writetest;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_sdk;
#[cfg(test)]
mod tests_search;
