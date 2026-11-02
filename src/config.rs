//! Runtime configuration, read once from the environment at startup.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    time::Duration,
};

use reqwest::Url;

const MIN_API_KEY_LEN: usize = 32;
const MAX_CONCURRENCY: usize = 64;

pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub api_key: String,
    /// Always ends in `/` so endpoint paths can be joined onto it.
    pub ollama_base_url: Url,
    pub ollama_model: String,
    pub virtual_model: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub max_concurrent_requests: usize,
    pub log_prompt_content: bool,
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

        let log_prompt_content = match env.get("LOG_PROMPT_CONTENT").as_deref() {
            None | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            Some(_) => return Err("LOG_PROMPT_CONTENT must be true or false".into()),
        };

        Ok(Config {
            host: env.parse("OPENGATE_HOST", IpAddr::V4(Ipv4Addr::UNSPECIFIED))?,
            port: env.parse("OPENGATE_PORT", 8000)?,
            api_key,
            ollama_base_url,
            ollama_model: env.required("OLLAMA_MODEL")?,
            virtual_model: env
                .get("OPENGATE_VIRTUAL_MODEL")
                .unwrap_or_else(|| "opengate/local".into()),
            connect_timeout: env.seconds("UPSTREAM_CONNECT_TIMEOUT_SECONDS", 3)?,
            request_timeout: env.seconds("UPSTREAM_REQUEST_TIMEOUT_SECONDS", 120)?,
            stream_idle_timeout: env.seconds("UPSTREAM_STREAM_IDLE_TIMEOUT_SECONDS", 60)?,
            max_concurrent_requests,
            log_prompt_content,
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
            .field("virtual_model", &self.virtual_model)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("log_prompt_content", &self.log_prompt_content)
            .finish()
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
}
