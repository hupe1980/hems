//! Configuration: a file, then the environment, then the defaults.
//!
//! # Why the environment wins
//!
//! A gateway box is configured from a file an installer edited; a fleet service
//! is configured from an orchestrator that only knows how to set environment
//! variables. Both are true at once, so the order has to be decided rather than
//! discovered: **environment over file over default**, because the environment
//! is what an operator reaches for when something is on fire and the file is
//! what somebody wrote down last month.
//!
//! # A missing file is not an error
//!
//! A daemon started with no configuration at all should come up on its defaults
//! and say so. A daemon started with a configuration file it cannot *parse*
//! should refuse: the first is a deployment that has not been customised, the
//! second is one that has been customised wrongly, and treating them alike is
//! how a typo becomes a silent default.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use thiserror::Error;

/// Why a configuration could not be loaded.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists and could not be read.
    #[error("configuration file {path} could not be read: {source}")]
    Unreadable {
        /// Which file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file exists and is not valid TOML, or does not match the shape the
    /// daemon expects.
    #[error("configuration file {path} is not valid: {source}")]
    Invalid {
        /// Which file.
        path: PathBuf,
        /// What the parser said.
        source: toml::de::Error,
    },
    /// A configured secret says where to find itself and it is not there.
    #[error("the secret {reference:?} could not be resolved: {detail}")]
    UnresolvedSecret {
        /// What the configuration said.
        reference: String,
        /// Why it could not be resolved.
        detail: String,
    },
    /// An environment variable is set and cannot be read as the field wants.
    #[error("{variable} is set to {value:?}, which is not a {expected}")]
    BadEnvironment {
        /// The variable's name.
        variable: String,
        /// What it was set to.
        value: String,
        /// What was expected — `"socket address"`, `"number"`.
        expected: &'static str,
    },
}

/// The settings every daemon has, whatever else it adds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Where the health and readiness surface listens.
    ///
    /// The same socket serves the daemon's own API. Splitting them onto two
    /// ports is a decision an operator can make with a proxy and one this does
    /// not make for them.
    pub listen: SocketAddr,
    /// The `tracing` filter, in `EnvFilter` syntax.
    pub log_filter: String,
    /// Whether to emit logs as JSON rather than for a human.
    ///
    /// Off by default: a daemon somebody is watching start up for the first time
    /// is a daemon somebody is *reading*, and the fleet sets this from the
    /// environment along with everything else.
    pub log_json: bool,
    /// How long a shutdown may take before connections are dropped.
    ///
    /// The orchestrator's own grace period is the number to match. Longer and
    /// the process is killed mid-request anyway; shorter and it hangs up on
    /// somebody for no reason.
    pub shutdown_grace_s: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Loopback, not `0.0.0.0`. A daemon that binds every interface by
            // default is a daemon that is exposed by default, and the one on a
            // household gateway box is on the household's own network.
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            log_filter: "info".into(),
            log_json: false,
            shutdown_grace_s: 20,
        }
    }
}

impl Settings {
    /// The shutdown grace period.
    #[must_use]
    pub const fn shutdown_grace(&self) -> time::Duration {
        time::Duration::seconds(self.shutdown_grace_s.cast_signed())
    }
}

/// A credential, and where to get it from.
///
/// [`load_from`] reads a daemon's own fields from the file, which is right for a
/// poll interval and wrong for a credential: one in a configuration file is one
/// in an image, in a backup, and eventually in a repository — and no
/// orchestrator injects secrets that way. So the *reference* is configured and
/// the value is not:
///
/// ```text
/// webhook_secrets = ["env:HEMS_OBSD_WEBHOOK_SECRET"]   # from the environment
/// webhook_secrets = ["file:/run/secrets/webhook"]      # from a mounted file
/// webhook_secrets = ["whsec_literal"]                  # the secret itself
/// ```
///
/// A reference that cannot be resolved is an **error**, never a fallback to the
/// literal: a deployment signing with the string `file:/run/secrets/webhook`
/// looks exactly like one whose counterparty has started rejecting it.
///
/// [`Debug`] and [`serde::Serialize`] are redacted, because a daemon that logs
/// its settings at startup must not print what it was given. A *reference*
/// prints: the name of a variable is not a credential, and it is the first thing
/// anybody needs when a daemon will not start.
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// A secret given literally.
    #[must_use]
    pub fn literal(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Resolve it, reading the environment through `env` and files from disk.
    ///
    /// # Errors
    /// [`ConfigError::UnresolvedSecret`] if a reference names an environment
    /// variable that is not set, or a file that cannot be read.
    pub fn resolve(&self, env: impl Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
        if let Some(name) = self.0.strip_prefix("env:") {
            return env(name).ok_or_else(|| ConfigError::UnresolvedSecret {
                reference: self.0.clone(),
                detail: format!("{name} is not set"),
            });
        }
        if let Some(path) = self.0.strip_prefix("file:") {
            return std::fs::read_to_string(path)
                // A file written by `echo` ends in a newline, and a newline is
                // not part of anybody's secret.
                .map(|v| v.trim_end_matches(['\r', '\n']).to_owned())
                .map_err(|e| ConfigError::UnresolvedSecret {
                    reference: self.0.clone(),
                    detail: e.to_string(),
                });
        }
        Ok(self.0.clone())
    }

    /// Resolve it from the process environment.
    ///
    /// # Errors
    /// As [`Secret::resolve`].
    pub fn resolve_from_process(&self) -> Result<String, ConfigError> {
        self.resolve(|name| std::env::var(name).ok())
    }

    /// The bytes, without resolving anything.
    ///
    /// The deliberate escape hatch, named so that `grep expose` finds every
    /// place a credential leaves this type — which is the whole value of having
    /// the type at all. It returns the configured value verbatim, so calling it
    /// on an **unresolved** reference yields the literal string `env:…`, and a
    /// comparison against that would silently never match. Resolve first; this
    /// is for the comparison afterwards.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl core::fmt::Debug for Secret {
    /// Redacted — but it still says whether it is a *reference*, because
    /// "which variable did it want" is the first question when a daemon will not
    /// start, and the name of a variable is not a credential.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.starts_with("env:") || self.0.starts_with("file:") {
            write!(f, "Secret({})", self.0)
        } else {
            f.write_str("Secret(<redacted>)")
        }
    }
}

impl serde::Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Round-tripping a reference is safe and useful; round-tripping a
        // literal would write the credential back out.
        if self.0.starts_with("env:") || self.0.starts_with("file:") {
            serializer.serialize_str(&self.0)
        } else {
            serializer.serialize_str("<redacted>")
        }
    }
}

/// Load `T` from `path` if it exists, then let the environment override it.
///
/// `prefix` names the environment variables: `HEMS_TARIFFD_LISTEN` for a prefix
/// of `HEMS_TARIFFD`. Only the shared [`Settings`] are read from the
/// environment; a daemon's own fields come from the file, because a daemon with
/// forty settings has forty environment variables nobody documents.
///
/// # Errors
/// [`ConfigError`] when the file exists and cannot be read or parsed, or when an
/// environment variable is set to something the field cannot hold.
pub fn load<T>(path: Option<&Path>, prefix: &str) -> Result<T, ConfigError>
where
    T: DeserializeOwned + Default + AsMut<Settings>,
{
    load_from(path, prefix, |name| std::env::var(name).ok())
}

/// The same, reading the environment through `env` rather than from the process.
///
/// # Why this exists rather than a test that sets variables
///
/// Setting an environment variable is `unsafe` in this edition — the process
/// environment is global mutable state and another thread may be reading it —
/// and this crate forbids `unsafe`. That is a rule worth keeping rather than
/// working around: a test that mutates the process environment is a test that
/// cannot run beside another one, and the whole workspace's argument is that its
/// decisions are pure functions of their inputs. So the *decision* is one, and
/// reading the actual environment is the caller's single line.
///
/// # Errors
/// As [`load`].
pub fn load_from<T>(
    path: Option<&Path>,
    prefix: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<T, ConfigError>
where
    T: DeserializeOwned + Default + AsMut<Settings>,
{
    let mut loaded: T = match path {
        Some(p) if p.exists() => {
            let text = std::fs::read_to_string(p).map_err(|source| ConfigError::Unreadable {
                path: p.to_path_buf(),
                source,
            })?;
            toml::from_str(&text).map_err(|source| ConfigError::Invalid {
                path: p.to_path_buf(),
                source,
            })?
        }
        // No path, or a path that is not there: the defaults, and the daemon
        // says so in its first log line rather than pretending it was
        // configured.
        _ => T::default(),
    };
    apply_environment(loaded.as_mut(), prefix, &env)?;
    Ok(loaded)
}

/// Overlay the environment onto the shared settings.
fn apply_environment(
    settings: &mut Settings,
    prefix: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(), ConfigError> {
    let var = |suffix: &str| env(&format!("{prefix}_{suffix}"));

    if let Some(v) = var("LISTEN") {
        settings.listen = v.parse().map_err(|_| ConfigError::BadEnvironment {
            variable: format!("{prefix}_LISTEN"),
            value: v,
            expected: "socket address",
        })?;
    }
    if let Some(v) = var("LOG_FILTER") {
        settings.log_filter = v;
    }
    if let Some(v) = var("LOG_JSON") {
        settings.log_json = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    if let Some(v) = var("SHUTDOWN_GRACE_S") {
        settings.shutdown_grace_s = v.parse().map_err(|_| ConfigError::BadEnvironment {
            variable: format!("{prefix}_SHUTDOWN_GRACE_S"),
            value: v,
            expected: "number of seconds",
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_secret_is_itself() {
        assert_eq!(
            Secret::literal("whsec_abc").resolve(|_| None).unwrap(),
            "whsec_abc"
        );
    }

    #[test]
    fn an_environment_reference_is_read_from_the_environment() {
        let secret: Secret = toml::from_str::<Wrapper>("value = \"env:HEMS_TEST_SECRET\"")
            .unwrap()
            .value;
        assert_eq!(
            secret
                .resolve(|n| (n == "HEMS_TEST_SECRET").then(|| "from-the-env".to_owned()))
                .unwrap(),
            "from-the-env"
        );
    }

    #[test]
    fn a_reference_that_cannot_be_resolved_is_an_error_and_never_the_literal() {
        // The failure this refuses to have: a deployment that thinks it is
        // reading a mounted file and is in fact signing with the string
        // `file:/run/secrets/webhook`.
        let secret = Secret::literal("env:HEMS_TEST_ABSENT");
        assert!(matches!(
            secret.resolve(|_| None),
            Err(ConfigError::UnresolvedSecret { .. })
        ));
    }

    #[test]
    fn a_file_reference_is_read_and_its_trailing_newline_is_not_part_of_it() {
        // `echo secret > /run/secrets/x` is how one of these gets written, and
        // the newline it leaves is not anybody's credential.
        let path = std::env::temp_dir().join(format!("hems-secret-{}", std::process::id()));
        std::fs::write(&path, "whsec_from-a-file\n").unwrap();
        let secret = Secret::literal(format!("file:{}", path.display()));
        assert_eq!(secret.resolve(|_| None).unwrap(), "whsec_from-a-file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_secret_is_not_printed_and_a_reference_is() {
        // A daemon logs its settings at startup. The *name* of a variable is not
        // a credential and is the first thing anybody needs when it will not
        // start; the value is.
        assert_eq!(
            format!("{:?}", Secret::literal("whsec_abc")),
            "Secret(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", Secret::literal("env:HEMS_X")),
            "Secret(env:HEMS_X)"
        );
        // And it is redacted on the way *out* too, because a settings struct
        // that is serialised into a diagnostic bundle is the other way a
        // credential escapes.
        assert_eq!(
            serde_json::to_string(&Secret::literal("whsec_abc")).unwrap(),
            "\"<redacted>\""
        );
        assert_eq!(
            serde_json::to_string(&Secret::literal("env:HEMS_X")).unwrap(),
            "\"env:HEMS_X\""
        );
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Wrapper {
        value: Secret,
    }

    #[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    struct Example {
        #[serde(flatten)]
        service: Settings,
        upstream: String,
    }

    impl AsMut<Settings> for Example {
        fn as_mut(&mut self) -> &mut Settings {
            &mut self.service
        }
    }

    #[test]
    fn no_file_at_all_is_the_defaults_rather_than_an_error() {
        let loaded: Example = load(None, "HEMS_TEST_NONE").unwrap();
        assert_eq!(loaded.service, Settings::default());
    }

    #[test]
    fn a_path_that_is_not_there_is_also_the_defaults() {
        let loaded: Example = load(
            Some(Path::new("/nonexistent/hems.toml")),
            "HEMS_TEST_MISSING",
        )
        .unwrap();
        assert_eq!(loaded.service, Settings::default());
    }

    #[test]
    fn a_file_that_is_there_and_wrong_is_refused() {
        // The distinction the whole loader turns on: an *absent* file is a
        // deployment nobody customised, a *broken* one is a deployment somebody
        // customised wrongly, and defaulting past the second is how a typo
        // becomes a silent default.
        let dir = std::env::temp_dir().join("hems-service-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(&path, "listen = \"not an address\"\nupstream = \"x\"\n").unwrap();
        let err = load::<Example>(Some(&path), "HEMS_TEST_BROKEN").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_typo_in_a_key_is_refused_rather_than_ignored() {
        // `deny_unknown_fields`, and it is load bearing: a setting spelled
        // `log_fitler` that is silently dropped is a daemon logging at the
        // wrong level in production with a configuration file that looks right.
        let dir = std::env::temp_dir().join("hems-service-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("typo.toml");
        std::fs::write(&path, "log_fitler = \"debug\"\nupstream = \"x\"\n").unwrap();
        assert!(load::<Example>(Some(&path), "HEMS_TEST_TYPO").is_err());
        std::fs::remove_file(&path).ok();
    }

    /// An environment that is a list rather than the process's.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let dir = std::env::temp_dir().join("hems-service-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("good.toml");
        std::fs::write(
            &path,
            "listen = \"127.0.0.1:9001\"\nlog_filter = \"warn\"\nupstream = \"https://example\"\n",
        )
        .unwrap();

        let loaded: Example = load_from(
            Some(&path),
            "HEMS_TARIFFD",
            env(&[("HEMS_TARIFFD_LISTEN", "127.0.0.1:9999")]),
        )
        .unwrap();
        assert_eq!(loaded.service.listen.port(), 9999, "the environment wins");
        assert_eq!(
            loaded.service.log_filter, "warn",
            "and the file still holds"
        );
        assert_eq!(loaded.upstream, "https://example");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn another_daemons_variables_are_not_read() {
        // The prefix is the whole of what separates six daemons sharing one
        // environment, so a `HEMS_HISTD_LISTEN` must not move `tariffd`.
        let loaded: Example = load_from(
            None,
            "HEMS_TARIFFD",
            env(&[("HEMS_HISTD_LISTEN", "127.0.0.1:9999")]),
        )
        .unwrap();
        assert_eq!(loaded.service.listen, Settings::default().listen);
    }

    #[test]
    fn an_environment_variable_that_is_nonsense_is_an_error_rather_than_a_default() {
        let err = load_from::<Example>(
            None,
            "HEMS_TARIFFD",
            env(&[("HEMS_TARIFFD_SHUTDOWN_GRACE_S", "soon")]),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::BadEnvironment { .. }), "{err}");
    }

    #[test]
    fn the_default_listener_is_loopback() {
        // A daemon that binds every interface by default is exposed by default.
        assert!(Settings::default().listen.ip().is_loopback());
    }
}
