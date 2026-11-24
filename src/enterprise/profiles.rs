//! Connection profiles shared with the `lancedb` CLI, and the endpoint and
//! credential rules it applies.
//!
//! The CLI keeps profiles in `~/.lancedb/config.toml`:
//!
//! ```toml
//! default_profile = "prod"
//! [profiles.prod]
//! http_server_url = "https://query.example.com"
//! sql_server_url = "grpcs://sql.example.com:10026"
//! database = "analytics"
//! api_key = "…"   # or oidc_access_token_file = "…", or a [profiles.prod.oidc] table
//! ```
//!
//! joust only reads that file (the CLI rewrites it whole), ignores keys it
//! does not know, and resolves addresses and credentials the way the CLI does,
//! so a profile behaves the same in both.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The profile the CLI builds in for a `lancedb server` on this machine.
pub const LOCAL_PROFILE: &str = "local";
/// The key a local `lancedb server` accepts (it runs without auth).
pub const LOCAL_API_KEY: &str = "sk_localtest";
pub const API_KEY_ENV: &str = "LANCEDB_API_KEY";
pub const OIDC_TOKEN_ENV: &str = "LANCEDB_OIDC_ACCESS_TOKEN";
pub const OIDC_TOKEN_FILE_ENV: &str = "LANCEDB_OIDC_ACCESS_TOKEN_FILE";
/// Where an OIDC client secret comes from when the profile names no file.
pub const OIDC_CLIENT_SECRET_ENV: &str = "LANCEDB_OIDC_CLIENT_SECRET";
const DEFAULT_REST_PORT: u16 = 10024;
const DEFAULT_SQL_PORT: u16 = 10025;

/// `~/.lancedb/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    lancedb_dir().map(|dir| dir.join("config.toml"))
}

/// `~/.lancedb`, where the CLI keeps its config and sign-in sessions.
pub fn lancedb_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".lancedb"))
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    default_profile: Option<String>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

/// One `[profiles.NAME]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Profile {
    pub http_server_url: Option<String>,
    pub sql_server_url: Option<String>,
    pub database: Option<String>,
    pub api_key: Option<String>,
    pub oidc_access_token_file: Option<String>,
    pub oidc: Option<OidcSettings>,
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

impl Profile {
    /// The CLI's built-in `local` profile.
    pub fn local() -> Self {
        Self {
            http_server_url: Some(format!("localhost:{DEFAULT_REST_PORT}")),
            sql_server_url: Some(format!("localhost:{DEFAULT_SQL_PORT}")),
            ..Self::default()
        }
    }
}

/// A profile's `[oidc]` table: sign-in through an identity provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OidcSettings {
    pub flow: OidcFlow,
    pub issuer_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub client_secret_file: Option<PathBuf>,
    pub client_auth_method: Option<ClientAuth>,
    pub resource: Option<String>,
    pub audience: Option<String>,
    pub redirect_uri: Option<String>,
    pub callback_port: Option<u16>,
    pub refresh_buffer_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OidcFlow {
    ClientCredentials,
    AuthorizationCode,
    DeviceCode,
}

impl fmt::Display for OidcFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClientCredentials => "service account",
            Self::AuthorizationCode => "browser sign-in",
            Self::DeviceCode => "device code",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuth {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// The profiles to offer, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    /// The profile to preselect: the file's `default_profile`, else `local`.
    pub default: String,
    /// Sorted by name; always has `local`.
    pub entries: BTreeMap<String, Profile>,
}

impl Profiles {
    /// Only the built-in `local` profile.
    pub fn builtin() -> Self {
        Self {
            default: LOCAL_PROFILE.to_string(),
            entries: BTreeMap::from([(LOCAL_PROFILE.to_string(), Profile::local())]),
        }
    }

    /// Reads the CLI config at `path`. A missing file gives the built-in
    /// profile; an unreadable or invalid one is an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("invalid {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::builtin()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Parses the text of a CLI config file.
    pub fn parse(text: &str) -> Result<Self> {
        let file: ConfigFile = toml::from_str(text)?;
        let mut entries = file.profiles;
        entries
            .entry(LOCAL_PROFILE.to_string())
            .or_insert_with(Profile::local);
        let default = file
            .default_profile
            .filter(|name| entries.contains_key(name))
            .unwrap_or_else(|| LOCAL_PROFILE.to_string());
        Ok(Self { default, entries })
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.entries.get(name)
    }
}

impl Default for Profiles {
    fn default() -> Self {
        Self::builtin()
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// A server address split the way the CLI reads one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
}

impl Endpoint {
    /// Parses `address`. Without a scheme it is plaintext on `default_port`
    /// (as in the CLI); `https://` (or `grpcs://`) asks for TLS, and then a
    /// missing port is the scheme's own (443 or 80).
    fn parse(address: &str, default_port: u16, grpc: bool) -> Result<Self> {
        let address = address.trim();
        let (tls, rest, explicit) = match address.split_once("://") {
            Some((scheme, rest)) => {
                let tls = match scheme.to_ascii_lowercase().as_str() {
                    "http" => false,
                    "https" => true,
                    "grpc" if grpc => false,
                    "grpcs" if grpc => true,
                    other => bail!(
                        "unsupported scheme `{other}` in `{address}` (use {})",
                        if grpc {
                            "grpc://, grpcs://, http:// or https://"
                        } else {
                            "http:// or https://"
                        }
                    ),
                };
                (tls, rest, true)
            }
            None => (false, address, false),
        };
        // Only the authority is dialled; drop any path, query or fragment.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.is_empty() {
            bail!("`{address}` has no host");
        }
        let (host, port) =
            split_host_port(authority).with_context(|| format!("invalid address `{address}`"))?;
        let port = port.unwrap_or(match (explicit, tls) {
            (true, true) => 443,
            (true, false) => 80,
            (false, _) => default_port,
        });
        Ok(Self { tls, host, port })
    }

    fn host_port(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Splits `host[:port]`, accepting bracketed (`[::1]:10024`) and bare IPv6.
fn split_host_port(authority: &str) -> Result<(String, Option<u16>)> {
    let parse_port = |port: &str| {
        port.parse::<u16>()
            .with_context(|| format!("invalid port `{port}`"))
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').context("unclosed `[`")?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if after.is_empty() => None,
            None => bail!("unexpected `{after}` after `]`"),
        };
        return Ok((host.to_string(), port));
    }
    match authority.split_once(':') {
        Some((host, port)) if !port.contains(':') => {
            if host.is_empty() {
                bail!("missing host");
            }
            Ok((host.to_string(), Some(parse_port(port)?)))
        }
        // No colon, or a bare IPv6 literal (several colons, no port).
        _ => Ok((authority.to_string(), None)),
    }
}

/// Normalises a REST address to `http[s]://host:port`.
pub fn rest_url(address: &str) -> Result<String> {
    let endpoint = Endpoint::parse(address, DEFAULT_REST_PORT, false)?;
    let scheme = if endpoint.tls { "https" } else { "http" };
    Ok(format!("{scheme}://{}", endpoint.host_port()))
}

/// Normalises a Flight SQL address to `grpc[s]://host:port`.
pub fn sql_url(address: &str) -> Result<String> {
    let endpoint = Endpoint::parse(address, DEFAULT_SQL_PORT, true)?;
    Ok(grpc_url(&endpoint))
}

/// The SQL endpoint a REST address implies: same host and TLS, and the REST
/// port + 1 (the server's convention: 10024 → 10025).
pub fn derived_sql_url(rest: &str) -> Result<String> {
    let mut endpoint = Endpoint::parse(rest, DEFAULT_REST_PORT, false)?;
    endpoint.port = endpoint
        .port
        .checked_add(1)
        .context("the REST port has no next port for SQL")?;
    Ok(grpc_url(&endpoint))
}

fn grpc_url(endpoint: &Endpoint) -> String {
    let scheme = if endpoint.tls { "grpcs" } else { "grpc" };
    format!("{scheme}://{}", endpoint.host_port())
}

/// The host of `address`, for labels (the address itself if it won't parse).
pub fn host_of(address: &str) -> String {
    Endpoint::parse(address, DEFAULT_REST_PORT, true)
        .map(|endpoint| endpoint.host)
        .unwrap_or_else(|_| address.trim().to_string())
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// How to sign in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// The CLI's order: `LANCEDB_API_KEY`, `LANCEDB_OIDC_ACCESS_TOKEN`,
    /// `LANCEDB_OIDC_ACCESS_TOKEN_FILE`, the profile, then the local key.
    #[default]
    Automatic,
    /// An API key typed for this session (never saved).
    ApiKey,
    /// An OIDC access token read from a file.
    TokenFile,
    /// The profile's `[oidc]` sign-in.
    ProfileSignIn,
}

impl AuthMethod {
    pub const ALL: [Self; 4] = [
        Self::Automatic,
        Self::ApiKey,
        Self::TokenFile,
        Self::ProfileSignIn,
    ];
}

impl fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Automatic => "Environment or profile",
            Self::ApiKey => "API key",
            Self::TokenFile => "OIDC access-token file",
            Self::ProfileSignIn => "Profile sign-in (OIDC)",
        })
    }
}

/// A resolved secret. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    OidcToken(String),
    /// Sign in through the profile's identity provider.
    OAuth {
        profile: String,
        settings: OidcSettings,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(…)"),
            Self::OidcToken(_) => f.write_str("OidcToken(…)"),
            Self::OAuth { profile, .. } => write!(f, "OAuth({profile})"),
        }
    }
}

/// A credential and where it came from (shown to the user; never the secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    pub credential: Credential,
    pub source: String,
}

impl ResolvedCredential {
    fn new(credential: Credential, source: impl Into<String>) -> Self {
        Self {
            credential,
            source: source.into(),
        }
    }
}

/// What a credential is resolved from.
#[derive(Debug, Clone, Copy)]
pub struct CredentialInputs<'a> {
    pub method: AuthMethod,
    pub profile_name: Option<&'a str>,
    pub profile: Option<&'a Profile>,
    /// A key typed into the dialog.
    pub api_key: &'a str,
    /// A token file named in the dialog.
    pub token_file: &'a str,
}

/// Resolves the credential to connect with. `env` looks up environment
/// variables (tests pass a fake).
pub fn resolve_credential(
    inputs: &CredentialInputs<'_>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedCredential> {
    let set = |name: &str| env(name).filter(|value| !value.trim().is_empty());
    let profile_name = inputs.profile_name.unwrap_or("this");
    match inputs.method {
        AuthMethod::ApiKey => {
            let key = inputs.api_key.trim();
            if key.is_empty() {
                bail!("enter an API key");
            }
            Ok(ResolvedCredential::new(
                Credential::ApiKey(key.to_string()),
                "the API key you entered",
            ))
        }
        AuthMethod::TokenFile => {
            if inputs.token_file.trim().is_empty() {
                bail!("enter the path of a file holding an OIDC access token");
            }
            Ok(ResolvedCredential::new(
                Credential::OidcToken(read_token_file(inputs.token_file)?),
                format!("the OIDC token in {}", inputs.token_file.trim()),
            ))
        }
        AuthMethod::ProfileSignIn => {
            let settings = inputs
                .profile
                .and_then(|profile| profile.oidc.clone())
                .with_context(|| format!("the {profile_name} profile has no [oidc] sign-in"))?;
            let source = format!("{profile_name} profile sign-in ({})", settings.flow);
            Ok(ResolvedCredential::new(
                Credential::OAuth {
                    profile: profile_name.to_string(),
                    settings,
                },
                source,
            ))
        }
        AuthMethod::Automatic => {
            if let Some(key) = set(API_KEY_ENV) {
                return Ok(ResolvedCredential::new(
                    Credential::ApiKey(key.trim().to_string()),
                    format!("the API key in ${API_KEY_ENV}"),
                ));
            }
            if let Some(token) = set(OIDC_TOKEN_ENV) {
                return Ok(ResolvedCredential::new(
                    Credential::OidcToken(token.trim().to_string()),
                    format!("the OIDC token in ${OIDC_TOKEN_ENV}"),
                ));
            }
            if let Some(path) = set(OIDC_TOKEN_FILE_ENV) {
                return Ok(ResolvedCredential::new(
                    Credential::OidcToken(read_token_file(&path)?),
                    format!("the OIDC token file in ${OIDC_TOKEN_FILE_ENV}"),
                ));
            }
            let profile = inputs.profile.cloned().unwrap_or_default();
            if let Some(key) = profile.api_key.filter(|key| !key.trim().is_empty()) {
                return Ok(ResolvedCredential::new(
                    Credential::ApiKey(key.trim().to_string()),
                    format!("the {profile_name} profile's API key"),
                ));
            }
            if let Some(path) = profile.oidc_access_token_file {
                return Ok(ResolvedCredential::new(
                    Credential::OidcToken(read_token_file(&path)?),
                    format!("the {profile_name} profile's OIDC token file"),
                ));
            }
            if let Some(settings) = profile.oidc {
                let source = format!("{profile_name} profile sign-in ({})", settings.flow);
                return Ok(ResolvedCredential::new(
                    Credential::OAuth {
                        profile: profile_name.to_string(),
                        settings,
                    },
                    source,
                ));
            }
            Ok(ResolvedCredential::new(
                Credential::ApiKey(LOCAL_API_KEY.to_string()),
                format!("the local server key ({LOCAL_API_KEY})"),
            ))
        }
    }
}

/// Reads an access token from `path` (`~/` is the home directory).
fn read_token_file(path: &str) -> Result<String> {
    let path = expand_home(path.trim());
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read the token file {}", path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", path.display());
    }
    Ok(token.to_string())
}

/// `~/x` → `$HOME/x`; anything else unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        default_profile = "prod"
        data_dir = "/ignored/by/joust"

        [profiles.prod]
        http_server_url = "https://query.example.com"
        sql_server_url = "grpcs://sql.example.com:10026"
        database = "analytics"
        api_key = "sk_prod"
        ca_cert = "/certs/ca.pem"
        some_future_key = true

        [profiles.sso]
        http_server_url = "query.example.com:8080"

        [profiles.sso.oidc]
        flow = "authorization_code"
        issuer_url = "https://idp.example.com"
        client_id = "joust"
        scopes = ["openid", "offline_access"]
        callback_port = 8400
        device_code_pkce = false
    "#;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn inputs(method: AuthMethod, profile: Option<&Profile>) -> CredentialInputs<'_> {
        CredentialInputs {
            method,
            profile_name: Some("prod"),
            profile,
            api_key: "",
            token_file: "",
        }
    }

    #[test]
    fn parses_cli_profiles_and_adds_local() {
        let profiles = Profiles::parse(CONFIG).unwrap();
        assert_eq!(profiles.default, "prod");
        let names: Vec<_> = profiles.entries.keys().map(String::as_str).collect();
        assert_eq!(names, ["local", "prod", "sso"]);

        let prod = profiles.get("prod").unwrap();
        assert_eq!(prod.database.as_deref(), Some("analytics"));
        assert_eq!(prod.ca_cert.as_deref(), Some("/certs/ca.pem"));
        let oidc = profiles.get("sso").unwrap().oidc.as_ref().unwrap();
        assert_eq!(oidc.flow, OidcFlow::AuthorizationCode);
        assert_eq!(oidc.scopes, ["openid", "offline_access"]);
        assert_eq!(oidc.callback_port, Some(8400));
        assert_eq!(profiles.get("local"), Some(&Profile::local()));

        // An unknown default falls back to `local`; a file's own `local` wins.
        let profiles = Profiles::parse(
            "default_profile = \"gone\"\n[profiles.local]\nhttp_server_url = \"localhost:9000\"",
        )
        .unwrap();
        assert_eq!(profiles.default, "local");
        assert_eq!(
            profiles.get("local").unwrap().http_server_url.as_deref(),
            Some("localhost:9000")
        );
        assert!(Profiles::parse("profiles = 3").is_err());
    }

    #[test]
    fn loads_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Profiles::load(&path).unwrap(), Profiles::builtin());
        std::fs::write(&path, CONFIG).unwrap();
        assert_eq!(Profiles::load(&path).unwrap().default, "prod");
        std::fs::write(&path, "[profiles.x\n").unwrap();
        let error = Profiles::load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("invalid"), "{error:#}");
        assert!(
            Profiles::load(dir.path()).is_err(),
            "a directory is unreadable"
        );
        assert!(config_path().is_none_or(|p| p.ends_with(".lancedb/config.toml")));
    }

    #[test]
    fn normalises_addresses_like_the_cli() {
        assert_eq!(rest_url("localhost").unwrap(), "http://localhost:10024");
        assert_eq!(
            rest_url(" localhost:9000 ").unwrap(),
            "http://localhost:9000"
        );
        assert_eq!(
            rest_url("https://query.example.com/v1?x").unwrap(),
            "https://query.example.com:443"
        );
        assert_eq!(rest_url("http://h").unwrap(), "http://h:80");
        assert_eq!(rest_url("[::1]:10024").unwrap(), "http://[::1]:10024");
        assert_eq!(rest_url("::1").unwrap(), "http://[::1]:10024");
        assert_eq!(sql_url("localhost").unwrap(), "grpc://localhost:10025");
        assert_eq!(sql_url("grpcs://s:10026").unwrap(), "grpcs://s:10026");
        assert_eq!(sql_url("https://s").unwrap(), "grpcs://s:443");
        assert_eq!(
            derived_sql_url("localhost:10024").unwrap(),
            "grpc://localhost:10025"
        );
        assert_eq!(derived_sql_url("https://q:8443").unwrap(), "grpcs://q:8444");

        for bad in [
            "", "grpc://x", "ftp://x", "https://", "h:port", "[::1", "[::1]x", ":80",
        ] {
            assert!(rest_url(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(derived_sql_url("h:65535").is_err());
        assert_eq!(
            host_of("https://query.example.com:443"),
            "query.example.com"
        );
        assert_eq!(host_of("not a url://"), "not a url://");
    }

    #[test]
    fn automatic_credentials_follow_the_cli_order() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, " tok_file \n").unwrap();
        let token_path = token_path.to_str().unwrap().to_string();

        let profile = Profiles::parse(CONFIG)
            .unwrap()
            .get("prod")
            .unwrap()
            .clone();
        let all = |name: &str| match name {
            API_KEY_ENV => Some("sk_env".to_string()),
            OIDC_TOKEN_ENV => Some("tok_env".to_string()),
            OIDC_TOKEN_FILE_ENV => Some(token_path.clone()),
            _ => None,
        };
        let auto = inputs(AuthMethod::Automatic, Some(&profile));

        let resolved = resolve_credential(&auto, all).unwrap();
        assert_eq!(resolved.credential, Credential::ApiKey("sk_env".into()));
        assert!(resolved.source.contains(API_KEY_ENV));

        let without_key = |name: &str| (name != API_KEY_ENV).then(|| all(name)).flatten();
        let resolved = resolve_credential(&auto, without_key).unwrap();
        assert_eq!(resolved.credential, Credential::OidcToken("tok_env".into()));

        let only_file = |name: &str| (name == OIDC_TOKEN_FILE_ENV).then(|| all(name)).flatten();
        let resolved = resolve_credential(&auto, only_file).unwrap();
        assert_eq!(
            resolved.credential,
            Credential::OidcToken("tok_file".into())
        );

        let resolved = resolve_credential(&auto, no_env).unwrap();
        assert_eq!(resolved.credential, Credential::ApiKey("sk_prod".into()));
        assert!(resolved.source.contains("prod profile"));

        let blank = |_: &str| Some("  ".to_string());
        let file_profile = Profile {
            oidc_access_token_file: Some(token_path.clone()),
            ..Profile::default()
        };
        let resolved =
            resolve_credential(&inputs(AuthMethod::Automatic, Some(&file_profile)), blank).unwrap();
        assert_eq!(
            resolved.credential,
            Credential::OidcToken("tok_file".into())
        );

        let sso = Profiles::parse(CONFIG).unwrap().get("sso").unwrap().clone();
        let resolved =
            resolve_credential(&inputs(AuthMethod::Automatic, Some(&sso)), no_env).unwrap();
        assert!(matches!(resolved.credential, Credential::OAuth { .. }));
        assert!(resolved.source.contains("browser sign-in"));

        let resolved = resolve_credential(&inputs(AuthMethod::Automatic, None), no_env).unwrap();
        assert_eq!(
            resolved.credential,
            Credential::ApiKey(LOCAL_API_KEY.into())
        );
    }

    #[test]
    fn explicit_credential_methods() {
        let mut typed = inputs(AuthMethod::ApiKey, None);
        assert!(resolve_credential(&typed, no_env).is_err());
        typed.api_key = " sk_typed ";
        let resolved = resolve_credential(&typed, no_env).unwrap();
        assert_eq!(resolved.credential, Credential::ApiKey("sk_typed".into()));

        let mut file = inputs(AuthMethod::TokenFile, None);
        assert!(resolve_credential(&file, no_env).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        std::fs::write(&path, "\n").unwrap();
        let path = path.to_str().unwrap().to_string();
        file.token_file = &path;
        let error = resolve_credential(&file, no_env).unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
        std::fs::write(&path, "tok").unwrap();
        assert_eq!(
            resolve_credential(&file, no_env).unwrap().credential,
            Credential::OidcToken("tok".into())
        );
        file.token_file = "/no/such/token";
        assert!(resolve_credential(&file, no_env).is_err());

        let sso = Profiles::parse(CONFIG).unwrap().get("sso").unwrap().clone();
        assert!(resolve_credential(&inputs(AuthMethod::ProfileSignIn, None), no_env).is_err());
        let resolved =
            resolve_credential(&inputs(AuthMethod::ProfileSignIn, Some(&sso)), no_env).unwrap();
        assert_eq!(format!("{:?}", resolved.credential), "OAuth(prod)");
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        assert_eq!(
            format!("{:?}", Credential::ApiKey("sk_x".into())),
            "ApiKey(…)"
        );
        assert_eq!(
            format!("{:?}", Credential::OidcToken("t".into())),
            "OidcToken(…)"
        );
        assert_eq!(AuthMethod::ALL.len(), 4);
        for method in AuthMethod::ALL {
            assert!(!method.to_string().is_empty());
        }
        assert_eq!(OidcFlow::DeviceCode.to_string(), "device code");
        assert_eq!(OidcFlow::ClientCredentials.to_string(), "service account");
        assert!(expand_home("~/x").ends_with("x"));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
