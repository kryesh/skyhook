//! The flaw-first security research product: its session factory and extension, the
//! research store its sessions keep, its tools, prompts, CVSS scoring, reports and the
//! Research view, for any host to serve.
mod capabilities;
mod config;
mod cvss;
mod modes;
mod product;
mod prompts;
mod reporting;
mod research_view;
mod session;
mod store;
mod tools;

pub use config::{ModelAssignment, ModelRole, SecurityConfig};
pub use cvss::Severity;
pub use product::{Options, Security, run_page, runs};
pub use store::{LivePolicy, Run, Store};
