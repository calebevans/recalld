//! HTTP client for the Recalld API server.
//!
//! [`RecalldClient`] wraps `reqwest::Client` and provides typed methods
//! for each API endpoint. All methods return deserialized response types.
//! HTTP errors and non-2xx status codes are converted to [`CliError`](crate::cli::CliError).

use reqwest::Client;
use serde::de::DeserializeOwned;

use crate::cli::output::{
    ForgetResult, InspectView, MemoryView, NamespaceStatsView, NamespaceView, ReinforceResult,
    SearchResult, StoreResult, StatusView, SweepResult,
};

/// HTTP client for the Recalld API server.
///
/// All methods return deserialized response types. HTTP errors and
/// non-2xx status codes are converted to `CliError`.
pub struct RecalldClient {
    client: Client,
    base_url: String,
}

impl RecalldClient {
    /// Create a new client.
    ///
    /// * `base_url` — API server root (e.g., "http://localhost:7878").
    ///   Trailing slash is stripped.
    pub fn new(base_url: &str) -> crate::cli::Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(crate::cli::CliError::Http)?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Build a full URL from a path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Send a GET request and deserialize the response.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> crate::cli::Result<T> {
        let resp = self.client.get(self.url(path)).send().await?;
        self.handle_response(resp).await
    }

    /// Send a GET request with query parameters.
    async fn get_with_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> crate::cli::Result<T> {
        let resp = self
            .client
            .get(self.url(path))
            .query(query)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    /// Send a POST request with a JSON body.
    async fn post<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::cli::Result<T> {
        let resp = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    /// Send a DELETE request.
    async fn delete<T: DeserializeOwned>(&self, path: &str) -> crate::cli::Result<T> {
        let resp = self.client.delete(self.url(path)).send().await?;
        self.handle_response(resp).await
    }

    /// Check status code and deserialize the body.
    async fn handle_response<T: DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> crate::cli::Result<T> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::cli::CliError::Api {
                status: status.as_u16(),
                body,
            });
        }
        let body = resp.json::<T>().await?;
        Ok(body)
    }

    // ── Public API Methods ─────────────────────────────────────────

    /// POST /v1/memories — store a new memory.
    pub async fn store_memory(
        &self,
        text: &str,
        tags: &[String],
        namespace: Option<&str>,
        parent_id: Option<&uuid::Uuid>,
    ) -> crate::cli::Result<StoreResult> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            summary: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            full_text: Option<&'a str>,
            tags: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            parent_id: Option<&'a uuid::Uuid>,
        }

        // If text exceeds 2,000 bytes, treat it as full_text and let
        // the server generate a summary. Otherwise, use it as the summary.
        let (summary, full_text) = if text.len() > 2000 {
            (&text[..200], Some(text)) // Use first 200 chars as provisional summary
        } else {
            (text, None)
        };

        let body = Body {
            summary,
            full_text,
            tags,
            namespace,
            parent_id,
        };

        self.post("/v1/memories", &body).await
    }

    /// POST /v1/search — search memories.
    pub async fn search_memories(
        &self,
        query: &str,
        limit: u32,
        namespace: Option<&str>,
        include_ghosts: bool,
        tags: &[String],
        depth: u32,
        min_strength: Option<f32>,
    ) -> crate::cli::Result<SearchResult> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            q: &'a str,
            limit: u32,
            depth: u32,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            include_ghosts: bool,
            #[serde(skip_serializing_if = "<[String]>::is_empty")]
            tags: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            min_strength: Option<f32>,
        }

        let body = Body {
            q: query,
            limit,
            depth,
            namespace,
            include_ghosts,
            tags,
            min_strength,
        };

        self.post("/v1/search", &body).await
    }

    /// GET /v1/memories/:id — get a memory by ID.
    pub async fn get_memory(&self, id: &uuid::Uuid) -> crate::cli::Result<MemoryView> {
        self.get(&format!("/v1/memories/{id}")).await
    }

    /// DELETE /v1/memories/:id — delete a memory.
    pub async fn delete_memory(&self, id: &uuid::Uuid) -> crate::cli::Result<ForgetResult> {
        self.delete(&format!("/v1/memories/{id}")).await
    }

    /// POST /v1/memories/:id/reinforce — manually reinforce a memory.
    pub async fn reinforce_memory(
        &self,
        id: &uuid::Uuid,
    ) -> crate::cli::Result<ReinforceResult> {
        self.post(&format!("/v1/memories/{id}/reinforce"), &()).await
    }

    /// GET /v1/memories/:id/inspect — full debug view.
    pub async fn inspect_memory(&self, id: &uuid::Uuid) -> crate::cli::Result<InspectView> {
        self.get(&format!("/v1/memories/{id}/inspect")).await
    }

    /// GET /v1/namespaces — list all namespaces.
    pub async fn list_namespaces(&self) -> crate::cli::Result<Vec<NamespaceView>> {
        self.get("/v1/namespaces").await
    }

    /// POST /v1/namespaces — create a namespace.
    pub async fn create_namespace(
        &self,
        name: &str,
        dim: u32,
        initial_stability: f32,
    ) -> crate::cli::Result<NamespaceView> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            name: &'a str,
            embedding_dim: u32,
            initial_stability: f32,
        }
        self.post(
            "/v1/namespaces",
            &Body {
                name,
                embedding_dim: dim,
                initial_stability,
            },
        )
        .await
    }

    /// GET /v1/namespaces/:name/stats — namespace statistics.
    pub async fn namespace_stats(
        &self,
        name: Option<&str>,
    ) -> crate::cli::Result<Vec<NamespaceStatsView>> {
        match name {
            Some(n) => {
                let s: NamespaceStatsView =
                    self.get(&format!("/v1/namespaces/{n}/stats")).await?;
                Ok(vec![s])
            }
            None => self.get("/v1/namespaces/stats").await,
        }
    }

    /// POST /v1/sweep — trigger a decay sweep.
    pub async fn sweep(
        &self,
        dry_run: bool,
        namespace: Option<&str>,
    ) -> crate::cli::Result<SweepResult> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            dry_run: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            namespace: Option<&'a str>,
        }
        self.post("/v1/sweep", &Body { dry_run, namespace })
            .await
    }

    /// GET /v1/status — system health.
    pub async fn status(&self) -> crate::cli::Result<StatusView> {
        self.get("/v1/status").await
    }

    /// GET /v1/memories/export — bulk export.
    ///
    /// Returns all matching memories as a vector. For v1, the entire
    /// response is loaded into memory. A streaming approach may be
    /// added in a future version for very large datasets.
    pub async fn export(
        &self,
        namespace: Option<&str>,
        include_text: bool,
        include_embeddings: bool,
    ) -> crate::cli::Result<Vec<MemoryView>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(ns) = namespace {
            params.push(("namespace", ns.to_string()));
        }
        if include_text {
            params.push(("includeText", "true".to_string()));
        }
        if include_embeddings {
            params.push(("includeEmbeddings", "true".to_string()));
        }
        let query_pairs: Vec<(&str, &str)> =
            params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.get_with_query("/v1/memories/export", &query_pairs)
            .await
    }
}
