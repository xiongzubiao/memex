use anyhow::{Context, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const AUTH_FILENAME: &str = "auth.json";
const RUNTIME_DIRNAME: &str = ".zeroclaw";
const RUNTIME_AUTH_FILENAME: &str = "auth-profiles.json";
const CURRENT_SCHEMA_VERSION: u32 = 1;
const NONCE_LEN: usize = 12;
const DEFAULT_PROFILE_NAME: &str = "default";
const OPENAI_CODEX_PROVIDER: &str = "openai-codex";
const GEMINI_PROVIDER: &str = "gemini";
const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_OAUTH_DEVICE_CODE_URL: &str = "https://auth.openai.com/oauth/device/code";
const GEMINI_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GEMINI_OAUTH_DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const GEMINI_OAUTH_SCOPES: &str =
    "openid profile email https://www.googleapis.com/auth/cloud-platform";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Codex,
    Gemini,
}

impl AuthProvider {
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Codex => OPENAI_CODEX_PROVIDER,
            Self::Gemini => GEMINI_PROVIDER,
        }
    }

    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone)]
pub enum LoginSource {
    Imported(PathBuf),
    DeviceCode,
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub provider: AuthProvider,
    pub account_id: Option<String>,
    pub source: LoginSource,
}

#[derive(Debug, Clone)]
pub struct AuthStatusEntry {
    pub provider: AuthProvider,
    pub account_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderAuthRecord {
    pub provider: String,
    pub kind: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub token_set: Option<TokenSet>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderAuthRecord>,
}

impl Default for AuthFile {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            updated_at: Utc::now(),
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemexAuthStore {
    root: PathBuf,
}

impl MemexAuthStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(AUTH_FILENAME)
    }

    pub fn secret_key_path(&self) -> PathBuf {
        self.root.join(".secret_key")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join(RUNTIME_DIRNAME)
    }

    pub async fn load(&self) -> Result<AuthFile> {
        let path = self.path();
        if !path.exists() {
            return Ok(AuthFile::default());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read auth store at {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(AuthFile::default());
        }

        let persisted: PersistedAuthFile =
            serde_json::from_str(&raw).context("Failed to parse auth.json")?;
        let secrets = SecretStore::new(&self.root, true);

        let mut providers = BTreeMap::new();
        for (provider, persisted_record) in persisted.providers {
            let access_token =
                decrypt_optional(&secrets, persisted_record.access_token.as_deref())?;
            let refresh_token =
                decrypt_optional(&secrets, persisted_record.refresh_token.as_deref())?;
            let id_token = decrypt_optional(&secrets, persisted_record.id_token.as_deref())?;

            let token_set = match (persisted_record.kind.as_str(), access_token) {
                ("oauth", Some(access_token)) => Some(TokenSet {
                    access_token,
                    refresh_token,
                    id_token,
                    expires_at: parse_optional_datetime(persisted_record.expires_at.as_deref())?,
                    token_type: persisted_record.token_type,
                    scope: persisted_record.scope,
                }),
                ("oauth", None) => {
                    anyhow::bail!("OAuth auth record missing access token for provider {provider}");
                }
                _ => None,
            };

            providers.insert(
                provider.clone(),
                ProviderAuthRecord {
                    provider,
                    kind: persisted_record.kind,
                    account_id: persisted_record.account_id,
                    token_set,
                    created_at: parse_datetime(&persisted_record.created_at)?,
                    updated_at: parse_datetime(&persisted_record.updated_at)?,
                },
            );
        }

        Ok(AuthFile {
            schema_version: persisted.schema_version,
            updated_at: parse_datetime(&persisted.updated_at)?,
            providers,
        })
    }

    pub async fn upsert_oauth(
        &self,
        provider: &str,
        token_set: TokenSet,
        account_id: Option<String>,
    ) -> Result<()> {
        let mut auth = self.load().await?;
        let now = Utc::now();

        let created_at = auth
            .providers
            .get(provider)
            .map(|record| record.created_at)
            .unwrap_or(now);

        auth.providers.insert(
            provider.to_string(),
            ProviderAuthRecord {
                provider: provider.to_string(),
                kind: "oauth".to_string(),
                account_id,
                token_set: Some(token_set),
                created_at,
                updated_at: now,
            },
        );
        auth.updated_at = now;

        self.save(&auth).await
    }

    pub async fn remove_provider(&self, provider: &str) -> Result<bool> {
        let mut auth = self.load().await?;
        let removed = auth.providers.remove(provider).is_some();
        if removed {
            auth.updated_at = Utc::now();
            self.save(&auth).await?;
        }
        Ok(removed)
    }

    pub async fn save(&self, auth: &AuthFile) -> Result<()> {
        let secrets = SecretStore::new(&self.root, true);
        let mut providers = BTreeMap::new();

        for (provider, record) in &auth.providers {
            let (access_token, refresh_token, id_token, expires_at, token_type, scope) =
                match (&record.kind[..], &record.token_set) {
                    ("oauth", Some(token_set)) => (
                        Some(secrets.encrypt(&token_set.access_token)?),
                        encrypt_optional(&secrets, token_set.refresh_token.as_deref())?,
                        encrypt_optional(&secrets, token_set.id_token.as_deref())?,
                        token_set.expires_at.map(|value| value.to_rfc3339()),
                        token_set.token_type.clone(),
                        token_set.scope.clone(),
                    ),
                    _ => (None, None, None, None, None, None),
                };

            providers.insert(
                provider.clone(),
                PersistedProviderAuthRecord {
                    provider: record.provider.clone(),
                    kind: record.kind.clone(),
                    account_id: record.account_id.clone(),
                    access_token,
                    refresh_token,
                    id_token,
                    expires_at,
                    token_type,
                    scope,
                    created_at: record.created_at.to_rfc3339(),
                    updated_at: record.updated_at.to_rfc3339(),
                },
            );
        }

        let persisted = PersistedAuthFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            updated_at: auth.updated_at.to_rfc3339(),
            providers,
        };

        write_json_atomic(&self.path(), &persisted)
    }

    pub async fn sync_to_runtime_store(&self) -> Result<()> {
        let auth = self.load().await?;
        let runtime_dir = self.runtime_dir();
        let runtime_secrets = SecretStore::new(&runtime_dir, true);

        let mut profiles = BTreeMap::new();
        let mut active_profiles = BTreeMap::new();

        for record in auth.providers.values() {
            let Some(token_set) = &record.token_set else {
                continue;
            };
            let profile_id = runtime_profile_id(&record.provider);
            active_profiles.insert(record.provider.clone(), profile_id.clone());
            profiles.insert(
                profile_id,
                PersistedRuntimeProfile {
                    provider: record.provider.clone(),
                    profile_name: DEFAULT_PROFILE_NAME.to_string(),
                    kind: "oauth".to_string(),
                    account_id: record.account_id.clone(),
                    workspace_id: None,
                    access_token: Some(runtime_secrets.encrypt(&token_set.access_token)?),
                    refresh_token: encrypt_optional(
                        &runtime_secrets,
                        token_set.refresh_token.as_deref(),
                    )?,
                    id_token: encrypt_optional(&runtime_secrets, token_set.id_token.as_deref())?,
                    token: None,
                    expires_at: token_set.expires_at.map(|value| value.to_rfc3339()),
                    token_type: token_set.token_type.clone(),
                    scope: token_set.scope.clone(),
                    metadata: BTreeMap::new(),
                    created_at: record.created_at.to_rfc3339(),
                    updated_at: record.updated_at.to_rfc3339(),
                },
            );
        }

        let persisted = PersistedRuntimeProfiles {
            schema_version: CURRENT_SCHEMA_VERSION,
            updated_at: auth.updated_at.to_rfc3339(),
            active_profiles,
            profiles,
        };

        write_json_atomic(&runtime_dir.join(RUNTIME_AUTH_FILENAME), &persisted)
    }
}

pub async fn import_codex_cli_auth(store: &MemexAuthStore, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let imported: CodexAuthFile = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("Failed to read Codex auth file {}", path.display()))?,
    )
    .with_context(|| format!("Failed to parse Codex auth file {}", path.display()))?;

    let access_token = imported.tokens.access_token;
    store
        .upsert_oauth(
            "openai-codex",
            TokenSet {
                expires_at: extract_expiry_from_jwt(&access_token),
                access_token: access_token.clone(),
                refresh_token: imported.tokens.refresh_token,
                id_token: imported.tokens.id_token,
                token_type: Some("Bearer".to_string()),
                scope: None,
            },
            imported
                .tokens
                .account_id
                .or_else(|| extract_account_id_from_jwt(&access_token)),
        )
        .await?;

    Ok(true)
}

pub async fn import_gemini_cli_auth(store: &MemexAuthStore, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let imported: GeminiOauthCreds = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("Failed to read Gemini OAuth file {}", path.display()))?,
    )
    .with_context(|| format!("Failed to parse Gemini OAuth file {}", path.display()))?;

    let account_id = imported
        .id_token
        .as_deref()
        .and_then(extract_account_email_from_id_token);

    store
        .upsert_oauth(
            "gemini",
            TokenSet {
                access_token: imported.access_token,
                refresh_token: imported.refresh_token,
                id_token: imported.id_token,
                expires_at: imported
                    .expiry_date
                    .and_then(DateTime::from_timestamp_millis),
                token_type: imported.token_type.or(Some("Bearer".to_string())),
                scope: imported.scope,
            },
            account_id,
        )
        .await?;

    Ok(true)
}

pub fn normalize_provider(input: &str) -> Result<AuthProvider> {
    let normalized = input.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "codex" | "openai-codex" | "openai_codex" => Ok(AuthProvider::Codex),
        "gemini" => Ok(AuthProvider::Gemini),
        _ => anyhow::bail!("Unsupported provider '{input}'. Expected one of: codex, gemini"),
    }
}

pub async fn login(root: &Path, provider: AuthProvider, device_code: bool) -> Result<LoginResult> {
    let store = MemexAuthStore::new(root);

    let imported_path = if device_code {
        None
    } else {
        match provider {
            AuthProvider::Codex => {
                if let Some(path) = default_codex_cli_auth_path() {
                    if import_codex_cli_auth(&store, &path).await? {
                        Some(path)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            AuthProvider::Gemini => {
                if let Some(path) = default_gemini_cli_auth_path() {
                    if import_gemini_cli_auth(&store, &path).await? {
                        Some(path)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    };

    if let Some(path) = imported_path {
        store.sync_to_runtime_store().await?;
        let account_id = store
            .load()
            .await?
            .providers
            .get(provider.canonical())
            .and_then(|record| record.account_id.clone());
        return Ok(LoginResult {
            provider,
            account_id,
            source: LoginSource::Imported(path),
        });
    }

    let result = match provider {
        AuthProvider::Codex => login_codex_with_device_code(&store).await?,
        AuthProvider::Gemini => login_gemini_with_device_code(&store).await?,
    };
    Ok(result)
}

pub async fn logout(root: &Path, provider: AuthProvider) -> Result<bool> {
    let store = MemexAuthStore::new(root);
    let removed = store.remove_provider(provider.canonical()).await?;
    if removed {
        store.sync_to_runtime_store().await?;
    }
    Ok(removed)
}

pub async fn status(root: &Path) -> Result<Vec<AuthStatusEntry>> {
    let store = MemexAuthStore::new(root);
    let auth = store.load().await?;

    let mut entries: Vec<AuthStatusEntry> = auth
        .providers
        .values()
        .filter_map(|record| {
            let provider = canonical_to_provider(&record.provider)?;
            Some(AuthStatusEntry {
                provider,
                account_id: record.account_id.clone(),
                expires_at: record
                    .token_set
                    .as_ref()
                    .and_then(|tokens| tokens.expires_at),
            })
        })
        .collect();

    entries.sort_by_key(|entry| entry.provider.cli_name().to_string());
    Ok(entries)
}

pub async fn auth_summary_line(root: &Path) -> Result<String> {
    let entries = status(root).await?;
    if entries.is_empty() {
        return Ok("none".to_string());
    }

    let labels: Vec<String> = entries
        .iter()
        .map(|entry| {
            if let Some(account) = &entry.account_id {
                format!(
                    "{} ({})",
                    entry.provider.cli_name(),
                    redact_for_display(account)
                )
            } else {
                entry.provider.cli_name().to_string()
            }
        })
        .collect();
    Ok(labels.join(", "))
}

fn canonical_to_provider(input: &str) -> Option<AuthProvider> {
    match input {
        OPENAI_CODEX_PROVIDER => Some(AuthProvider::Codex),
        GEMINI_PROVIDER => Some(AuthProvider::Gemini),
        _ => None,
    }
}

fn default_codex_cli_auth_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex").join("auth.json"))
}

fn default_gemini_cli_auth_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini").join("oauth_creds.json"))
}

fn redact_for_display(input: &str) -> String {
    if input.len() <= 8 {
        return "*".repeat(input.len().max(1));
    }
    let head = &input[..4];
    let tail = &input[input.len() - 4..];
    format!("{head}...{tail}")
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Clone)]
struct DeviceCodeStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
    message: Option<String>,
}

async fn login_codex_with_device_code(store: &MemexAuthStore) -> Result<LoginResult> {
    let client = reqwest::Client::new();
    let device = start_openai_device_code_flow(&client).await?;

    eprintln!("OpenAI Codex OAuth device login started.");
    eprintln!("Visit: {}", device.verification_uri);
    eprintln!("Code:  {}", device.user_code);
    if let Some(uri_complete) = &device.verification_uri_complete {
        eprintln!("Fast link: {uri_complete}");
    }
    if let Some(message) = &device.message {
        eprintln!("{message}");
    }

    let token_set = poll_openai_device_code_tokens(&client, &device).await?;
    let account_id = extract_account_id_from_jwt(&token_set.access_token);
    store
        .upsert_oauth(OPENAI_CODEX_PROVIDER, token_set, account_id.clone())
        .await?;
    store.sync_to_runtime_store().await?;

    Ok(LoginResult {
        provider: AuthProvider::Codex,
        account_id,
        source: LoginSource::DeviceCode,
    })
}

async fn login_gemini_with_device_code(store: &MemexAuthStore) -> Result<LoginResult> {
    let client = reqwest::Client::new();
    let (client_id, client_secret) = gemini_oauth_credentials()?;
    let device = start_gemini_device_code_flow(&client, &client_id).await?;

    eprintln!("Gemini OAuth device login started.");
    eprintln!("Visit: {}", device.verification_uri);
    eprintln!("Code:  {}", device.user_code);
    if let Some(uri_complete) = &device.verification_uri_complete {
        eprintln!("Fast link: {uri_complete}");
    }

    let token_set =
        poll_gemini_device_code_tokens(&client, &device, &client_id, &client_secret).await?;
    let account_id = token_set
        .id_token
        .as_deref()
        .and_then(extract_account_email_from_id_token);
    store
        .upsert_oauth(GEMINI_PROVIDER, token_set, account_id.clone())
        .await?;
    store.sync_to_runtime_store().await?;

    Ok(LoginResult {
        provider: AuthProvider::Gemini,
        account_id,
        source: LoginSource::DeviceCode,
    })
}

fn gemini_oauth_credentials() -> Result<(String, String)> {
    let client_id = std::env::var("GEMINI_OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("GEMINI_OAUTH_CLIENT_ID is required for Gemini OAuth device login")
        })?;
    let client_secret = std::env::var("GEMINI_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("GEMINI_OAUTH_CLIENT_SECRET is required for Gemini OAuth device login")
        })?;
    Ok((client_id, client_secret))
}

async fn start_openai_device_code_flow(client: &reqwest::Client) -> Result<DeviceCodeStart> {
    let response = client
        .post(OPENAI_OAUTH_DEVICE_CODE_URL)
        .form(&[
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
            ("scope", "openid profile email offline_access"),
        ])
        .send()
        .await
        .context("Failed to start OpenAI OAuth device-code flow")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI device-code start failed ({status}): {body}");
    }

    let parsed: OpenAiDeviceCodeResponse = response
        .json()
        .await
        .context("Failed to parse OpenAI device-code response")?;

    Ok(DeviceCodeStart {
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: parsed.verification_uri,
        verification_uri_complete: parsed.verification_uri_complete,
        expires_in: parsed.expires_in,
        interval: parsed.interval.unwrap_or(5).max(1),
        message: parsed.message,
    })
}

async fn poll_openai_device_code_tokens(
    client: &reqwest::Client,
    device: &DeviceCodeStart,
) -> Result<TokenSet> {
    let started = std::time::Instant::now();
    let mut interval_secs = device.interval.max(1);

    loop {
        if started.elapsed() > std::time::Duration::from_secs(device.expires_in) {
            anyhow::bail!("OpenAI device-code flow timed out before authorization completed");
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        let response = client
            .post(OPENAI_OAUTH_TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", OPENAI_OAUTH_CLIENT_ID),
            ])
            .send()
            .await
            .context("Failed polling OpenAI device-code token endpoint")?;

        if response.status().is_success() {
            return parse_oauth_token_response(response).await;
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&text) {
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs = interval_secs.saturating_add(5);
                    continue;
                }
                "access_denied" => anyhow::bail!("OpenAI device-code authorization was denied"),
                "expired_token" => anyhow::bail!("OpenAI device-code expired"),
                _ => anyhow::bail!(
                    "OpenAI device-code polling failed ({status}): {}",
                    err.error_description.unwrap_or(err.error)
                ),
            }
        }

        anyhow::bail!("OpenAI device-code polling failed ({status}): {text}");
    }
}

async fn start_gemini_device_code_flow(
    client: &reqwest::Client,
    client_id: &str,
) -> Result<DeviceCodeStart> {
    let response = client
        .post(GEMINI_OAUTH_DEVICE_CODE_URL)
        .form(&[("client_id", client_id), ("scope", GEMINI_OAUTH_SCOPES)])
        .send()
        .await
        .context("Failed to start Gemini OAuth device-code flow")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read Gemini device-code response body")?;
    if !status.is_success() {
        if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&body) {
            anyhow::bail!(
                "Gemini device-code start failed: {} - {}",
                err.error,
                err.error_description.unwrap_or_default()
            );
        }
        anyhow::bail!("Gemini device-code start failed ({status}): {body}");
    }

    let parsed: GeminiDeviceCodeResponse =
        serde_json::from_str(&body).context("Failed to parse Gemini device-code response")?;
    Ok(DeviceCodeStart {
        device_code: parsed.device_code,
        user_code: parsed.user_code.clone(),
        verification_uri: parsed.verification_url.clone(),
        verification_uri_complete: Some(format!(
            "{}?user_code={}",
            parsed.verification_url, parsed.user_code
        )),
        expires_in: parsed.expires_in.unwrap_or(1800),
        interval: parsed.interval.unwrap_or(5).max(1),
        message: None,
    })
}

async fn poll_gemini_device_code_tokens(
    client: &reqwest::Client,
    device: &DeviceCodeStart,
    client_id: &str,
    client_secret: &str,
) -> Result<TokenSet> {
    let started = std::time::Instant::now();
    let mut interval_secs = device.interval.max(1);

    loop {
        if started.elapsed() > std::time::Duration::from_secs(device.expires_in) {
            anyhow::bail!("Gemini device-code flow timed out before authorization completed");
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        let response = client
            .post(GEMINI_OAUTH_TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .context("Failed polling Gemini device-code token endpoint")?;

        if response.status().is_success() {
            return parse_oauth_token_response(response).await;
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&text) {
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs = interval_secs.saturating_add(5);
                    continue;
                }
                "access_denied" => anyhow::bail!("Gemini device-code authorization was denied"),
                "expired_token" => anyhow::bail!("Gemini device-code expired"),
                _ => anyhow::bail!(
                    "Gemini device-code polling failed ({status}): {}",
                    err.error_description.unwrap_or(err.error)
                ),
            }
        }

        anyhow::bail!("Gemini device-code polling failed ({status}): {text}");
    }
}

async fn parse_oauth_token_response(response: reqwest::Response) -> Result<TokenSet> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("OAuth token request failed ({status}): {body}");
    }
    let parsed: OAuthTokenResponse =
        serde_json::from_str(&body).context("Failed to parse OAuth token response")?;
    let expires_at = parsed.expires_in.and_then(|secs| {
        if secs > 0 {
            Some(Utc::now() + chrono::Duration::seconds(secs))
        } else {
            None
        }
    });
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        expires_at,
        token_type: parsed.token_type.or(Some("Bearer".to_string())),
        scope: parsed.scope,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAuthFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default = "default_now_rfc3339")]
    updated_at: String,
    #[serde(default)]
    providers: BTreeMap<String, PersistedProviderAuthRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedProviderAuthRecord {
    provider: String,
    kind: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default = "default_now_rfc3339")]
    created_at: String,
    #[serde(default = "default_now_rfc3339")]
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRuntimeProfiles {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default = "default_now_rfc3339")]
    updated_at: String,
    #[serde(default)]
    active_profiles: BTreeMap<String, String>,
    #[serde(default)]
    profiles: BTreeMap<String, PersistedRuntimeProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedRuntimeProfile {
    provider: String,
    profile_name: String,
    kind: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default = "default_now_rfc3339")]
    created_at: String,
    #[serde(default = "default_now_rfc3339")]
    updated_at: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: CodexAuthTokens,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiOauthCreds {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    expiry_date: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Clone)]
struct SecretStore {
    key_path: PathBuf,
    enabled: bool,
}

impl SecretStore {
    fn new(root: &Path, enabled: bool) -> Self {
        Self {
            key_path: root.join(".secret_key"),
            enabled,
        }
    }

    fn encrypt(&self, plaintext: &str) -> Result<String> {
        if !self.enabled || plaintext.is_empty() {
            return Ok(plaintext.to_string());
        }

        let key_bytes = self.load_or_create_key()?;
        let key = Key::from_slice(&key_bytes);
        let cipher = ChaCha20Poly1305::new(key);
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(format!("enc2:{}", hex_encode(&blob)))
    }

    fn decrypt(&self, value: &str) -> Result<String> {
        let Some(hex_str) = value.strip_prefix("enc2:") else {
            return Ok(value.to_string());
        };

        let blob = hex_decode(hex_str)?;
        anyhow::ensure!(
            blob.len() > NONCE_LEN,
            "Encrypted value too short (missing nonce)"
        );

        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let key_bytes = self.load_or_create_key()?;
        let key = Key::from_slice(&key_bytes);
        let cipher = ChaCha20Poly1305::new(key);
        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| anyhow::anyhow!("Decryption failed"))?;
        String::from_utf8(plaintext).context("Decrypted secret is not valid UTF-8")
    }

    fn load_or_create_key(&self) -> Result<Vec<u8>> {
        if self.key_path.exists() {
            let key = fs::read_to_string(&self.key_path)
                .with_context(|| format!("Failed to read {}", self.key_path.display()))?;
            return hex_decode(key.trim());
        }

        let key = ChaCha20Poly1305::generate_key(&mut OsRng).to_vec();
        if let Some(parent) = self.key_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::write(&self.key_path, hex_encode(&key))
            .with_context(|| format!("Failed to write {}", self.key_path.display()))?;
        set_owner_only_permissions(&self.key_path)?;
        Ok(key)
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let json = serde_json::to_vec_pretty(value).context("Failed to serialize JSON")?;
    let tmp_path = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::write(&tmp_path, json)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to replace {} with {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

fn encrypt_optional(secrets: &SecretStore, value: Option<&str>) -> Result<Option<String>> {
    match value {
        Some(value) if !value.is_empty() => secrets.encrypt(value).map(Some),
        _ => Ok(None),
    }
}

fn decrypt_optional(secrets: &SecretStore, value: Option<&str>) -> Result<Option<String>> {
    match value {
        Some(value) if !value.is_empty() => secrets.decrypt(value).map(Some),
        _ => Ok(None),
    }
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

fn default_now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn runtime_profile_id(provider: &str) -> String {
    format!("{provider}:{DEFAULT_PROFILE_NAME}")
}

fn parse_optional_datetime(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value.map(parse_datetime).transpose()
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("Invalid RFC3339 timestamp: {value}"))
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    decode_jwt_claims(token)
        .and_then(|claims| claims.account_id.or(claims.sub))
        .filter(|value| !value.trim().is_empty())
}

fn extract_expiry_from_jwt(token: &str) -> Option<DateTime<Utc>> {
    let exp = decode_jwt_claims(token)?.exp?;
    DateTime::from_timestamp(exp, 0)
}

fn extract_account_email_from_id_token(token: &str) -> Option<String> {
    decode_jwt_claims(token)?
        .email
        .filter(|value| !value.trim().is_empty())
}

fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn hex_encode(data: &[u8]) -> String {
    let mut output = String::with_capacity(data.len() * 2);
    for byte in data {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(hex.len().is_multiple_of(2), "Hex string has odd length");
    (0..hex.len())
        .step_by(2)
        .map(|idx| {
            u8::from_str_radix(&hex[idx..idx + 2], 16)
                .map_err(|e| anyhow::anyhow!("Invalid hex at position {idx}: {e}"))
        })
        .collect()
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sync_to_runtime_store_writes_hidden_auth_profiles_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = MemexAuthStore::new(dir.path());

        store
            .upsert_oauth(
                "openai-codex",
                TokenSet {
                    access_token: "access".into(),
                    refresh_token: Some("refresh".into()),
                    id_token: None,
                    expires_at: None,
                    token_type: Some("Bearer".into()),
                    scope: None,
                },
                Some("acct_123".into()),
            )
            .await
            .unwrap();

        store.sync_to_runtime_store().await.unwrap();

        let auth_json = dir.path().join("auth.json");
        let runtime_json = dir.path().join(".zeroclaw").join("auth-profiles.json");
        assert!(auth_json.exists());
        assert!(dir.path().join(".secret_key").exists());
        assert!(runtime_json.exists());
        assert!(dir.path().join(".zeroclaw").join(".secret_key").exists());

        let raw = fs::read_to_string(runtime_json).unwrap();
        assert!(raw.contains("\"openai-codex:default\""));
        assert!(raw.contains("enc2:"));
        assert!(!raw.contains("\"access\""));
        assert!(!raw.contains("\"refresh\""));
    }

    #[tokio::test]
    async fn import_gemini_cli_cache_creates_gemini_record() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path().join("oauth_creds.json");
        fs::write(
            &cache,
            r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expiry_date":4102444800000}"#,
        )
        .unwrap();

        let store = MemexAuthStore::new(dir.path());
        import_gemini_cli_auth(&store, &cache).await.unwrap();

        let snapshot = store.load().await.unwrap();
        let gemini = snapshot.providers.get("gemini").unwrap();
        let token_set = gemini.token_set.as_ref().unwrap();
        assert_eq!(token_set.access_token, "a");
        assert_eq!(token_set.refresh_token.as_deref(), Some("r"));
        assert_eq!(token_set.token_type.as_deref(), Some("Bearer"));
        assert!(token_set.expires_at.is_some());
    }

    #[tokio::test]
    async fn memex_auth_round_trip_encrypts_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = MemexAuthStore::new(dir.path());

        store
            .upsert_oauth(
                "gemini",
                TokenSet {
                    access_token: "access-token".into(),
                    refresh_token: Some("refresh-token".into()),
                    id_token: Some("id-token".into()),
                    expires_at: None,
                    token_type: Some("Bearer".into()),
                    scope: Some("openid".into()),
                },
                Some("user@example.com".into()),
            )
            .await
            .unwrap();

        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(raw.contains("enc2:"));
        assert!(!raw.contains("access-token"));
        assert!(!raw.contains("refresh-token"));

        let loaded = store.load().await.unwrap();
        let gemini = loaded.providers.get("gemini").unwrap();
        let token_set = gemini.token_set.as_ref().unwrap();
        assert_eq!(token_set.access_token, "access-token");
        assert_eq!(token_set.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(gemini.account_id.as_deref(), Some("user@example.com"));
    }
}
