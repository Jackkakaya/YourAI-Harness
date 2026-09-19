//! HTTP hook 执行器：POST JSON，仅 2xx 成功，不跟随重定向。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};

/// HTTP Hook 的宿主级安全策略。`None` 表示不额外限制，空列表表示全部拒绝。
/// DNS 安全检查默认允许 loopback，以支持已受信任配置中的本地 Hook 服务；宿主可用
/// `allowed_urls` 进一步收紧，且必须在注册 Project 配置前完成 workspace trust 检查。
#[derive(Debug, Clone, Default)]
pub struct HttpHookPolicy {
    pub allowed_urls: Option<Vec<String>>,
    pub allowed_env_vars: Option<Vec<String>>,
}

/// HTTP hook handler。
pub struct HttpHandler {
    url: String,
    headers: Option<HashMap<String, String>>,
    allowed_env_vars: Option<Vec<String>>,
    policy: HttpHookPolicy,
}

impl HttpHandler {
    pub fn new(
        url: String,
        headers: Option<HashMap<String, String>>,
        allowed_env_vars: Option<Vec<String>>,
    ) -> Self {
        Self {
            url,
            headers,
            allowed_env_vars,
            policy: HttpHookPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: HttpHookPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn interpolate_headers(&self) -> HashMap<String, String> {
        let mut result = HashMap::new();
        result.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(headers) = &self.headers {
            let hook_allowed: HashSet<&str> = self
                .allowed_env_vars
                .as_ref()
                .map(|vars| vars.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let allowed: HashSet<&str> = match &self.policy.allowed_env_vars {
                Some(global) => hook_allowed
                    .into_iter()
                    .filter(|name| global.iter().any(|item| item == name))
                    .collect(),
                None => hook_allowed,
            };
            for (name, value) in headers {
                let interpolated = interpolate_env_vars(value, &allowed);
                let sanitized = sanitize_header_value(&interpolated);
                result.insert(name.clone(), sanitized);
            }
        }
        result
    }

    async fn run(
        &self,
        invocation: &HookInvocation,
    ) -> Result<(String, u16), yourai_core::YourAiError> {
        let json_body = crate::hooks::event::to_wire_json(invocation).to_string();
        let headers = self.interpolate_headers();
        let (url, resolved) = self.validate_target().await?;

        let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        if let (Some(host), Some(addresses)) = (url.host_str(), resolved.as_deref()) {
            client = client.resolve_to_addrs(host, addresses);
        }
        let client = client
            .build()
            .map_err(|e| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed to build HTTP client: {e}"),
            })?;

        let response = client
            .post(url)
            .headers(headers_to_reqwest(headers))
            .body(json_body)
            .send()
            .await
            .map_err(|e| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("HTTP request failed: {e}"),
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("HTTP hook returned non-2xx status: {status}"),
            }
            .into());
        }

        let body = response
            .text()
            .await
            .map_err(|e| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed to read HTTP body: {e}"),
            })?;

        Ok((body, status))
    }

    async fn validate_target(
        &self,
    ) -> Result<(reqwest::Url, Option<Vec<SocketAddr>>), yourai_core::YourAiError> {
        if let Some(patterns) = &self.policy.allowed_urls {
            if !patterns
                .iter()
                .any(|pattern| wildcard_url_match(pattern, &self.url))
            {
                return Err(yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("HTTP hook URL is not allowed: {}", self.url),
                }
                .into());
            }
        }

        let url = reqwest::Url::parse(&self.url).map_err(|e| {
            yourai_core::ErrorKind::Config(format!("invalid HTTP hook URL '{}': {e}", self.url))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(yourai_core::ErrorKind::Config(format!(
                "HTTP hook URL must use http or https: {}",
                self.url
            ))
            .into());
        }
        let host = url.host_str().ok_or_else(|| {
            yourai_core::YourAiError::from(yourai_core::ErrorKind::Config(format!(
                "HTTP hook URL has no host: {}",
                self.url
            )))
        })?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            ensure_safe_ip(ip)?;
            return Ok((url, None));
        }

        let port = url.port_or_known_default().unwrap_or(80);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed to resolve HTTP hook host '{host}': {e}"),
            })?
            .collect();
        if addresses.is_empty() {
            return Err(yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("HTTP hook host resolved to no addresses: {host}"),
            }
            .into());
        }
        for address in &addresses {
            ensure_safe_ip(address.ip())?;
        }
        Ok((url, Some(addresses)))
    }
}

impl HookHandler for HttpHandler {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> crate::hooks::handler::BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        Box::pin(async move {
            let (body, status) = self.run(invocation).await?;
            Ok(HookOutput::Http { body, status })
        })
    }
}

fn interpolate_env_vars(value: &str, allowed: &std::collections::HashSet<&str>) -> String {
    let re = regex::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}|\$([A-Z_][A-Z0-9_]*)").unwrap();
    re.replace_all(value, |caps: &regex::Captures| {
        let var_name = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        if allowed.contains(var_name) {
            std::env::var(var_name).unwrap_or_default()
        } else {
            String::new()
        }
    })
    .to_string()
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\0')
        .collect()
}

fn headers_to_reqwest(headers: HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            map.insert(n, v);
        }
    }
    map
}

fn wildcard_url_match(pattern: &str, url: &str) -> bool {
    let mut expression = String::from("^");
    let authority_end = pattern
        .find("://")
        .map(|scheme_end| {
            pattern[scheme_end + 3..]
                .find('/')
                .map(|path_start| scheme_end + 3 + path_start)
                .unwrap_or(pattern.len())
        })
        .unwrap_or(0);
    for (index, ch) in pattern.char_indices() {
        if ch == '*' {
            if index < authority_end {
                // Host wildcard 只能匹配当前 DNS label，不能跨 `.`、端口或进入 path。
                expression.push_str("[^./:]*");
            } else {
                expression.push_str(".*");
            }
        } else {
            expression.push_str(&regex::escape(&ch.to_string()));
        }
    }
    expression.push('$');
    regex::Regex::new(&expression)
        .map(|compiled| compiled.is_match(url))
        .unwrap_or(false)
}

fn ensure_safe_ip(ip: IpAddr) -> Result<(), yourai_core::YourAiError> {
    let ip = match ip {
        IpAddr::V6(ipv6) => ipv6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ipv6)),
        ip => ip,
    };
    // 本地 HTTP hook 是受支持的显式用例；workspace trust 必须在注册配置前完成。
    if ip.is_loopback() {
        return Ok(());
    }
    if !is_publicly_routable(ip) {
        return Err(yourai_core::ErrorKind::Provider {
            name: "hook",
            message: format!("HTTP hook blocked by SSRF policy: {ip}"),
        }
        .into());
    }
    Ok(())
}

fn is_publicly_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let [a, b, c, _] = ipv4.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(ipv6) => {
            let segments = ipv6.segments();
            // 仅允许 2000::/3 全球单播，并排除文档前缀 2001:db8::/32。
            segments[0] & 0xe000 == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_allowed_var() {
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("MY_TOKEN");
        std::env::set_var("MY_TOKEN", "secret123");
        let result = interpolate_env_vars("Bearer $MY_TOKEN", &allowed);
        assert_eq!(result, "Bearer secret123");
    }

    #[test]
    fn interpolate_blocked_var() {
        let allowed = std::collections::HashSet::<&str>::new();
        let result = interpolate_env_vars("Bearer $MY_TOKEN", &allowed);
        assert_eq!(result, "Bearer ");
    }

    #[test]
    fn sanitize_strips_crlf() {
        let result = sanitize_header_value("token\r\nX-Evil: 1");
        assert_eq!(result, "tokenX-Evil: 1");
    }

    #[test]
    fn url_allowlist_supports_wildcards() {
        assert!(wildcard_url_match(
            "https://hooks.example.com/*",
            "https://hooks.example.com/trace"
        ));
        assert!(!wildcard_url_match(
            "https://hooks.example.com/*",
            "https://evil.example/trace"
        ));
        assert!(!wildcard_url_match(
            "https://*.example.com/*",
            "https://evil.com/a.example.com/trace"
        ));
    }

    #[test]
    fn ssrf_policy_blocks_private_but_allows_loopback() {
        assert!(ensure_safe_ip("10.0.0.1".parse().unwrap()).is_err());
        assert!(ensure_safe_ip("169.254.1.1".parse().unwrap()).is_err());
        assert!(ensure_safe_ip("127.0.0.1".parse().unwrap()).is_ok());
        assert!(ensure_safe_ip("100.64.0.1".parse().unwrap()).is_err());
        assert!(ensure_safe_ip("::ffff:10.0.0.1".parse().unwrap()).is_err());
    }

    #[test]
    fn global_env_policy_intersects_hook_allowlist() {
        std::env::set_var("HOOK_ALLOWED_TOKEN", "allowed");
        std::env::set_var("HOOK_BLOCKED_TOKEN", "blocked");
        let handler = HttpHandler::new(
            "https://example.com".to_string(),
            Some(HashMap::from([(
                "Authorization".to_string(),
                "$HOOK_ALLOWED_TOKEN:$HOOK_BLOCKED_TOKEN".to_string(),
            )])),
            Some(vec![
                "HOOK_ALLOWED_TOKEN".to_string(),
                "HOOK_BLOCKED_TOKEN".to_string(),
            ]),
        )
        .with_policy(HttpHookPolicy {
            allowed_urls: None,
            allowed_env_vars: Some(vec!["HOOK_ALLOWED_TOKEN".to_string()]),
        });
        assert_eq!(handler.interpolate_headers()["Authorization"], "allowed:");
    }
}
