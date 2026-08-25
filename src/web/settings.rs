//! Where the web server's bind address and credentials come from.
//!
//! Three sources, highest first: the command line, the environment, then `[web]` in
//! `~/.config/agent-console/config.toml`. Each key resolves independently, so a config file
//! that sets `host` keeps working next to a `--port` on the command line.
//!
//! The environment sits in the middle for one specific reason: `--auth alice:hunter2` is
//! visible in `ps` output to every other user on the machine, so a password belongs in
//! `AGENT_CONSOLE_WEB_AUTH` (or the config file) rather than in argv.

use std::{
    env, io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
};

use crate::config::WebConfig;

use super::auth::Credentials;

pub(crate) const DEFAULT_HOST: &str = "127.0.0.1";
pub(crate) const DEFAULT_PORT: u16 = 7878;

const ENABLED_ENV: &str = "AGENT_CONSOLE_WEB_ENABLED";
const HOST_ENV: &str = "AGENT_CONSOLE_WEB_HOST";
const PORT_ENV: &str = "AGENT_CONSOLE_WEB_PORT";
const AUTH_ENV: &str = "AGENT_CONSOLE_WEB_AUTH";

/// What the command line asked for. `None` means "not given", which is what lets a lower
/// priority source supply the value instead of a default silently winning.
#[derive(Clone, Debug, Default)]
pub(crate) struct WebOverrides {
    pub(crate) enabled: Option<bool>,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) auth: Option<String>,
}

/// The environment's contribution, read once so resolution stays a pure function of its
/// inputs and can be tested without mutating the process environment.
#[derive(Clone, Debug, Default)]
pub(crate) struct WebEnv {
    pub(crate) enabled: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<String>,
    pub(crate) auth: Option<String>,
}

impl WebEnv {
    pub(crate) fn from_environment() -> Self {
        Self {
            enabled: non_empty(ENABLED_ENV),
            host: non_empty(HOST_ENV),
            port: non_empty(PORT_ENV),
            auth: non_empty(AUTH_ENV),
        }
    }
}

fn non_empty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// The resolved answer the server actually runs with.
#[derive(Clone, Debug)]
pub(crate) struct WebSettings {
    pub(crate) enabled: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `None` means no credentials were configured anywhere, which selects token mode.
    pub(crate) credentials: Option<Credentials>,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            credentials: None,
        }
    }
}

impl WebSettings {
    /// Command line beats environment beats config file, key by key.
    pub(crate) fn resolve(
        cli: &WebOverrides,
        environment: &WebEnv,
        config: &WebConfig,
    ) -> Result<Self, String> {
        let enabled = match (cli.enabled, &environment.enabled, config.enabled) {
            (Some(enabled), _, _) => enabled,
            (None, Some(value), _) => parse_bool(value, ENABLED_ENV)?,
            (None, None, Some(enabled)) => enabled,
            (None, None, None) => true,
        };
        let host = cli
            .host
            .clone()
            .or_else(|| environment.host.clone())
            .or_else(|| config.host.clone())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());
        let port = match (cli.port, &environment.port, config.port) {
            (Some(port), _, _) => port,
            (None, Some(value), _) => value
                .parse()
                .map_err(|_| format!("{PORT_ENV} is not a port number: {value}"))?,
            (None, None, Some(port)) => port,
            (None, None, None) => DEFAULT_PORT,
        };
        let credentials = match (&cli.auth, &environment.auth, &config.auth) {
            (Some(value), _, _) => Some(Credentials::parse(value, "--auth")?),
            (None, Some(value), _) => Some(Credentials::parse(value, AUTH_ENV)?),
            (None, None, Some(value)) => Some(Credentials::parse(value, "[web] auth")?),
            (None, None, None) => None,
        };
        Ok(Self {
            enabled,
            host,
            port,
            credentials,
        })
    }
}

fn parse_bool(value: &str, name: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "{name} must be true or false (got {other}); use 0/1, on/off, yes/no or true/false"
        )),
    }
}

/// Turns the configured host into the address the listener binds.
///
/// `ToSocketAddrs` rather than `IpAddr::from_str` because `localhost` -- the host a user is
/// most likely to type -- is a name, not a literal. When a name resolves to both families the
/// IPv4 address wins: binding a name's `::1` alone would leave `http://127.0.0.1:<port>`,
/// which is what the console prints and what people paste, refusing connections.
pub(crate) fn resolve_bind(host: &str, port: u16) -> io::Result<SocketAddr> {
    let resolved = (host, port).to_socket_addrs().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot resolve web host {host:?}: {error}"),
        )
    })?;
    let mut first = None;
    for address in resolved {
        if address.is_ipv4() {
            return Ok(address);
        }
        first.get_or_insert(address);
    }
    first.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("web host {host:?} resolved to no addresses"),
        )
    })
}

/// Whether the bound address is reachable only from this machine.
///
/// Keyed off the address the listener actually got, not off the string the user typed: a
/// hostname says nothing on its own, and `0.0.0.0`/`::` are wildcards that include every
/// non-loopback interface.
pub(crate) fn is_loopback_bind(address: &SocketAddr) -> bool {
    let ip = address.ip();
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(host: Option<&str>, port: Option<u16>, auth: Option<&str>) -> WebConfig {
        WebConfig {
            enabled: None,
            host: host.map(ToOwned::to_owned),
            port,
            auth: auth.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn nothing_configured_binds_loopback_on_the_default_port_with_no_credentials() {
        let settings = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv::default(),
            &WebConfig::default(),
        )
        .unwrap();

        assert!(settings.enabled);
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.port, 7878);
        assert_eq!(settings.credentials, None, "token mode is the fallback");
    }

    #[test]
    fn the_config_file_supplies_host_and_port_when_nothing_overrides_them() {
        let settings = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv::default(),
            &config(Some("0.0.0.0"), Some(8080), None),
        )
        .unwrap();

        assert_eq!(settings.host, "0.0.0.0");
        assert_eq!(settings.port, 8080);
    }

    /// Both directions, because a precedence bug that only ever widens the bind is the
    /// dangerous one: `--host 127.0.0.1` has to be able to pull a configured `0.0.0.0` back
    /// to loopback, not just the other way round.
    #[test]
    fn the_command_line_overrides_the_config_file_in_both_directions() {
        let narrowed = WebSettings::resolve(
            &WebOverrides {
                host: Some("127.0.0.1".into()),
                port: Some(9001),
                ..WebOverrides::default()
            },
            &WebEnv::default(),
            &config(Some("0.0.0.0"), Some(8080), None),
        )
        .unwrap();
        assert_eq!(narrowed.host, "127.0.0.1");
        assert_eq!(narrowed.port, 9001);

        let widened = WebSettings::resolve(
            &WebOverrides {
                host: Some("0.0.0.0".into()),
                ..WebOverrides::default()
            },
            &WebEnv::default(),
            &config(Some("127.0.0.1"), None, None),
        )
        .unwrap();
        assert_eq!(widened.host, "0.0.0.0");
    }

    #[test]
    fn the_environment_sits_between_the_command_line_and_the_config_file() {
        let environment = WebEnv {
            host: Some("10.0.0.5".into()),
            port: Some("8443".into()),
            auth: Some("env-user:env-pass".into()),
            enabled: None,
        };

        let without_cli = WebSettings::resolve(
            &WebOverrides::default(),
            &environment,
            &config(
                Some("192.168.0.103"),
                Some(8080),
                Some("file-user:file-pass"),
            ),
        )
        .unwrap();
        assert_eq!(without_cli.host, "10.0.0.5");
        assert_eq!(without_cli.port, 8443);
        assert_eq!(without_cli.credentials.unwrap().user, "env-user");

        let with_cli = WebSettings::resolve(
            &WebOverrides {
                host: Some("127.0.0.1".into()),
                port: Some(9001),
                auth: Some("cli-user:cli-pass".into()),
                enabled: None,
            },
            &environment,
            &config(
                Some("192.168.0.103"),
                Some(8080),
                Some("file-user:file-pass"),
            ),
        )
        .unwrap();
        assert_eq!(with_cli.host, "127.0.0.1");
        assert_eq!(with_cli.port, 9001);
        let credentials = with_cli.credentials.unwrap();
        assert_eq!(credentials.user, "cli-user");
        assert_eq!(credentials.password, "cli-pass");
    }

    #[test]
    fn credentials_fall_through_to_the_config_file_when_nothing_else_sets_them() {
        let settings = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv::default(),
            &config(None, None, Some("file-user:file-pass")),
        )
        .unwrap();

        assert_eq!(settings.credentials.unwrap().user, "file-user");
    }

    #[test]
    fn a_malformed_credential_names_its_own_source() {
        let from_cli = WebSettings::resolve(
            &WebOverrides {
                auth: Some("no-colon".into()),
                ..WebOverrides::default()
            },
            &WebEnv::default(),
            &WebConfig::default(),
        )
        .unwrap_err();
        assert!(from_cli.starts_with("--auth"), "{from_cli}");

        let from_env = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv {
                auth: Some("no-colon".into()),
                ..WebEnv::default()
            },
            &WebConfig::default(),
        )
        .unwrap_err();
        assert!(from_env.starts_with(AUTH_ENV), "{from_env}");

        let from_file = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv::default(),
            &config(None, None, Some("no-colon")),
        )
        .unwrap_err();
        assert!(from_file.starts_with("[web] auth"), "{from_file}");
    }

    #[test]
    fn the_embedded_server_can_be_switched_off_from_either_source() {
        let from_file = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv::default(),
            &WebConfig {
                enabled: Some(false),
                ..WebConfig::default()
            },
        )
        .unwrap();
        assert!(!from_file.enabled);

        let re_enabled_by_cli = WebSettings::resolve(
            &WebOverrides {
                enabled: Some(true),
                ..WebOverrides::default()
            },
            &WebEnv::default(),
            &WebConfig {
                enabled: Some(false),
                ..WebConfig::default()
            },
        )
        .unwrap();
        assert!(re_enabled_by_cli.enabled);

        let from_env = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv {
                enabled: Some("off".into()),
                ..WebEnv::default()
            },
            &WebConfig::default(),
        )
        .unwrap();
        assert!(!from_env.enabled);
    }

    #[test]
    fn an_unparseable_port_or_flag_from_the_environment_is_reported_not_ignored() {
        let port = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv {
                port: Some("http".into()),
                ..WebEnv::default()
            },
            &WebConfig::default(),
        )
        .unwrap_err();
        assert!(port.contains(PORT_ENV) && port.contains("http"), "{port}");

        let enabled = WebSettings::resolve(
            &WebOverrides::default(),
            &WebEnv {
                enabled: Some("maybe".into()),
                ..WebEnv::default()
            },
            &WebConfig::default(),
        )
        .unwrap_err();
        assert!(enabled.contains(ENABLED_ENV), "{enabled}");
    }

    /// A hostname has to work, not just a literal -- `localhost` is what people type, and
    /// parsing straight into `IpAddr` would reject it.
    #[test]
    fn a_hostname_resolves_to_a_loopback_bind() {
        let address = resolve_bind("localhost", 7878).unwrap();

        assert!(is_loopback_bind(&address), "{address} should be loopback");
        assert_eq!(address.port(), 7878);
    }

    #[test]
    fn loopback_literals_are_recognized() {
        for host in ["127.0.0.1", "::1"] {
            let address = resolve_bind(host, 7878).unwrap();
            assert!(is_loopback_bind(&address), "{host} should be loopback");
        }
    }

    /// Wildcards and a concrete LAN address are all exposed, and all have to warn.
    #[test]
    fn wildcard_and_lan_binds_are_flagged() {
        for host in ["0.0.0.0", "::", "192.168.0.103"] {
            let address = resolve_bind(host, 7878).unwrap();
            assert!(
                !is_loopback_bind(&address),
                "{host} resolved to {address}, which must not count as loopback"
            );
        }
    }

    #[test]
    fn a_host_that_cannot_be_resolved_says_so_instead_of_panicking() {
        let error = resolve_bind("", 7878).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("cannot resolve web host"),
            "{error}"
        );
    }
}
