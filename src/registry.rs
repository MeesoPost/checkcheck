use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{ACCEPT, WWW_AUTHENTICATE};
use reqwest::{Client, Method, Response, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;

const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.oci.image.manifest.v1+json";

const DIGEST_HEADER: &str = "docker-content-digest";

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub host: String,
    pub repository: String,
    pub tag: String,
    pub pinned_digest: Option<String>,
    pub name_no_tag: String,
}

pub fn parse(image: &str) -> Result<ImageRef> {
    let image = image.trim();
    if image.is_empty() {
        bail!("empty image reference");
    }

    let (name_part, pinned_digest) = match image.split_once('@') {
        Some((name, digest)) => (name, Some(digest.to_string())),
        None => (image, None),
    };

    let tag_sep = name_part
        .rfind(':')
        .filter(|i| !name_part[*i..].contains('/'));
    let (name_no_tag, tag) = match tag_sep {
        Some(i) => (&name_part[..i], &name_part[i + 1..]),
        None => (name_part, "latest"),
    };

    let (registry, repository) = match name_no_tag.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first, rest.to_string())
        }
        _ if name_no_tag.contains('/') => ("docker.io", name_no_tag.to_string()),
        _ => ("docker.io", format!("library/{name_no_tag}")),
    };

    let host = match registry {
        "docker.io" | "index.docker.io" => "registry-1.docker.io",
        other => other,
    };

    Ok(ImageRef {
        host: host.to_string(),
        repository,
        tag: tag.to_string(),
        pinned_digest,
        name_no_tag: name_no_tag.to_string(),
    })
}

impl ImageRef {
    pub fn local_digest(&self, repo_digests: &[String]) -> Option<String> {
        if let Some(pinned) = &self.pinned_digest {
            return Some(pinned.clone());
        }

        let want = normalize_name(&self.name_no_tag);
        let matched = repo_digests
            .iter()
            .filter_map(|entry| entry.split_once('@'))
            .find(|(repo, _)| normalize_name(repo) == want);
        if let Some((_, digest)) = matched {
            return Some(digest.to_string());
        }

        match repo_digests {
            [only] => only.split_once('@').map(|(_, d)| d.to_string()),
            _ => None,
        }
    }
}

fn normalize_name(name: &str) -> &str {
    let name = name
        .strip_prefix("docker.io/")
        .or_else(|| name.strip_prefix("index.docker.io/"))
        .unwrap_or(name);
    name.strip_prefix("library/").unwrap_or(name)
}

pub struct RegistryClient {
    http: Client,
    tokens: HashMap<String, String>,
}

impl RegistryClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent(concat!("checkcheck/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the HTTP client")?;
        Ok(Self {
            http,
            tokens: HashMap::new(),
        })
    }

    pub async fn manifest_digest(&mut self, image: &ImageRef) -> Result<String> {
        let url = format!(
            "https://{}/v2/{}/manifests/{}",
            image.host, image.repository, image.tag
        );

        let resp = self
            .send(Method::HEAD, &url, MANIFEST_ACCEPT, image)
            .await?;
        let status = resp.status();
        match status {
            StatusCode::NOT_FOUND => bail!("tag `{}` does not exist in the registry", image.tag),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                bail!("registry refused anonymous access (private repository?)")
            }
            StatusCode::TOO_MANY_REQUESTS => bail!("registry rate limit reached (HTTP 429)"),
            _ => {}
        }

        if status.is_success() {
            if let Some(digest) = digest_header(&resp) {
                return Ok(digest);
            }
        } else if status != StatusCode::METHOD_NOT_ALLOWED {
            bail!("registry returned HTTP {status}");
        }

        self.digest_via_get(&url, image).await
    }

    async fn digest_via_get(&mut self, url: &str, image: &ImageRef) -> Result<String> {
        let resp = self.send(Method::GET, url, MANIFEST_ACCEPT, image).await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("registry returned HTTP {status}");
        }
        if let Some(digest) = digest_header(&resp) {
            return Ok(digest);
        }

        let body = resp
            .bytes()
            .await
            .context("could not read the manifest body")?;
        Ok(format!("sha256:{:x}", Sha256::digest(&body)))
    }

    pub async fn manifest_created(
        &mut self,
        image: &ImageRef,
        digest: &str,
        os: &str,
        architecture: &str,
    ) -> Option<String> {
        let manifest = self
            .get_json(image, "manifests", digest, MANIFEST_ACCEPT)
            .await
            .ok()?;

        let config_digest = match manifest.get("manifests").and_then(|m| m.as_array()) {
            Some(list) => {
                let entry = list.iter().find(|m| {
                    m.get("platform").is_some_and(|p| {
                        p.get("os").and_then(|v| v.as_str()) == Some(os)
                            && p.get("architecture").and_then(|v| v.as_str()) == Some(architecture)
                    })
                })?;
                let sub_digest = entry.get("digest")?.as_str()?;
                let single = self
                    .get_json(image, "manifests", sub_digest, MANIFEST_ACCEPT)
                    .await
                    .ok()?;
                single.get("config")?.get("digest")?.as_str()?.to_string()
            }
            None => manifest.get("config")?.get("digest")?.as_str()?.to_string(),
        };

        let config = self
            .get_json(image, "blobs", &config_digest, "application/json")
            .await
            .ok()?;
        config.get("created")?.as_str().map(str::to_string)
    }

    async fn get_json(
        &mut self,
        image: &ImageRef,
        kind: &str,
        reference: &str,
        accept: &str,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "https://{}/v2/{}/{kind}/{reference}",
            image.host, image.repository
        );
        let resp = self.send(Method::GET, &url, accept, image).await?;
        if !resp.status().is_success() {
            bail!("registry returned HTTP {}", resp.status());
        }
        resp.json()
            .await
            .context("could not parse the registry response as JSON")
    }

    async fn send(
        &mut self,
        method: Method,
        url: &str,
        accept: &str,
        image: &ImageRef,
    ) -> Result<Response> {
        let key = format!("{}/{}", image.host, image.repository);
        let cached = self.tokens.get(&key).cloned();

        let resp = self
            .request(method.clone(), url, accept, cached.as_deref())
            .await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        let challenge = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let token = self.fetch_token(&challenge, image).await?;
        self.tokens.insert(key, token.clone());
        self.request(method, url, accept, Some(&token)).await
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        accept: &str,
        token: Option<&str>,
    ) -> Result<Response> {
        let mut req = self.http.request(method, url).header(ACCEPT, accept);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send()
            .await
            .with_context(|| format!("could not reach {url}"))
    }

    async fn fetch_token(&self, challenge: &str, image: &ImageRef) -> Result<String> {
        let params = parse_challenge(challenge);
        let realm = params
            .get("realm")
            .ok_or_else(|| anyhow!("registry did not offer a bearer token endpoint"))?;
        let scope = params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| format!("repository:{}:pull", image.repository));

        let mut req = self.http.get(realm).query(&[("scope", scope.as_str())]);
        if let Some(service) = params.get("service") {
            req = req.query(&[("service", service.as_str())]);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("token request to {realm} failed"))?;
        if !resp.status().is_success() {
            bail!("token endpoint returned HTTP {}", resp.status());
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            token: Option<String>,
            access_token: Option<String>,
        }

        let body: TokenResponse = resp
            .json()
            .await
            .context("could not parse the token response")?;
        body.token
            .or(body.access_token)
            .ok_or_else(|| anyhow!("token endpoint returned no token"))
    }
}

fn digest_header(resp: &Response) -> Option<String> {
    resp.headers()
        .get(DIGEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn parse_challenge(header: &str) -> HashMap<String, String> {
    let header = header.trim();
    let rest = match header.get(..6) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer") => &header[6..],
        _ => header,
    };

    rest.split(',')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| {
            (
                key.trim().to_ascii_lowercase(),
                value.trim().trim_matches('"').to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_hub_official_image() {
        let r = parse("nginx:latest").unwrap();
        assert_eq!(r.host, "registry-1.docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn defaults_to_latest() {
        let r = parse("linuxserver/sonarr").unwrap();
        assert_eq!(r.repository, "linuxserver/sonarr");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parses_custom_registry_with_port() {
        let r = parse("registry.local:5000/team/app:v2").unwrap();
        assert_eq!(r.host, "registry.local:5000");
        assert_eq!(r.repository, "team/app");
        assert_eq!(r.tag, "v2");
    }

    #[test]
    fn port_without_tag_is_not_a_tag() {
        let r = parse("registry.local:5000/team/app").unwrap();
        assert_eq!(r.host, "registry.local:5000");
        assert_eq!(r.repository, "team/app");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parses_lscr_image() {
        let r = parse("lscr.io/linuxserver/sonarr:latest").unwrap();
        assert_eq!(r.host, "lscr.io");
        assert_eq!(r.repository, "linuxserver/sonarr");
    }

    #[test]
    fn detects_pinned_digest() {
        let r = parse("nginx@sha256:abc").unwrap();
        assert_eq!(r.pinned_digest.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn matches_repo_digest() {
        let r = parse("docker.io/library/nginx:latest").unwrap();
        let digests = vec!["nginx@sha256:beef".to_string()];
        assert_eq!(r.local_digest(&digests).as_deref(), Some("sha256:beef"));
    }

    #[test]
    fn picks_the_matching_entry_among_several() {
        let r = parse("lscr.io/linuxserver/sonarr").unwrap();
        let digests = vec![
            "ghcr.io/other/app@sha256:aaa".to_string(),
            "lscr.io/linuxserver/sonarr@sha256:bbb".to_string(),
        ];
        assert_eq!(r.local_digest(&digests).as_deref(), Some("sha256:bbb"));
    }

    #[test]
    fn parses_bearer_challenge() {
        let params =
            parse_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry""#);
        assert_eq!(
            params.get("realm").map(String::as_str),
            Some("https://auth.docker.io/token")
        );
        assert_eq!(params.get("service").map(String::as_str), Some("registry"));
    }
}
