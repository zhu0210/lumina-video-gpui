use lumina_video_core::video::VideoError;

#[derive(Debug, Clone)]
pub struct WebMoqUrl {
    /// Remote host
    host: String,
    /// Remote port
    port: u16,
    /// Whether to use TLS
    use_tls: bool,
    /// Auth path (e.g., "anon" for anonymous access on dev relays)
    auth_path: Option<String>,
    /// Namespace / broadcast name (last path segment)
    namespace: String,
    /// Query string (e.g., "jwt=xxx" for authentication)
    query: Option<String>,
    /// Original URL (reserved for debugging/logging)
    #[allow(dead_code)]
    original: String,
}

impl WebMoqUrl {
    /// Parses a MoQ URL string.
    pub fn parse(url: &str) -> Result<Self, VideoError> {
        let original = url.to_string();
        let parsed = url::Url::parse(url)
            .map_err(|error| VideoError::OpenFailed(format!("Invalid MoQ URL: {error}")))?;
        let use_tls = match parsed.scheme() {
            "moqs" => true,
            "moq" => false,
            _ => {
                return Err(VideoError::OpenFailed(
                    "URL must start with moq:// or moqs://".into(),
                ))
            }
        };
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(VideoError::OpenFailed(
                "MoQ URLs do not support userinfo or fragments".into(),
            ));
        }
        let host = parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| VideoError::OpenFailed("Missing host".into()))?
            .to_owned();
        let port = parsed.port().unwrap_or(443);
        let path = parsed.path();
        let query = parsed.query().map(str::to_owned);

        // Parse path: last segment = namespace, preceding segments = auth_path
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Err(VideoError::OpenFailed("Missing namespace".to_string()));
        }

        let (auth_path, namespace) = match path.rfind('/') {
            Some(idx) => (Some(path[..idx].to_string()), path[idx + 1..].to_string()),
            None => (None, path.to_string()),
        };

        if namespace.is_empty() {
            return Err(VideoError::OpenFailed("Missing namespace".to_string()));
        }

        Ok(WebMoqUrl {
            host,
            port,
            use_tls,
            auth_path,
            namespace,
            query,
            original,
        })
    }

    /// Returns the WebTransport URL for connection (includes auth path if present).
    pub fn webtransport_url(&self) -> String {
        let scheme = if self.use_tls { "https" } else { "http" };
        let path = match &self.auth_path {
            Some(p) => format!("/{}", p),
            None => String::new(),
        };
        match &self.query {
            Some(q) => format!("{}://{}:{}{}?{}", scheme, self.host, self.port, path, q),
            None => format!("{}://{}:{}{}", scheme, self.host, self.port, path),
        }
    }

    /// Returns the namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Returns true if this is a MoQ URL.
    pub fn is_moq_url(url: &str) -> bool {
        url.starts_with("moq://") || url.starts_with("moqs://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_moq_url_parse_with_auth_path() {
        let url = WebMoqUrl::parse("moq://localhost:4443/anon/bbb").unwrap();
        assert_eq!(url.host, "localhost");
        assert_eq!(url.port, 4443);
        assert!(!url.use_tls);
        assert_eq!(url.auth_path, Some("anon".to_string()));
        assert_eq!(url.namespace, "bbb");
        assert_eq!(url.webtransport_url(), "http://localhost:4443/anon");
    }

    #[test]
    fn test_moq_url_parse_no_auth_path() {
        let url = WebMoqUrl::parse("moqs://relay.example.com/live").unwrap();
        assert_eq!(url.host, "relay.example.com");
        assert_eq!(url.port, 443);
        assert!(url.use_tls);
        assert_eq!(url.auth_path, None);
        assert_eq!(url.namespace, "live");
        assert_eq!(url.webtransport_url(), "https://relay.example.com:443");
    }

    #[test]
    fn test_is_moq_url() {
        assert!(WebMoqUrl::is_moq_url("moq://localhost/test"));
        assert!(WebMoqUrl::is_moq_url("moqs://relay.example.com/live"));
        assert!(!WebMoqUrl::is_moq_url("https://example.com/video.mp4"));
    }
    #[test]
    fn ipv6_authorities_preserve_brackets_and_explicit_ports() {
        let url = WebMoqUrl::parse("moqs://[::1]:8443/anon/video?jwt=test").unwrap();
        assert_eq!(url.webtransport_url(), "https://[::1]:8443/anon?jwt=test");
        assert_eq!(url.namespace(), "video");
        assert!(WebMoqUrl::parse("moqs://[bad]/video").is_err());
        assert!(WebMoqUrl::parse("moqs://host:99999/video").is_err());
    }
}
