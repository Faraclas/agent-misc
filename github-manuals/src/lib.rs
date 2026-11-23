//! Generate utcp HTTP manuals for the GitHub REST API from GitHub's OpenAPI description.

pub mod cli;
pub mod config;
pub mod generate;
pub mod http;
pub mod names;
pub mod source;
pub mod text;
pub mod utcp;

#[cfg(test)]
mod tests;
