//! The Google API of the owner, through the San Mateo proxy. Feature
//! `google`.

use autumn_harvest_agent::{AgentError, BoxFuture, ErrorKind};
use littleguy_tools::google::{ApiRequest, ApiResponse, GoogleApi};
use san_mateo::store::ConnectionFilter;
use san_mateo::{ConnectionKey, ProxyRequest, SanMateo};

use super::{Connections, ENVIRONMENT};

impl Connections {
    /// The Google API of the owner, through the connection of
    /// `integration`.
    #[must_use]
    pub fn google(&self, integration: &str) -> GoogleProxy {
        GoogleProxy {
            san: self.san.clone(),
            integration: integration.to_owned(),
            owner: self.owner.clone(),
        }
    }
}

/// Sends Google API calls through the San Mateo proxy. San Mateo adds the
/// token of the owner and refreshes it, so no token leaves San Mateo.
#[derive(Clone)]
pub struct GoogleProxy {
    san: SanMateo,
    integration: String,
    owner: String,
}

impl std::fmt::Debug for GoogleProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProxy")
            .field("integration", &self.integration)
            .finish_non_exhaustive()
    }
}

/// A tool error that the model reads.
fn tool_error(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

impl GoogleProxy {
    /// The newest connection of the integration for the owner.
    async fn connection(&self) -> Result<ConnectionKey, AgentError> {
        let filter = ConnectionFilter {
            provider_config_key: Some(self.integration.clone()),
            end_user_id: Some(self.owner.clone()),
            ..ConnectionFilter::default()
        };
        let summaries = self
            .san
            .list_connections(ENVIRONMENT, &filter)
            .await
            .map_err(|e| tool_error(format!("the connections do not read: {e}")))?;
        let newest = summaries
            .into_iter()
            .filter(|c| !c.refresh_exhausted)
            .max_by_key(|c| c.created_at)
            .ok_or_else(|| {
                tool_error(format!(
                    "no Google account is connected. The owner runs `lg connect {}`",
                    self.integration
                ))
            })?;
        Ok(ConnectionKey::new(&self.integration, &newest.connection_id))
    }

    /// Sends `request` with the token of the owner.
    async fn call(&self, request: ApiRequest) -> Result<ApiResponse, AgentError> {
        let key = self.connection().await?;
        let mut proxy = ProxyRequest::new(request.method, request.endpoint);
        proxy.params = request.params;
        if let Some(body) = request.body {
            proxy.body = serde_json::to_vec(&body)
                .map_err(|e| tool_error(format!("the request does not encode: {e}")))?;
            proxy = proxy.header("content-type", "application/json");
        }
        let answer = self
            .san
            .proxy(ENVIRONMENT, &key, proxy)
            .await
            .map_err(|e| tool_error(format!("the call did not reach Google: {e}")))?;
        Ok(ApiResponse {
            status: answer.status,
            body: answer.body,
        })
    }
}

impl GoogleApi for GoogleProxy {
    fn send(&self, request: ApiRequest) -> BoxFuture<'_, Result<ApiResponse, AgentError>> {
        Box::pin(self.call(request))
    }
}
