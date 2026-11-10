//! OAuth connections, kept by an embedded San Mateo on SQLite.
//!
//! San Mateo seals each token with AES-256-GCM and refreshes it before it
//! expires. Its data lives in `san-mateo.db` next to `lg.db`. The sealing
//! key comes from `LG_SAN_MATEO_KEY` only, so the file alone opens nothing.
//!
//! The connect pages and the OAuth callback listen on a loopback address.
//! The owner runs `lg connect <integration>` and opens the link in a
//! browser on the same machine. The REST API needs a secret key that the
//! daemon makes at each start and never shows, so no other client uses it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use littleguy_core::{Integration, SanMateoConfig};
use san_mateo::store::ConnectionFilter;
use san_mateo::{
    Background, BackgroundSettings, Catalog, ConnectSessionInput, ConnectionKey, EndUser,
    EnvironmentConfig, HttpTransport, IntegrationPatch, KeyRing, NewIntegration, SanMateo, Secret,
};
use san_mateo_sqlite::SqliteStore;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::protocol::ConnectionEntry;

#[cfg(feature = "google")]
mod google;

/// The variable that holds the base64 sealing key, 32 bytes.
pub const KEY_VAR: &str = "LG_SAN_MATEO_KEY";

/// The id of the sealing key.
const KEY_ID: &str = "k1";

/// The San Mateo environment of the daemon.
pub const ENVIRONMENT: &str = "lg";

/// The San Mateo database, next to `lg.db`.
pub const DB_FILE: &str = "san-mateo.db";

/// The San Mateo database next to `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent().unwrap_or_else(|| Path::new(".")).join(DB_FILE)
}

/// Test hooks: a fake provider network and catalog.
#[derive(Default)]
pub struct Hooks {
    /// The HTTP transport. `None` is the real network.
    pub transport: Option<Arc<dyn HttpTransport>>,
    /// The provider catalog. `None` is the built-in one.
    pub catalog: Option<Catalog>,
}

/// A secret key for the REST API that no one else learns.
fn fresh_secret_key() -> Result<String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).context("reading randomness for the San Mateo secret key")?;
    let hex = bytes
        .iter()
        .fold(String::with_capacity(48), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    Ok(format!("sk_lg_{hex}"))
}

/// The running San Mateo: the service, its pages and its refresh loop.
pub struct Connections {
    san: SanMateo,
    owner: String,
    server_url: String,
    integrations: Vec<String>,
    background: Background,
    stop: oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Connections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connections")
            .field("server_url", &self.server_url)
            .field("integrations", &self.integrations)
            .finish_non_exhaustive()
    }
}

impl Connections {
    /// Opens the store next to `db`, puts each integration of `config`,
    /// and serves the connect pages on `config.bind`.
    ///
    /// `key` is the base64 sealing key. `secret` reads one environment
    /// variable, for the client secrets.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is not 32 base64 bytes, the store does
    /// not open, an integration is not valid, or the address does not bind.
    pub async fn start(
        config: &SanMateoConfig,
        owner: &str,
        db: &Path,
        key: &str,
        secret: impl Fn(&str) -> Option<String> + Sync,
        hooks: Hooks,
    ) -> Result<Self> {
        let keys = KeyRing::from_base64(KEY_ID, key.trim())
            .with_context(|| format!("{KEY_VAR} must be 32 random bytes in base64"))?;
        let path = path_for(db);
        let store =
            SqliteStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
        store
            .migrate()
            .await
            .context("migrating the San Mateo store")?;
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("binding {}", config.bind))?;
        let address: SocketAddr = listener.local_addr()?;
        let server_url = format!("http://{address}");
        let mut builder = SanMateo::builder()
            .server_url(server_url.as_str())
            .environment(EnvironmentConfig::new(ENVIRONMENT, fresh_secret_key()?))
            .key_ring(keys)
            .store(Arc::new(store));
        if let Some(transport) = hooks.transport {
            builder = builder.transport(transport);
        }
        if let Some(catalog) = hooks.catalog {
            builder = builder.catalog(catalog);
        }
        let san = builder.build().context("building San Mateo")?;
        for integration in &config.integrations {
            put_integration(&san, integration, &secret).await?;
        }
        let app = san_mateo::api::router(san.clone());
        let (stop, stopped) = oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
            if let Err(error) = served {
                tracing::warn!(%error, "the San Mateo pages stopped");
            }
        });
        let background = san.start_background(BackgroundSettings {
            run_syncs: false,
            ..BackgroundSettings::default()
        });
        tracing::info!(%server_url, integrations = config.integrations.len(), "San Mateo is running");
        Ok(Self {
            san,
            owner: owner.to_owned(),
            server_url,
            integrations: config.integrations.iter().map(|i| i.key.clone()).collect(),
            background,
            stop,
            serving,
        })
    }

    /// The base URL of the connect pages.
    #[cfg(test)]
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// A link that connects one account of `integration` for the owner.
    /// The browser goes from it straight to the provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the config has no such integration, or the
    /// session does not save.
    pub async fn connect(&self, integration: &str) -> Result<String> {
        if !self.integrations.iter().any(|key| key == integration) {
            bail!("the config has no integration `{integration}`. Add a [[san_mateo.integration]]");
        }
        let input = ConnectSessionInput {
            end_user: Some(EndUser {
                id: self.owner.clone(),
                ..EndUser::default()
            }),
            allowed_integrations: vec![integration.to_owned()],
            ..ConnectSessionInput::default()
        };
        let session = self
            .san
            .create_connect_session(ENVIRONMENT, input)
            .await
            .context("making a connect session")?;
        Ok(format!(
            "{}/oauth/connect/{integration}?connect_session_token={}",
            self.server_url,
            session.token.expose()
        ))
    }

    /// Every connection, with no credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub async fn list(&self) -> Result<Vec<ConnectionEntry>> {
        let summaries = self
            .san
            .list_connections(ENVIRONMENT, &ConnectionFilter::default())
            .await
            .context("listing the connections")?;
        Ok(summaries
            .into_iter()
            .map(|c| ConnectionEntry {
                integration: c.provider_config_key,
                connection: c.connection_id,
                provider: c.provider,
                created_at: c.created_at.to_rfc3339(),
                refresh_exhausted: c.refresh_exhausted,
            })
            .collect())
    }

    /// Deletes one connection. Returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub async fn disconnect(&self, integration: &str, connection: &str) -> Result<bool> {
        let key = ConnectionKey::new(integration, connection);
        match self.san.delete_connection(ENVIRONMENT, &key).await {
            Ok(()) => Ok(true),
            Err(san_mateo::Error::UnknownConnection(_)) => Ok(false),
            Err(error) => Err(error).context("deleting the connection"),
        }
    }

    /// Stops the refresh loop and the pages.
    pub async fn stop(self) {
        self.background.stop().await;
        let _ = self.stop.send(());
        let _ = self.serving.await;
        self.san.flush().await;
    }
}

/// Adds `integration`, or updates it to match the config.
async fn put_integration(
    san: &SanMateo,
    integration: &Integration,
    secret: &(impl Fn(&str) -> Option<String> + Sync),
) -> Result<()> {
    let client_secret = match &integration.client_secret_env {
        Some(var) => Some(Secret::new(
            secret(var)
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!("set {var}, the client secret of `{}`", integration.key)
                })?,
        )),
        None => None,
    };
    match san.get_integration(ENVIRONMENT, &integration.key).await {
        Ok(current) if current.provider != integration.provider => bail!(
            "integration `{}` is stored with provider `{}`, not `{}`. Use a new key",
            integration.key,
            current.provider,
            integration.provider
        ),
        Ok(_) => {
            let patch = IntegrationPatch {
                client_id: integration.client_id.clone(),
                client_secret,
                scopes: Some(integration.scopes.clone()),
                ..IntegrationPatch::default()
            };
            san.update_integration(ENVIRONMENT, &integration.key, patch)
                .await
                .with_context(|| format!("updating integration `{}`", integration.key))?;
        }
        Err(san_mateo::Error::UnknownIntegration(_)) => {
            let input = NewIntegration {
                unique_key: integration.key.clone(),
                provider: integration.provider.clone(),
                client_id: integration.client_id.clone(),
                client_secret,
                scopes: integration.scopes.clone(),
                ..NewIntegration::default()
            };
            san.create_integration(ENVIRONMENT, input)
                .await
                .with_context(|| format!("adding integration `{}`", integration.key))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading integration `{}`", integration.key));
        }
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    #[cfg(feature = "google")]
    use littleguy_tools::google::{ApiRequest, GoogleApi};
    use san_mateo::MemoryTransport;
    use san_mateo::transport::HttpResponse;

    use super::*;

    const TOKEN_URL: &str = "https://auth.acme.example.com/token";
    const CATALOG: &str = r"
acme:
    display_name: Acme
    auth_mode: OAUTH2
    authorization_url: https://auth.acme.example.com/authorize
    token_url: https://auth.acme.example.com/token
    default_scopes: [read]
    proxy:
        base_url: https://api.acme.example.com
github:
    display_name: GitHub
    auth_mode: OAUTH2
    authorization_url: https://github.com/login/oauth/authorize
    token_url: https://github.com/login/oauth/access_token
google:
    display_name: Google
    auth_mode: OAUTH2
    authorization_url: https://accounts.google.com/o/oauth2/v2/auth
    token_url: https://oauth2.googleapis.com/token
    proxy:
        base_url: https://www.googleapis.com
";

    /// The base URL of the fake Google API.
    #[cfg(feature = "google")]
    pub const GOOGLE: &str = "https://www.googleapis.com";

    #[cfg(feature = "google")]
    /// A San Mateo section with one `google` integration.
    pub fn google_section() -> SanMateoConfig {
        let mut config = section("google", None);
        config.integrations[0].key = String::from("google");
        config
    }

    #[cfg(feature = "google")]
    /// Stores a Google connection for `owner`, as a finished connect would.
    pub async fn import_google(connections: &Connections, owner: &str) {
        let credentials =
            san_mateo::Credentials::OAuth2(san_mateo::credentials::OAuth2Credentials {
                access_token: Secret::new(String::from("at-google")),
                refresh_token: None,
                expires_at: None,
                raw: serde_json::Value::Null,
            });
        let input = san_mateo::ImportConnection {
            connection_id: Some(String::from("c-1")),
            provider_config_key: String::from("google"),
            credentials,
            connection_config: serde_json::Map::new(),
            metadata: serde_json::Map::new(),
            tags: san_mateo::Tags::default(),
            end_user: Some(EndUser {
                id: owner.to_owned(),
                ..EndUser::default()
            }),
            webhook_url_override: None,
        };
        connections
            .san
            .import_connection(ENVIRONMENT, input)
            .await
            .unwrap();
    }

    #[cfg(feature = "google")]
    #[tokio::test(flavor = "multi_thread")]
    async fn google_calls_carry_the_owner_token_and_need_a_connection() {
        let dir = tempfile::tempdir().unwrap();
        let fake = MemoryTransport::new();
        let url = format!("{GOOGLE}/gmail/v1/users/me/messages");
        fake.reply(
            "GET",
            &url,
            HttpResponse::json(200, &serde_json::json!({"messages": []})),
        );
        let connections = start(&dir.path().join("lg.db"), &google_section(), &fake)
            .await
            .unwrap();
        let api = connections.google("google");
        assert!(format!("{api:?}").contains("google"));
        let request = ApiRequest {
            method: "GET",
            endpoint: String::from("/gmail/v1/users/me/messages"),
            params: vec![(String::from("q"), String::from("x"))],
            body: None,
        };
        let Err(none) = api.send(request.clone()).await else {
            panic!("a call with no connection went out");
        };
        assert!(none.to_string().contains("lg connect google"), "{none}");
        import_google(&connections, "someone-else").await;
        assert!(
            api.send(request.clone()).await.is_err(),
            "only the owner's account counts"
        );
        connections.disconnect("google", "c-1").await.unwrap();
        import_google(&connections, "mark").await;
        let answer = api.send(request).await.unwrap();
        assert_eq!(answer.status, 200);
        let sent = fake.requests_to(&url);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].header_value("authorization"),
            Some("Bearer at-google")
        );
        assert!(sent[0].url.contains("q=x"));
        let post = ApiRequest {
            method: "POST",
            endpoint: String::from("/gmail/v1/users/me/drafts"),
            params: Vec::new(),
            body: Some(serde_json::json!({"message": {"raw": "eA"}})),
        };
        let missing = api.send(post).await.unwrap();
        assert_eq!(missing.status, 404, "the fake has no route");
        let drafts = fake.requests_to(&format!("{GOOGLE}/gmail/v1/users/me/drafts"));
        assert_eq!(
            drafts[0].header_value("content-type"),
            Some("application/json")
        );
        connections.stop().await;
    }

    /// 32 zero bytes in base64. Tests only.
    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    pub fn section(provider: &str, secret_env: Option<&str>) -> SanMateoConfig {
        SanMateoConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            integrations: vec![Integration {
                key: String::from("acme"),
                provider: provider.to_owned(),
                client_id: Some(String::from("client-1")),
                client_secret_env: secret_env.map(ToOwned::to_owned),
                scopes: vec![String::from("read")],
            }],
        }
    }

    fn hooks(fake: &MemoryTransport) -> Hooks {
        Hooks {
            transport: Some(Arc::new(fake.clone())),
            catalog: Some(Catalog::from_yaml_str(CATALOG).unwrap()),
        }
    }

    pub async fn start(
        db: &Path,
        config: &SanMateoConfig,
        fake: &MemoryTransport,
    ) -> Result<Connections> {
        let secret = |var: &str| (var == "LG_ACME_SECRET").then(|| String::from("shh"));
        Connections::start(config, "mark", db, KEY, secret, hooks(fake)).await
    }

    fn query(url: &reqwest::Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }

    /// The owner opens the link, the provider sends the browser back, and
    /// the connection is kept across a restart.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_connects_an_account_that_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let fake = MemoryTransport::new();
        fake.reply(
            "POST",
            TOKEN_URL,
            HttpResponse::json(
                200,
                &serde_json::json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600}),
            ),
        );
        let config = section("acme", Some("LG_ACME_SECRET"));
        let connections = start(&db, &config, &fake).await.unwrap();
        assert_eq!(connections.list().await.unwrap(), Vec::new());

        let link = connections.connect("acme").await.unwrap();
        assert!(link.starts_with(connections.server_url()));
        let browser = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let start_page = browser.get(&link).send().await.unwrap();
        assert_eq!(start_page.status(), reqwest::StatusCode::FOUND);
        let location = start_page.headers()["location"].to_str().unwrap();
        let authorize = reqwest::Url::parse(location).unwrap();
        assert_eq!(authorize.host_str(), Some("auth.acme.example.com"));
        assert_eq!(query(&authorize, "client_id").as_deref(), Some("client-1"));
        let state = query(&authorize, "state").unwrap();
        let callback = format!(
            "{}/oauth/callback?code=code-1&state={state}",
            connections.server_url()
        );
        let back = browser.get(callback).send().await.unwrap();
        assert!(back.status().is_success(), "{}", back.status());

        let entries = connections.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].integration, "acme");
        assert!(!entries[0].refresh_exhausted);
        let token_calls = fake.requests_to(TOKEN_URL);
        assert_eq!(token_calls.len(), 1);
        let form = String::from_utf8_lossy(&token_calls[0].body).into_owned();
        assert!(
            form.contains("client_secret=shh"),
            "the secret comes from its variable"
        );
        connections.stop().await;

        let again = start(&db, &config, &fake).await.unwrap();
        let kept = again.list().await.unwrap();
        assert_eq!(kept, entries, "the connection is in san-mateo.db");
        let id = kept[0].connection.clone();
        assert!(again.disconnect("acme", &id).await.unwrap());
        assert!(!again.disconnect("acme", &id).await.unwrap());
        assert_eq!(again.list().await.unwrap(), Vec::new());
        again.stop().await;
        assert!(path_for(&db).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_setups_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let fake = MemoryTransport::new();
        let missing = start(&db, &section("acme", Some("LG_OTHER")), &fake).await;
        assert!(format!("{:#}", missing.unwrap_err()).contains("LG_OTHER"));
        let bad_key = Connections::start(
            &section("acme", None),
            "mark",
            &db,
            "not base64",
            |_| None,
            hooks(&fake),
        )
        .await;
        assert!(format!("{:#}", bad_key.unwrap_err()).contains(KEY_VAR));
        let unknown = start(&db, &section("nope", None), &fake).await;
        assert!(unknown.is_err());

        let connections = start(&db, &section("acme", None), &fake).await.unwrap();
        // A link holds a session token, so neither branch prints it.
        let Err(error) = connections.connect("other").await else {
            panic!("an unknown integration gave a link");
        };
        assert!(error.to_string().contains("no integration `other`"));
        connections.stop().await;
        // The key is stored with provider acme. Another provider is refused.
        let moved = start(&db, &section("github", None), &fake).await;
        assert!(format!("{:#}", moved.unwrap_err()).contains("Use a new key"));
    }

    #[test]
    fn the_secret_key_is_fresh() {
        let a = fresh_secret_key().unwrap();
        assert!(a.starts_with("sk_lg_") && a.len() == 54);
        // `assert_ne!` prints both keys on failure, so compare them plainly.
        let differ = a != fresh_secret_key().unwrap();
        assert!(differ);
    }
}
