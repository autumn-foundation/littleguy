//! The API of any connected provider, through the San Mateo proxy (ADR
//! 0031). Feature `api`.

use autumn_harvest_agent::{AgentError, BoxFuture, ErrorKind};
use littleguy_tools::api::{ApiAnswer, ApiCall, ApiIntegration, ProviderApi};
use san_mateo::{ProxyRequest, SanMateo};

use super::{Connections, ENVIRONMENT};

impl Connections {
    /// The proxy for the integrations in `keys`, and what the tools tell
    /// the model about each one. A key that the config does not have is
    /// left out.
    #[must_use]
    pub fn api(&self, keys: &[String]) -> (ApiProxy, Vec<ApiIntegration>) {
        let integrations: Vec<ApiIntegration> = self
            .integrations
            .iter()
            .filter(|(key, _)| keys.contains(key))
            .map(|(key, provider)| ApiIntegration {
                key: key.clone(),
                provider: provider.clone(),
                base_url: self
                    .san
                    .catalog()
                    .get(provider)
                    .and_then(|p| p.base_url().map(str::to_owned)),
            })
            .collect();
        let proxy = ApiProxy {
            san: self.san.clone(),
            keys: integrations.iter().map(|i| i.key.clone()).collect(),
            owner: self.owner.clone(),
        };
        (proxy, integrations)
    }
}

/// Sends calls through the San Mateo proxy. San Mateo adds the credential
/// of the owner and sends the call to the base URL of the provider, so no
/// credential leaves San Mateo and the call cannot change the host.
#[derive(Clone)]
pub struct ApiProxy {
    san: SanMateo,
    keys: Vec<String>,
    owner: String,
}

impl std::fmt::Debug for ApiProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiProxy")
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

/// A tool error that the model reads.
fn tool_error(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

impl ApiProxy {
    /// Sends `call` with the credential of the owner.
    async fn call(&self, call: ApiCall) -> Result<ApiAnswer, AgentError> {
        // The tools check the key too. This check holds for any caller.
        if !self.keys.contains(&call.integration) {
            return Err(tool_error(format!(
                "`{}` is not an integration of the API tools",
                call.integration
            )));
        }
        let key = super::newest(&self.san, &call.integration, &self.owner, "account")
            .await
            .map_err(tool_error)?;
        let mut proxy = ProxyRequest::new(&call.method, call.path);
        proxy.params = call.params;
        proxy.body = call.body;
        for (name, value) in call.headers {
            proxy = proxy.header(&name, value);
        }
        let answer = self
            .san
            .proxy(ENVIRONMENT, &key, proxy)
            .await
            .map_err(|e| tool_error(format!("the call did not reach {}: {e}", call.integration)))?;
        Ok(ApiAnswer {
            status: answer.status,
            headers: answer.headers,
            body: answer.body,
        })
    }
}

impl ProviderApi for ApiProxy {
    fn send(&self, call: ApiCall) -> BoxFuture<'_, Result<ApiAnswer, AgentError>> {
        Box::pin(self.call(call))
    }
}
