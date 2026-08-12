//! The **only** place in kctx where a Kubernetes API handle is constructed.
//!
//! [`ReadOnly`] wraps `kube::Api` and re-exports exactly two operations: `get` and `list`. The
//! inner handle is private and never appears in a public signature, so no other module *can*
//! reach `create`, `replace`, `patch`, `delete` or `delete_collection` — introducing a mutation
//! would mean editing this file, which is small enough to review at a glance.
//!
//! `tests/readonly_guard.rs` enforces the same invariant mechanically.

use std::fmt::Debug;

use kube::api::{Api, ListParams};
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use kube::{Client, Resource};
use serde::de::DeserializeOwned;

/// The result of a capped list request.
#[derive(Debug, Clone)]
pub struct Listing<K> {
    /// The objects that were returned.
    pub items: Vec<K>,
    /// True when the server had more objects than the requested limit.
    ///
    /// kctx never pages: a single capped request keeps inspection fast and bounded, and callers
    /// say so rather than presenting a truncated count as complete.
    pub truncated: bool,
}

/// A read-only handle to one Kubernetes resource kind.
pub struct ReadOnly<K> {
    api: Api<K>,
}

impl<K> ReadOnly<K>
where
    K: Resource<Scope = NamespaceResourceScope>,
    K::DynamicType: Default,
{
    /// Read this kind within a single namespace.
    pub fn namespaced(client: Client, namespace: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
        }
    }
}

impl<K> ReadOnly<K>
where
    K: Resource<Scope = ClusterResourceScope>,
    K::DynamicType: Default,
{
    /// Read this cluster-scoped kind.
    pub fn cluster(client: Client) -> Self {
        Self {
            api: Api::all(client),
        }
    }
}

impl<K> ReadOnly<K>
where
    K: Clone + DeserializeOwned + Debug,
{
    /// `GET` a single object by name.
    pub async fn get(&self, name: &str) -> Result<K, kube::Error> {
        self.api.get(name).await
    }

    /// `LIST` at most `limit` objects.
    pub async fn list(&self, limit: u32) -> Result<Listing<K>, kube::Error> {
        let list = self.api.list(&ListParams::default().limit(limit)).await?;
        let truncated = list
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|token| !token.is_empty());
        Ok(Listing {
            items: list.items,
            truncated,
        })
    }
}
