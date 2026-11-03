//! Runtime configuration, read once from the environment at startup.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use base64::Engine;
use reqwest::Url;
use tracing::warn;

const MIN_API_KEY_LEN: usize = 32;
const MAX_CONCURRENCY: usize = 64;
const MAX_WOL_STARTUP_SECONDS: u64 = 3600;
const NUM_CTX_RANGE: std::ops::RangeInclusive<u32> = 512..=1_048_576;

pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub api_key: String,
    /// Always ends in `/` so endpoint paths can be joined onto it.
    pub ollama_base_url: Url,
    pub ollama_model: String,
    /// `options.num_ctx` sent with every request; `None` leaves the context
    /// size to Ollama (`OLLAMA_CONTEXT_LENGTH`, Modelfile or its default).
    pub ollama_num_ctx: Option<u32>,
    pub virtual_model: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub max_concurrent_requests: usize,
    pub log_prompt_content: bool,
    /// `None` when Wake-on-LAN is disabled or its configuration is incomplete.
    pub wol: Option<WolConfig>,
    /// `None` unless `CHATGPT_FALLBACK_ENABLED=true`.
    pub chatgpt: Option<ChatGptConfig>,
}

/// The ChatGPT plan fallback (Sign in with ChatGPT, direct Responses API).
///
/// There is deliberately no API-key setting: this provider only ever sends
/// the OAuth access token from `opengate chatgpt login`.
#[derive(Clone)]
pub struct ChatGptConfig {
    /// Encrypted credential file, on a persistent volume.
    pub credentials_path: PathBuf,
    pub credentials_key: CredentialsKey,
    /// `None` = `auto`: the first model in the account's catalog.
    pub model: Option<String>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    /// After a usage-limit error, new fallback requests fail fast this long.
    pub limit_pause: Duration,
    /// Drop sampling hints the ChatGPT plan route rejects instead of failing.
    pub drop_unsupported_sampling: bool,
    /// Drop `max_tokens` (which the route cannot honour) instead of failing.
    pub drop_max_tokens: bool,
    /// OAuth issuer (OIDC discovery base). Fixed in production; tests point
    /// it at a mock. Not settable from the environment.
    pub issuer: Url,
    /// Responses API base, ending in `/`. Fixed like `issuer`.
    pub api_base: Url,
}

pub const CHATGPT_ISSUER: &str = "https://auth.openai.com";
pub const CHATGPT_API_BASE: &str = "https://api.openai.com/v1/";
/// The OAuth `resource` for ChatGPT plan inference (no trailing slash).
pub const CHATGPT_RESOURCE: &str = "https://api.openai.com/v1";
const DEFAULT_CREDENTIALS_PATH: &str = "/data/chatgpt/credentials.json";

/// 32-byte key for the credential file. Never printed.
#[derive(Clone)]
pub struct CredentialsKey(pub(crate) [u8; 32]);

impl FromStr for CredentialsKey {
    type Err = ();

    /// Standard or URL-safe base64, padded or not, of exactly 32 bytes.
    fn from_str(s: &str) -> Result<Self, ()> {
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        let bytes = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
            .iter()
            .find_map(|e| e.decode(s).ok())
            .ok_or(())?;
        Ok(CredentialsKey(bytes.try_into().map_err(|_| ())?))
    }
}

impl fmt::Debug for CredentialsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Debug for ChatGptConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptConfig")
            .field("credentials_path", &self.credentials_path)
            .field("credentials_key", &self.credentials_key)
            .field("model", &self.model.as_deref().unwrap_or("auto"))
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("limit_pause", &self.limit_pause)
            .field("drop_unsupported_sampling", &self.drop_unsupported_sampling)
            .field("drop_max_tokens", &self.drop_max_tokens)
            .field("issuer", &self.issuer.as_str())
            .field("api_base", &self.api_base.as_str())
            .finish()
    }
}

impl ChatGptConfig {
    /// For the `opengate chatgpt` commands, which work whether or not the
    /// gateway's fallback is enabled.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let env = Env(lookup);
        no_api_billing(&env)?;
        chatgpt_config(&env)
    }
}

/// Wake-on-LAN settings for the Ollama host.
#[derive(Clone, Debug)]
pub struct WolConfig {
    pub mac: MacAddress,
    /// Where the magic packet is sent.
    pub target: WolTarget,
    /// How long to wait for Ollama after sending the packet.
    pub startup_timeout: Duration,
    pub poll_interval: Duration,
    /// After a failed wake, requests fail fast for this long before a new
    /// wake is attempted.
    pub retry_cooldown: Duration,
}

#[derive(Clone, Debug)]
pub enum WolTarget {
    /// Broadcast directly on the LAN (OpenGate runs on the host network).
    Broadcast(SocketAddrV4),
    /// Unicast to a host-side relay (`host:port`) that broadcasts for us.
    /// Used when OpenGate runs on a Docker bridge network.
    Relay(String),
}

/// Settings for `opengate wol-relay`, the host-side broadcaster.
#[derive(Clone, Debug)]
pub struct WolRelayConfig {
    pub mac: MacAddress,
    pub broadcast: SocketAddrV4,
    pub listen: SocketAddr,
}

/// A unicast Ethernet MAC address.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MacAddress(pub(crate) [u8; 6]);

impl FromStr for MacAddress {
    type Err = ();

    /// Accepts `AA:BB:CC:DD:EE:64` or `AA-BB-CC-DD-EE-64`.
    fn from_str(s: &str) -> Result<Self, ()> {
        let sep = if s.contains('-') { '-' } else { ':' };
        let mut mac = [0u8; 6];
        let mut parts = s.split(sep);
        for byte in &mut mac {
            let part = parts.next().ok_or(())?;
            if part.len() != 2 {
                return Err(());
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| ())?;
        }
        // A NIC's own address is never all-zero or a group (multicast/broadcast) address.
        if parts.next().is_some() || mac == [0; 6] || mac[0] & 1 == 1 {
            return Err(());
        }
        Ok(MacAddress(mac))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{g:02X}")
    }
}

impl fmt::Debug for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Error messages name the variable but never echo its value.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let env = Env(lookup);

        let api_key = env.required("OPENGATE_API_KEY")?;
        if api_key.len() < MIN_API_KEY_LEN
            || api_key.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "OPENGATE_API_KEY must be at least {MIN_API_KEY_LEN} characters with no whitespace"
            ));
        }

        let mut ollama_base_url: Url = env
            .required("OLLAMA_BASE_URL")?
            .parse()
            .map_err(|_| "OLLAMA_BASE_URL is not a valid URL".to_string())?;
        if !matches!(ollama_base_url.scheme(), "http" | "https")
            || ollama_base_url.host_str().is_none()
            || !ollama_base_url.username().is_empty()
            || ollama_base_url.password().is_some()
            || ollama_base_url.query().is_some()
            || ollama_base_url.fragment().is_some()
        {
            return Err(
                "OLLAMA_BASE_URL must be a plain http(s) URL with no credentials or query".into(),
            );
        }
        if !ollama_base_url.path().ends_with('/') {
            let path = format!("{}/", ollama_base_url.path());
            ollama_base_url.set_path(&path);
        }

        let max_concurrent_requests = env.parse("MAX_CONCURRENT_REQUESTS", 2)?;
        if !(1..=MAX_CONCURRENCY).contains(&max_concurrent_requests) {
            return Err(format!(
                "MAX_CONCURRENT_REQUESTS must be between 1 and {MAX_CONCURRENCY}"
            ));
        }

        let ollama_num_ctx = env
            .get("OLLAMA_NUM_CTX")
            .map(|v| {
                v.parse()
                    .ok()
                    .filter(|n| NUM_CTX_RANGE.contains(n))
                    .ok_or_else(|| {
                        format!(
                            "OLLAMA_NUM_CTX must be a whole number between {} and {}",
                            NUM_CTX_RANGE.start(),
                            NUM_CTX_RANGE.end()
                        )
                    })
            })
            .transpose()?;

        let log_prompt_content = env.bool("LOG_PROMPT_CONTENT")?;
        let wol = wol_config(&env)?;
        no_api_billing(&env)?;
        let chatgpt = if env.bool("CHATGPT_FALLBACK_ENABLED")? {
            Some(chatgpt_config(&env)?)
        } else {
            None
        };

        Ok(Config {
            host: env.parse("OPENGATE_HOST", IpAddr::V4(Ipv4Addr::UNSPECIFIED))?,
            port: env.parse("OPENGATE_PORT", 8000)?,
            api_key,
            ollama_base_url,
            ollama_model: env.required("OLLAMA_MODEL")?,
            ollama_num_ctx,
            virtual_model: env
                .get("OPENGATE_VIRTUAL_MODEL")
                .unwrap_or_else(|| "opengate/local".into()),
            connect_timeout: env.seconds("UPSTREAM_CONNECT_TIMEOUT_SECONDS", 3)?,
            request_timeout: env.seconds("UPSTREAM_REQUEST_TIMEOUT_SECONDS", 120)?,
            stream_idle_timeout: env.seconds("UPSTREAM_STREAM_IDLE_TIMEOUT_SECONDS", 60)?,
            max_concurrent_requests,
            log_prompt_content,
            wol,
            chatgpt,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

// Hand-written so the API key can never end up in a log line.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen_addr())
            .field("api_key", &"<redacted>")
            .field("ollama_base_url", &self.ollama_base_url.as_str())
            .field("ollama_model", &self.ollama_model)
            .field("ollama_num_ctx", &self.ollama_num_ctx)
            .field("virtual_model", &self.virtual_model)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("log_prompt_content", &self.log_prompt_content)
            .field("wol", &self.wol)
            .field("chatgpt", &self.chatgpt)
            .finish()
    }
}

/// Wake-on-LAN is off unless `WOL_ENABLED=true`. When enabled, values that are
/// present must be valid (startup fails otherwise), but if the MAC or the
/// destination is missing, WOL stays disabled with a warning.
fn wol_config<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<Option<WolConfig>, String> {
    if !env.bool("WOL_ENABLED")? {
        return Ok(None);
    }
    let mac = env.get("WOL_MAC_ADDRESS").map(|_| mac(env)).transpose()?;
    let broadcast = env
        .get("WOL_BROADCAST_ADDRESS")
        .map(|_| broadcast(env))
        .transpose()?;
    let relay = env
        .get("WOL_RELAY_ADDRESS")
        .map(|v| relay_address(&v))
        .transpose()?;

    let startup_timeout = env.seconds("WOL_STARTUP_TIMEOUT_SECONDS", 90)?;
    if startup_timeout > Duration::from_secs(MAX_WOL_STARTUP_SECONDS) {
        return Err(format!(
            "WOL_STARTUP_TIMEOUT_SECONDS must be at most {MAX_WOL_STARTUP_SECONDS}"
        ));
    }
    let poll_interval = env.seconds("WOL_POLL_INTERVAL_SECONDS", 3)?;
    if poll_interval > startup_timeout {
        return Err("WOL_POLL_INTERVAL_SECONDS must not exceed WOL_STARTUP_TIMEOUT_SECONDS".into());
    }
    let retry_cooldown = env.seconds("WOL_RETRY_COOLDOWN_SECONDS", 30)?;

    let target = match (relay, broadcast) {
        (Some(relay), _) => Some(WolTarget::Relay(relay)),
        (None, Some(addr)) => Some(WolTarget::Broadcast(addr)),
        (None, None) => None,
    };
    match (mac, target) {
        (Some(mac), Some(target)) => Ok(Some(WolConfig {
            mac,
            target,
            startup_timeout,
            poll_interval,
            retry_cooldown,
        })),
        (mac, _) => {
            let missing = if mac.is_none() {
                "WOL_MAC_ADDRESS"
            } else {
                "WOL_BROADCAST_ADDRESS (or WOL_RELAY_ADDRESS)"
            };
            warn!("WOL_ENABLED=true but {missing} is not set; Wake-on-LAN is disabled");
            Ok(None)
        }
    }
}

/// OpenGate never bills an OpenAI Platform account. The switch exists only so
/// that turning it on fails loudly instead of being silently ignored.
fn no_api_billing<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<(), String> {
    if env.bool("OPENAI_API_BILLING_ENABLED")? {
        return Err(
            "OPENAI_API_BILLING_ENABLED=true is not supported: OpenGate only uses local Ollama \
             and the ChatGPT plan; it never uses OpenAI API keys or API billing"
                .into(),
        );
    }
    if env.get("OPENAI_API_KEY").is_some() {
        warn!("OPENAI_API_KEY is set but OpenGate never reads or sends it");
    }
    Ok(())
}

fn chatgpt_config<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<ChatGptConfig, String> {
    match env.get("CHATGPT_AUTH_METHOD").as_deref() {
        None | Some("official_oauth") => {}
        Some(_) => return Err("CHATGPT_AUTH_METHOD must be official_oauth".into()),
    }
    match env.get("CHATGPT_PROVIDER").as_deref() {
        None | Some("responses") => {}
        Some("codex_app_server") => {
            return Err("CHATGPT_PROVIDER=codex_app_server is not supported; use \
                 responses (README: \"Why not Codex app-server\")"
                .into())
        }
        Some(_) => return Err("CHATGPT_PROVIDER must be responses".into()),
    }
    let model = match env.get("CHATGPT_MODEL") {
        None => None,
        Some(m) if m == "auto" => None,
        Some(m)
            if m.len() <= 100
                && m.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')) =>
        {
            Some(m)
        }
        Some(_) => return Err("CHATGPT_MODEL must be auto or a model slug".into()),
    };
    let credentials_key = env
        .required("CHATGPT_CREDENTIALS_KEY")?
        .parse()
        .map_err(|_| {
            "CHATGPT_CREDENTIALS_KEY must be 32 random bytes, base64-encoded \
             (openssl rand -base64 32)"
                .to_string()
        })?;
    let credentials_path = PathBuf::from(
        env.get("CHATGPT_CREDENTIALS_PATH")
            .unwrap_or_else(|| DEFAULT_CREDENTIALS_PATH.into()),
    );
    if !credentials_path.is_absolute() || credentials_path.file_name().is_none() {
        return Err("CHATGPT_CREDENTIALS_PATH must be an absolute file path".into());
    }
    Ok(ChatGptConfig {
        credentials_path,
        credentials_key,
        model,
        connect_timeout: env.seconds("UPSTREAM_CONNECT_TIMEOUT_SECONDS", 3)?,
        request_timeout: env.seconds("CHATGPT_REQUEST_TIMEOUT_SECONDS", 300)?,
        stream_idle_timeout: env.seconds("CHATGPT_STREAM_IDLE_TIMEOUT_SECONDS", 120)?,
        limit_pause: env.seconds("CHATGPT_LIMIT_PAUSE_SECONDS", 300)?,
        drop_unsupported_sampling: env.bool("CHATGPT_DROP_UNSUPPORTED_SAMPLING")?,
        drop_max_tokens: env.bool("CHATGPT_DROP_MAX_TOKENS")?,
        issuer: CHATGPT_ISSUER.parse().expect("valid constant"),
        api_base: CHATGPT_API_BASE.parse().expect("valid constant"),
    })
}

impl WolRelayConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let env = Env(lookup);
        env.required("WOL_MAC_ADDRESS")?;
        env.required("WOL_BROADCAST_ADDRESS")?;
        Ok(WolRelayConfig {
            mac: mac(&env)?,
            broadcast: broadcast(&env)?,
            listen: env
                .required("WOL_RELAY_LISTEN")?
                .parse()
                .map_err(|_| "WOL_RELAY_LISTEN must be an IP:port address".to_string())?,
        })
    }
}

fn mac<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<MacAddress, String> {
    env.parse("WOL_MAC_ADDRESS", MacAddress([0; 6]))
        .map_err(|_| "WOL_MAC_ADDRESS must be a unicast MAC like AA:BB:CC:DD:EE:64".to_string())
}

fn broadcast<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<SocketAddrV4, String> {
    let ip: Ipv4Addr = env.parse("WOL_BROADCAST_ADDRESS", Ipv4Addr::UNSPECIFIED)?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err("WOL_BROADCAST_ADDRESS must be an IPv4 broadcast address".into());
    }
    let port: u16 = env.parse("WOL_PORT", 9)?;
    if port == 0 {
        return Err("WOL_PORT must be between 1 and 65535".into());
    }
    Ok(SocketAddrV4::new(ip, port))
}

/// `host:port` or `ip:port`; resolved when a packet is sent.
fn relay_address(value: &str) -> Result<String, String> {
    if value.parse::<SocketAddr>().is_ok() {
        return Ok(value.to_owned());
    }
    let valid = value.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            && port.parse::<u16>().is_ok_and(|p| p != 0)
    });
    if valid {
        Ok(value.to_owned())
    } else {
        Err("WOL_RELAY_ADDRESS must be host:port".into())
    }
}

struct Env<F>(F);

impl<F: Fn(&str) -> Option<String>> Env<F> {
    // Coolify can leave variables defined but blank — treat those as unset.
    fn get(&self, name: &str) -> Option<String> {
        (self.0)(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.get(name).ok_or_else(|| format!("{name} is required"))
    }

    fn parse<T: FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("{name} has an invalid value")),
        }
    }

    fn bool(&self, name: &str) -> Result<bool, String> {
        match self.get(name).as_deref() {
            None | Some("false") | Some("0") => Ok(false),
            Some("true") | Some("1") => Ok(true),
            Some(_) => Err(format!("{name} must be true or false")),
        }
    }

    fn seconds(&self, name: &str, default: u64) -> Result<Duration, String> {
        match self.parse(name, default)? {
            0 => Err(format!("{name} must be at least 1")),
            secs => Ok(Duration::from_secs(secs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn load(vars: &[(&str, &str)]) -> Result<Config, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    fn minimal() -> Vec<(&'static str, &'static str)> {
        vec![
            ("OPENGATE_API_KEY", KEY),
            ("OLLAMA_BASE_URL", "http://192.168.1.50:11434"),
            ("OLLAMA_MODEL", "llama3.1:8b"),
        ]
    }

    #[test]
    fn defaults() {
        let c = load(&minimal()).unwrap();
        assert_eq!(c.listen_addr().to_string(), "0.0.0.0:8000");
        assert_eq!(c.virtual_model, "opengate/local");
        assert_eq!(c.ollama_base_url.as_str(), "http://192.168.1.50:11434/");
        assert_eq!(c.max_concurrent_requests, 2);
        assert!(!c.log_prompt_content);
        assert_eq!(c.ollama_num_ctx, None);
    }

    #[test]
    fn num_ctx() {
        let mut vars = minimal();
        vars.push(("OLLAMA_NUM_CTX", "32768"));
        assert_eq!(load(&vars).unwrap().ollama_num_ctx, Some(32768));
        for bad in ["0", "100", "lots", "-1", "99999999"] {
            let mut vars = minimal();
            vars.push(("OLLAMA_NUM_CTX", bad));
            assert!(load(&vars).unwrap_err().contains("OLLAMA_NUM_CTX"), "{bad}");
        }
    }

    #[test]
    fn api_key_is_required_and_long() {
        let mut vars = minimal();
        vars.retain(|(k, _)| *k != "OPENGATE_API_KEY");
        assert!(load(&vars)
            .unwrap_err()
            .contains("OPENGATE_API_KEY is required"));
        vars.push(("OPENGATE_API_KEY", "   "));
        assert!(load(&vars)
            .unwrap_err()
            .contains("OPENGATE_API_KEY is required"));
        vars.pop();
        vars.push(("OPENGATE_API_KEY", "short"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("at least 32"));
        assert!(!err.contains("short"), "error must not echo the value");
    }

    #[test]
    fn rejects_bad_upstream_urls() {
        for url in [
            "ftp://host",
            "not a url",
            "http://user:pw@host:11434",
            "http://host?x=1",
        ] {
            let mut vars = minimal();
            vars.retain(|(k, _)| *k != "OLLAMA_BASE_URL");
            vars.push(("OLLAMA_BASE_URL", url));
            assert!(load(&vars).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn debug_redacts_key() {
        let c = load(&minimal()).unwrap();
        assert!(!format!("{c:?}").contains(KEY));
    }

    #[test]
    fn rejects_zero_timeouts_and_bad_limits() {
        for (k, v) in [
            ("UPSTREAM_REQUEST_TIMEOUT_SECONDS", "0"),
            ("MAX_CONCURRENT_REQUESTS", "0"),
            ("MAX_CONCURRENT_REQUESTS", "1000"),
            ("LOG_PROMPT_CONTENT", "yes"),
            ("OPENGATE_PORT", "http"),
        ] {
            let mut vars = minimal();
            vars.push((k, v));
            assert!(load(&vars).is_err(), "{k}={v} should be rejected");
        }
    }

    // Documentation-only test values; the real MAC and LAN live in .env only.
    const TEST_MAC: &str = "02:00:5e:10:00:01";

    fn wol(extra: &[(&'static str, &'static str)]) -> Result<Config, String> {
        let mut vars = minimal();
        vars.extend([
            ("WOL_ENABLED", "true"),
            ("WOL_MAC_ADDRESS", TEST_MAC),
            ("WOL_BROADCAST_ADDRESS", "10.9.8.255"),
        ]);
        for (k, v) in extra {
            vars.retain(|(name, _)| name != k);
            vars.push((k, v));
        }
        load(&vars)
    }

    #[test]
    fn wol_is_off_by_default() {
        assert!(load(&minimal()).unwrap().wol.is_none());
        assert!(wol(&[("WOL_ENABLED", "false")]).unwrap().wol.is_none());
    }

    #[test]
    fn wol_defaults() {
        let w = wol(&[]).unwrap().wol.unwrap();
        assert_eq!(w.mac.to_string(), "02:00:5E:10:00:01");
        assert!(matches!(w.target, WolTarget::Broadcast(a) if a.to_string() == "10.9.8.255:9"));
        assert_eq!(w.startup_timeout, Duration::from_secs(90));
        assert_eq!(w.poll_interval, Duration::from_secs(3));
        assert_eq!(w.retry_cooldown, Duration::from_secs(30));

        let w = wol(&[("WOL_MAC_ADDRESS", "02-00-5E-10-00-01"), ("WOL_PORT", "7")])
            .unwrap()
            .wol
            .unwrap();
        assert_eq!(w.mac.to_string(), "02:00:5E:10:00:01");
        assert!(matches!(w.target, WolTarget::Broadcast(a) if a.port() == 7));
    }

    #[test]
    fn wol_stays_disabled_when_incomplete() {
        for missing in ["WOL_MAC_ADDRESS", "WOL_BROADCAST_ADDRESS"] {
            let c = wol(&[(missing, "  ")]).unwrap();
            assert!(c.wol.is_none(), "{missing} unset must disable WOL");
        }
    }

    #[test]
    fn wol_relay_replaces_broadcast() {
        let w = wol(&[
            ("WOL_BROADCAST_ADDRESS", ""),
            ("WOL_RELAY_ADDRESS", "host.docker.internal:9009"),
        ])
        .unwrap()
        .wol
        .unwrap();
        assert!(matches!(w.target, WolTarget::Relay(r) if r == "host.docker.internal:9009"));
        assert!(wol(&[("WOL_RELAY_ADDRESS", "172.17.0.1:9009")]).is_ok());
    }

    #[test]
    fn rejects_invalid_wol_values() {
        for (k, v) in [
            ("WOL_ENABLED", "yes"),
            ("WOL_MAC_ADDRESS", "AA:BB:CC:DD:EE"),
            ("WOL_MAC_ADDRESS", "AA:BB:CC:DD:EE:64:00"),
            ("WOL_MAC_ADDRESS", "AA:BB:CC:DD:EE:6G"),
            ("WOL_MAC_ADDRESS", "AABBCCDDEE64"),
            ("WOL_MAC_ADDRESS", "8:BF:B8:EE:6C:64"),
            ("WOL_MAC_ADDRESS", "AA:BB-CC:DD:EE:64"),
            ("WOL_MAC_ADDRESS", "00:00:00:00:00:00"),
            ("WOL_MAC_ADDRESS", "FF:FF:FF:FF:FF:FF"),
            ("WOL_MAC_ADDRESS", "01:00:5E:00:00:01"),
            ("WOL_BROADCAST_ADDRESS", "not-an-ip"),
            ("WOL_BROADCAST_ADDRESS", "10.9.8"),
            ("WOL_BROADCAST_ADDRESS", "0.0.0.0"),
            ("WOL_BROADCAST_ADDRESS", "224.0.0.1"),
            ("WOL_BROADCAST_ADDRESS", "ff02::1"),
            ("WOL_PORT", "0"),
            ("WOL_PORT", "65536"),
            ("WOL_STARTUP_TIMEOUT_SECONDS", "0"),
            ("WOL_STARTUP_TIMEOUT_SECONDS", "100000"),
            ("WOL_POLL_INTERVAL_SECONDS", "0"),
            ("WOL_POLL_INTERVAL_SECONDS", "91"),
            ("WOL_RETRY_COOLDOWN_SECONDS", "-1"),
            ("WOL_RELAY_ADDRESS", "relay"),
            ("WOL_RELAY_ADDRESS", "relay:0"),
            ("WOL_RELAY_ADDRESS", "http://relay:9009"),
        ] {
            assert!(wol(&[(k, v)]).is_err(), "{k}={v} should be rejected");
        }
    }

    #[test]
    fn invalid_wol_values_are_ignored_while_disabled() {
        let c = wol(&[("WOL_ENABLED", "false"), ("WOL_MAC_ADDRESS", "nope")]).unwrap();
        assert!(c.wol.is_none());
    }

    const CRED_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

    fn chatgpt(extra: &[(&'static str, &'static str)]) -> Result<Config, String> {
        let mut vars = minimal();
        vars.extend([
            ("CHATGPT_FALLBACK_ENABLED", "true"),
            ("CHATGPT_CREDENTIALS_KEY", CRED_KEY),
        ]);
        for (k, v) in extra {
            vars.retain(|(name, _)| name != k);
            vars.push((k, v));
        }
        load(&vars)
    }

    #[test]
    fn chatgpt_is_off_by_default() {
        assert!(load(&minimal()).unwrap().chatgpt.is_none());
        let c = chatgpt(&[
            ("CHATGPT_FALLBACK_ENABLED", "false"),
            ("CHATGPT_CREDENTIALS_KEY", ""),
        ]);
        assert!(c.unwrap().chatgpt.is_none());
    }

    #[test]
    fn chatgpt_defaults() {
        let c = chatgpt(&[]).unwrap().chatgpt.unwrap();
        assert_eq!(
            c.credentials_path,
            PathBuf::from("/data/chatgpt/credentials.json")
        );
        assert_eq!(c.model, None);
        assert_eq!(c.issuer.as_str(), "https://auth.openai.com/");
        assert_eq!(c.api_base.as_str(), "https://api.openai.com/v1/");
        assert!(!c.drop_unsupported_sampling);
        assert!(!c.drop_max_tokens);
        let c = chatgpt(&[("CHATGPT_MODEL", "gpt-6.1-sol")])
            .unwrap()
            .chatgpt
            .unwrap();
        assert_eq!(c.model.as_deref(), Some("gpt-6.1-sol"));
    }

    #[test]
    fn api_billing_is_refused() {
        for vars in [
            vec![("OPENAI_API_BILLING_ENABLED", "true")],
            vec![("OPENAI_API_BILLING_ENABLED", "1")],
        ] {
            let mut all = minimal();
            all.extend(vars);
            assert!(load(&all)
                .unwrap_err()
                .contains("OPENAI_API_BILLING_ENABLED"));
            assert!(ChatGptConfig::from_lookup(|n| all
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string()))
            .is_err());
        }
        let mut ok = minimal();
        ok.push(("OPENAI_API_BILLING_ENABLED", "false"));
        assert!(load(&ok).is_ok());
    }

    #[test]
    fn rejects_invalid_chatgpt_values() {
        for (k, v) in [
            ("CHATGPT_CREDENTIALS_KEY", ""),
            ("CHATGPT_CREDENTIALS_KEY", "short"),
            ("CHATGPT_CREDENTIALS_KEY", "MDEyMzQ1Njc4OWFiY2RlZg=="),
            ("CHATGPT_AUTH_METHOD", "api_key"),
            ("CHATGPT_AUTH_METHOD", "cookies"),
            ("CHATGPT_PROVIDER", "codex_app_server"),
            ("CHATGPT_PROVIDER", "openai_api"),
            ("CHATGPT_MODEL", "gpt 5"),
            ("CHATGPT_MODEL", "http://evil/"),
            ("CHATGPT_CREDENTIALS_PATH", "relative/creds.json"),
            ("CHATGPT_LIMIT_PAUSE_SECONDS", "0"),
            ("CHATGPT_DROP_UNSUPPORTED_SAMPLING", "maybe"),
            ("CHATGPT_DROP_MAX_TOKENS", "maybe"),
        ] {
            let err = chatgpt(&[(k, v)])
                .err()
                .unwrap_or_else(|| panic!("{k}={v} accepted"));
            assert!(err.contains(k), "{err}");
            if k == "CHATGPT_CREDENTIALS_KEY" && !v.is_empty() {
                assert!(!err.contains(v), "error must not echo the key");
            }
        }
    }

    #[test]
    fn debug_redacts_credentials_key() {
        let c = chatgpt(&[]).unwrap();
        let printed = format!("{c:?}");
        assert!(printed.contains("<redacted>") && !printed.contains(CRED_KEY));
    }

    #[test]
    fn relay_config() {
        let vars: HashMap<&str, &str> = [
            ("WOL_MAC_ADDRESS", TEST_MAC),
            ("WOL_BROADCAST_ADDRESS", "10.9.8.255"),
            ("WOL_RELAY_LISTEN", "172.17.0.1:9009"),
        ]
        .into();
        let get = |vars: &HashMap<&str, &str>, name: &str| vars.get(name).map(|v| v.to_string());
        let c = WolRelayConfig::from_lookup(|n| get(&vars, n)).unwrap();
        assert_eq!(c.broadcast.to_string(), "10.9.8.255:9");
        assert_eq!(c.listen.to_string(), "172.17.0.1:9009");
        for (k, v) in [
            ("WOL_RELAY_LISTEN", "localhost:9009"),
            ("WOL_RELAY_LISTEN", ""),
            ("WOL_MAC_ADDRESS", ""),
            ("WOL_BROADCAST_ADDRESS", "x"),
        ] {
            let mut bad = vars.clone();
            bad.insert(k, v);
            assert!(
                WolRelayConfig::from_lookup(|n| get(&bad, n)).is_err(),
                "{k}={v} should be rejected"
            );
        }
    }
}
