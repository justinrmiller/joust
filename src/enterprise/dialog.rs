//! State of the "Connect to LanceDB Enterprise" dialog.

use std::path::Path;

use anyhow::{Result, bail};

use super::profiles::{self, AuthMethod, CredentialInputs, Profile, Profiles, ResolvedCredential};
use super::{Check, EnterpriseTarget, format_namespace, parse_namespace};

/// A text field in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectField {
    RestUrl,
    SqlUrl,
    Database,
    Namespace,
    ApiKey,
    TokenFile,
}

/// The Open dialog's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenTab {
    /// A LanceDB directory on this machine.
    #[default]
    Local,
    /// A LanceDB Enterprise server.
    Enterprise,
}

/// What the Open dialog shows and edits: the tab, and the Enterprise
/// connection form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectDialog {
    pub tab: OpenTab,
    pub profiles: Profiles,
    /// Why the CLI config could not be read (only `local` is offered then).
    pub config_error: Option<String>,
    pub selected: Option<String>,
    pub rest_url: String,
    pub sql_url: String,
    pub database: String,
    pub namespace: String,
    pub auth: AuthMethod,
    /// Typed for this session only; never saved.
    pub api_key: String,
    pub token_file: String,
    /// The last connection check.
    pub checks: Option<Vec<Check>>,
    pub error: Option<String>,
    /// "Testing…" or "Connecting…" while the server is being probed.
    pub working: Option<&'static str>,
    tls: [Option<String>; 3],
}

impl ConnectDialog {
    /// Opens on the CLI's default profile. `config` is the CLI config file;
    /// `None` offers only the built-in `local` profile.
    pub fn new(config: Option<&Path>) -> Self {
        let (profiles, config_error) = match config.map(Profiles::load) {
            Some(Ok(profiles)) => (profiles, None),
            Some(Err(error)) => (Profiles::builtin(), Some(format!("{error:#}"))),
            None => (Profiles::builtin(), None),
        };
        let default = profiles.default.clone();
        let mut dialog = Self {
            profiles,
            config_error,
            ..Self::default()
        };
        dialog.select(&default);
        dialog
    }

    /// Opens on `target` (a recent connection), showing `error`: why it
    /// could not connect straight away.
    pub fn for_target(config: Option<&Path>, target: &EnterpriseTarget, error: String) -> Self {
        let mut dialog = Self::new(config);
        dialog.selected = target
            .profile
            .clone()
            .filter(|name| dialog.profiles.get(name).is_some());
        dialog.rest_url = target.rest_url.clone();
        dialog.sql_url = target.sql_url.clone();
        dialog.database = target.database.clone();
        dialog.namespace = format_namespace(&target.namespace);
        dialog.auth = target.auth;
        dialog.token_file = target.token_file.clone();
        dialog.tls = [
            target.ca_cert.clone(),
            target.client_cert.clone(),
            target.client_key.clone(),
        ];
        dialog.error = Some(error);
        dialog.tab = OpenTab::Enterprise;
        dialog
    }

    /// Fills the fields from profile `name`.
    pub fn select(&mut self, name: &str) {
        let Some(profile) = self.profiles.get(name).cloned() else {
            return;
        };
        self.selected = Some(name.to_string());
        self.rest_url = profile.http_server_url.unwrap_or_default();
        self.sql_url = profile.sql_server_url.unwrap_or_default();
        self.database = profile.database.unwrap_or_else(default_database);
        self.namespace.clear();
        self.auth = AuthMethod::Automatic;
        self.tls = [profile.ca_cert, profile.client_cert, profile.client_key];
        self.checks = None;
        self.error = None;
    }

    pub fn edit(&mut self, field: ConnectField, value: String) {
        let slot = match field {
            ConnectField::RestUrl => &mut self.rest_url,
            ConnectField::SqlUrl => &mut self.sql_url,
            ConnectField::Database => &mut self.database,
            ConnectField::Namespace => &mut self.namespace,
            ConnectField::ApiKey => &mut self.api_key,
            ConnectField::TokenFile => &mut self.token_file,
        };
        *slot = value;
        self.checks = None;
        self.error = None;
    }

    pub fn set_auth(&mut self, method: AuthMethod) {
        self.auth = method;
        self.checks = None;
        self.error = None;
    }

    pub fn profile(&self) -> Option<&Profile> {
        self.selected
            .as_deref()
            .and_then(|name| self.profiles.get(name))
    }

    /// Whether the selected profile can sign in through OIDC.
    pub fn has_profile_sign_in(&self) -> bool {
        self.profile().is_some_and(|profile| profile.oidc.is_some())
    }

    /// The target the fields describe, checked.
    pub fn target(&self) -> Result<EnterpriseTarget> {
        let [ca_cert, client_cert, client_key] = self.tls.clone();
        let target = EnterpriseTarget {
            profile: self.selected.clone(),
            rest_url: self.rest_url.trim().to_string(),
            sql_url: self.sql_url.trim().to_string(),
            database: self.database.trim().to_string(),
            namespace: parse_namespace(&self.namespace),
            auth: self.auth,
            token_file: self.token_file.trim().to_string(),
            ca_cert,
            client_cert,
            client_key,
        };
        target.endpoints()?;
        if target.database.is_empty() {
            bail!("enter a database name");
        }
        Ok(target)
    }

    /// The target and the credential to connect with.
    pub fn resolve(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(EnterpriseTarget, ResolvedCredential)> {
        let target = self.target()?;
        let credential = profiles::resolve_credential(
            &CredentialInputs {
                method: self.auth,
                profile_name: self.selected.as_deref(),
                profile: self.profile(),
                api_key: &self.api_key,
                token_file: &self.token_file,
            },
            env,
        )?;
        Ok((target, credential))
    }
}

/// The credential to reconnect to `target` without the dialog (a recent
/// connection, or another database on the same server). `api_key` is a key
/// typed earlier this session.
pub fn reconnect_credential(
    target: &EnterpriseTarget,
    profiles: &Profiles,
    api_key: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedCredential> {
    let profile = target
        .profile
        .as_deref()
        .and_then(|name| profiles.get(name));
    profiles::resolve_credential(
        &CredentialInputs {
            method: target.auth,
            profile_name: target.profile.as_deref(),
            profile,
            api_key: api_key.unwrap_or_default(),
            token_file: &target.token_file,
        },
        env,
    )
}

/// The CLI's default database: the user's name.
fn default_database() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enterprise::profiles::{API_KEY_ENV, Credential};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn config() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            default_profile = "prod"
            [profiles.prod]
            http_server_url = "https://query.example.com"
            database = "analytics"
            api_key = "sk_prod"
            ca_cert = "/ca.pem"
            [profiles.sso.oidc]
            flow = "device_code"
            issuer_url = "https://idp.example.com"
            client_id = "joust"
            scopes = ["openid"]
            "#,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn opens_on_the_default_profile() {
        let (_dir, path) = config();
        let dialog = ConnectDialog::new(Some(&path));
        assert_eq!(dialog.selected.as_deref(), Some("prod"));
        assert_eq!(dialog.rest_url, "https://query.example.com");
        assert_eq!(dialog.database, "analytics");
        assert!(!dialog.has_profile_sign_in());
        assert!(dialog.config_error.is_none());

        let target = dialog.target().unwrap();
        assert_eq!(target.ca_cert.as_deref(), Some("/ca.pem"));
        let (_, credential) = dialog.resolve(no_env).unwrap();
        assert_eq!(credential.credential, Credential::ApiKey("sk_prod".into()));

        let builtin = ConnectDialog::new(None);
        assert_eq!(builtin.selected.as_deref(), Some("local"));
        assert_eq!(builtin.rest_url, "localhost:10024");
        assert!(!builtin.database.is_empty(), "defaults to the user's name");
    }

    #[test]
    fn a_broken_config_still_offers_local() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        let dialog = ConnectDialog::new(Some(&path));
        assert!(dialog.config_error.is_some());
        assert_eq!(dialog.selected.as_deref(), Some("local"));
    }

    #[test]
    fn editing_clears_results_and_validates() {
        let (_dir, path) = config();
        let mut dialog = ConnectDialog::new(Some(&path));
        dialog.checks = Some(vec![Check::ok("REST API", "ok")]);
        dialog.error = Some("old".into());
        dialog.edit(ConnectField::Namespace, "team$reports".into());
        assert_eq!(dialog.checks, None);
        assert_eq!(dialog.error, None);
        assert_eq!(dialog.target().unwrap().namespace, ["team", "reports"]);

        for field in [ConnectField::SqlUrl, ConnectField::TokenFile] {
            dialog.edit(field, "x".into());
        }
        assert_eq!(
            (dialog.sql_url.as_str(), dialog.token_file.as_str()),
            ("x", "x")
        );
        dialog.edit(ConnectField::SqlUrl, String::new());

        dialog.edit(ConnectField::Database, " ".into());
        assert!(
            dialog
                .target()
                .unwrap_err()
                .to_string()
                .contains("database")
        );
        dialog.edit(ConnectField::Database, "d".into());
        dialog.edit(ConnectField::RestUrl, "ftp://x".into());
        assert!(dialog.target().is_err());
        dialog.edit(ConnectField::RestUrl, "localhost".into());

        dialog.set_auth(AuthMethod::ApiKey);
        assert!(dialog.resolve(no_env).is_err(), "no key typed yet");
        dialog.edit(ConnectField::ApiKey, "sk_typed".into());
        let (_, credential) = dialog.resolve(no_env).unwrap();
        assert_eq!(credential.credential, Credential::ApiKey("sk_typed".into()));

        dialog.select("missing");
        assert_eq!(
            dialog.selected.as_deref(),
            Some("prod"),
            "unknown names are ignored"
        );
        dialog.select("sso");
        assert!(dialog.has_profile_sign_in());
        assert_eq!(dialog.auth, AuthMethod::Automatic);
        assert!(dialog.target().is_err(), "the sso profile has no address");
    }

    #[test]
    fn recent_targets_reopen_and_reconnect() {
        let (_dir, path) = config();
        let profiles = Profiles::load(&path).unwrap();
        let target = EnterpriseTarget {
            profile: Some("prod".into()),
            rest_url: "https://query.example.com".into(),
            database: "other".into(),
            namespace: vec!["team".into()],
            auth: AuthMethod::ApiKey,
            ..EnterpriseTarget::default()
        };
        let dialog = ConnectDialog::for_target(Some(&path), &target, "enter an API key".into());
        assert_eq!(dialog.database, "other");
        assert_eq!(dialog.namespace, "team");
        assert_eq!(dialog.auth, AuthMethod::ApiKey);
        assert_eq!(dialog.error.as_deref(), Some("enter an API key"));
        assert_eq!(dialog.target().unwrap(), target);

        assert!(reconnect_credential(&target, &profiles, None, no_env).is_err());
        let credential =
            reconnect_credential(&target, &profiles, Some("sk_session"), no_env).unwrap();
        assert_eq!(
            credential.credential,
            Credential::ApiKey("sk_session".into())
        );

        let automatic = EnterpriseTarget {
            auth: AuthMethod::Automatic,
            ..target
        };
        let env = |name: &str| (name == API_KEY_ENV).then(|| "sk_env".to_string());
        let credential = reconnect_credential(&automatic, &profiles, None, env).unwrap();
        assert_eq!(credential.credential, Credential::ApiKey("sk_env".into()));
        let credential = reconnect_credential(&automatic, &profiles, None, no_env).unwrap();
        assert_eq!(credential.credential, Credential::ApiKey("sk_prod".into()));
    }
}
