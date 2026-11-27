//! LanceDB Enterprise connections: profiles shared with the `lancedb` CLI,
//! credentials, and the connect dialog's state.

pub mod dashboard;
pub mod dialog;
pub mod profiles;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use profiles::AuthMethod;

/// Where to connect and how to sign in. Holds no secret, so it can be kept
/// in the recent-connections list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnterpriseTarget {
    /// The CLI profile it came from, if any.
    pub profile: Option<String>,
    pub rest_url: String,
    /// Empty: the REST port + 1.
    pub sql_url: String,
    pub database: String,
    /// Namespace for unqualified table names (empty: the root, `public`).
    pub namespace: Vec<String>,
    pub auth: AuthMethod,
    /// For [`AuthMethod::TokenFile`].
    pub token_file: String,
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

impl EnterpriseTarget {
    /// `prod · analytics`, or `query.example.com · analytics` without a profile.
    pub fn label(&self) -> String {
        let place = self
            .profile
            .clone()
            .unwrap_or_else(|| profiles::host_of(&self.rest_url));
        format!("{place} · {}", self.database)
    }

    /// The normalised REST and Flight SQL endpoints.
    pub fn endpoints(&self) -> Result<(String, String)> {
        if self.rest_url.trim().is_empty() {
            bail!("enter the REST endpoint");
        }
        let rest = profiles::rest_url(&self.rest_url)?;
        let sql = if self.sql_url.trim().is_empty() {
            profiles::derived_sql_url(&self.rest_url)?
        } else {
            profiles::sql_url(&self.sql_url)?
        };
        Ok((rest, sql))
    }

    /// The same target in another database.
    pub fn with_database(&self, database: &str) -> Self {
        Self {
            database: database.to_string(),
            ..self.clone()
        }
    }
}

/// `team$reports` (the server's notation); empty for the root namespace.
pub fn format_namespace(path: &[String]) -> String {
    path.join("$")
}

/// Parses `team$reports`; empty or `public` is the root namespace.
pub fn parse_namespace(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("public") {
        return Vec::new();
    }
    text.split('$')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// One line of a connection check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub label: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn ok(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            label,
            ok: true,
            detail: detail.into(),
        }
    }

    pub fn unavailable(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            label,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> EnterpriseTarget {
        EnterpriseTarget {
            profile: Some("prod".into()),
            rest_url: "https://query.example.com".into(),
            database: "analytics".into(),
            ..EnterpriseTarget::default()
        }
    }

    #[test]
    fn labels_and_endpoints() {
        let mut target = target();
        assert_eq!(target.label(), "prod · analytics");
        assert_eq!(
            target.endpoints().unwrap(),
            (
                "https://query.example.com:443".to_string(),
                "grpcs://query.example.com:444".to_string()
            )
        );
        target.profile = None;
        target.sql_url = "grpcs://sql.example.com:10026".into();
        assert_eq!(target.label(), "query.example.com · analytics");
        assert_eq!(
            target.endpoints().unwrap().1,
            "grpcs://sql.example.com:10026"
        );
        assert_eq!(target.with_database("other").database, "other");
        target.rest_url = " ".into();
        assert!(target.endpoints().is_err());
    }

    #[test]
    fn namespaces_use_the_servers_notation() {
        assert!(parse_namespace("").is_empty());
        assert!(parse_namespace(" Public ").is_empty());
        assert_eq!(parse_namespace("team$reports"), ["team", "reports"]);
        assert_eq!(parse_namespace("$a$$b$"), ["a", "b"]);
        assert_eq!(
            format_namespace(&parse_namespace("team$reports")),
            "team$reports"
        );
    }

    #[test]
    fn targets_serialise_without_secrets() {
        let json = serde_json::to_string(&target()).unwrap();
        assert!(!json.contains("api_key"));
        let back: EnterpriseTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, target());
        let partial: EnterpriseTarget = serde_json::from_str(r#"{"database":"d"}"#).unwrap();
        assert_eq!(partial.auth, AuthMethod::Automatic);
        assert_eq!(Check::ok("A", "b").ok, !Check::unavailable("A", "b").ok);
    }
}
