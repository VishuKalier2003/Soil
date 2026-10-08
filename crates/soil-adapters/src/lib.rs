//! Soil adapters for GitHub, Kubernetes, generic CI and HTTP.
//!
//! Each adapter translates normalized Soil invocations into provider-shaped requests sent over
//! a [`Transport`], and translates provider-shaped responses back into normalized
//! [`soil_core::adapter::ExecutionOutcome`]s and [`soil_core::adapter::Observation`]s. Provider
//! semantics (status codes, payload shapes) never leave this crate.
//!
//! The MVP ships one transport, [`sim::SimWorld`]: a deterministic in-memory model of the
//! four providers with fault injection, used by the demo, the CLI and the tests. Live
//! transports implement the same trait.

pub mod ci;
pub mod github;
pub mod http;
pub mod kubernetes;
pub mod sim;
pub mod transport;

mod support;

use soil_core::adapter::{AdapterCatalog, AdapterRegistry};
use std::sync::Arc;
pub use transport::{ProviderRequest, ProviderResponse, Transport};

/// All built-in adapters over one transport.
pub fn registry(transport: Arc<dyn Transport>) -> AdapterRegistry {
    let mut r = AdapterRegistry::new();
    r.register(Arc::new(github::GitHubAdapter::new(transport.clone())));
    r.register(Arc::new(kubernetes::KubernetesAdapter::new(transport.clone())));
    r.register(Arc::new(ci::CiAdapter::new(transport.clone())));
    r.register(Arc::new(http::HttpAdapter::new(transport)));
    r
}

/// Descriptors of all built-in adapters (no transport needed).
pub fn catalog() -> AdapterCatalog {
    registry(Arc::new(transport::NoTransport)).catalog()
}
