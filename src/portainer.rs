use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const TOKEN_ENV: &str = "CHECKCHECK_PORTAINER_TOKEN";
pub const URL_ENV: &str = "CHECKCHECK_PORTAINER_URL";

pub const PROJECT_LABEL: &str = "com.docker.compose.project";

#[derive(Debug, Clone, Deserialize)]
pub struct Stack {
    #[serde(rename = "Id")]
    pub id: i64,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "EndpointId")]
    pub endpoint_id: i64,
    #[serde(rename = "Env", default)]
    pub env: Vec<EnvVar>,
    #[serde(rename = "GitConfig", default)]
    pub git_config: Option<serde_json::Value>,
}

impl Stack {
    pub fn is_git_backed(&self) -> bool {
        self.git_config.is_some()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

pub struct PortainerClient {
    http: Client,
    base: String,
    token: String,
}

impl PortainerClient {
    pub fn from_env(url: Option<&str>, timeout_secs: u64, insecure: bool) -> Result<Self> {
        let base = match url {
            Some(url) => url.to_string(),
            None => std::env::var(URL_ENV)
                .with_context(|| format!("set --portainer-url or ${URL_ENV}"))?,
        };
        let token = std::env::var(TOKEN_ENV).with_context(|| {
            format!("set ${TOKEN_ENV} to a Portainer API token (My account → Access tokens)")
        })?;
        if token.trim().is_empty() {
            bail!("${TOKEN_ENV} is empty");
        }

        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!("checkcheck/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the Portainer HTTP client")?;

        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
        })
    }

    pub async fn stacks(&self) -> Result<Vec<Stack>> {
        self.get_json("/api/stacks")
            .await
            .context("could not list Portainer stacks")
    }

    pub async fn stack_file(&self, id: i64) -> Result<String> {
        #[derive(Deserialize)]
        struct StackFile {
            #[serde(rename = "StackFileContent")]
            content: String,
        }
        let file: StackFile = self
            .get_json(&format!("/api/stacks/{id}/file"))
            .await
            .with_context(|| format!("could not read the compose file of stack {id}"))?;
        Ok(file.content)
    }

    pub async fn redeploy(&self, stack: &Stack, compose: &str) -> Result<()> {
        let url = format!(
            "{}/api/stacks/{}?endpointId={}",
            self.base, stack.id, stack.endpoint_id
        );
        let resp = self
            .http
            .put(&url)
            .header("X-API-Key", &self.token)
            .json(&redeploy_body(stack, compose))
            .send()
            .await
            .with_context(|| format!("could not reach {url}"))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = resp.text().await.unwrap_or_default();
        bail!("{}", describe(status, &detail));
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .http
            .get(&url)
            .header("X-API-Key", &self.token)
            .send()
            .await
            .with_context(|| format!("could not reach {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            bail!("{}", describe(status, &detail));
        }
        resp.json()
            .await
            .context("could not parse the Portainer response as JSON")
    }
}

fn redeploy_body(stack: &Stack, compose: &str) -> serde_json::Value {
    serde_json::json!({
        "stackFileContent": compose,
        "env": stack.env,
        "prune": false,
        "pullImage": true,
    })
}

fn describe(status: StatusCode, body: &str) -> String {
    let hint = match status {
        StatusCode::UNAUTHORIZED => {
            format!(" — check that ${TOKEN_ENV} is valid and not expired")
        }
        StatusCode::FORBIDDEN => " — the token's user lacks access to this stack".to_string(),
        StatusCode::NOT_FOUND => " — no such stack, or the API path differs in this \
             Portainer version"
            .to_string(),
        _ => String::new(),
    };

    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());

    if message.trim().is_empty() {
        format!("Portainer returned HTTP {status}{hint}")
    } else {
        format!("Portainer returned HTTP {status}{hint}: {}", message.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_stack_listing() {
        let json = r#"[{
            "Id": 3,
            "Name": "gitea",
            "EndpointId": 2,
            "Env": [{"name": "TZ", "value": "Europe/Amsterdam"}],
            "GitConfig": null
        }]"#;
        let stacks: Vec<Stack> = serde_json::from_str(json).unwrap();
        assert_eq!(stacks[0].id, 3);
        assert_eq!(stacks[0].name, "gitea");
        assert_eq!(stacks[0].endpoint_id, 2);
        assert_eq!(stacks[0].env[0].name, "TZ");
        assert!(!stacks[0].is_git_backed());
    }

    #[test]
    fn detects_a_git_backed_stack() {
        let json = r#"[{
            "Id": 4, "Name": "media", "EndpointId": 1,
            "GitConfig": {"URL": "https://github.com/example/stacks"}
        }]"#;
        let stacks: Vec<Stack> = serde_json::from_str(json).unwrap();
        assert!(stacks[0].is_git_backed());
    }

    #[test]
    fn tolerates_a_stack_without_env() {
        let json = r#"[{"Id": 1, "Name": "solo", "EndpointId": 1}]"#;
        let stacks: Vec<Stack> = serde_json::from_str(json).unwrap();
        assert!(stacks[0].env.is_empty());
    }

    #[test]
    fn explains_an_unauthorised_response() {
        let msg = describe(
            StatusCode::UNAUTHORIZED,
            r#"{"message":"Invalid JWT token","details":"..."}"#,
        );
        assert!(msg.contains("401"));
        assert!(msg.contains(TOKEN_ENV));
        assert!(msg.contains("Invalid JWT token"));
    }

    #[test]
    fn builds_a_redeploy_body_that_preserves_env() {
        let stack = Stack {
            id: 7,
            name: "gitea".to_string(),
            endpoint_id: 2,
            env: vec![EnvVar {
                name: "TZ".to_string(),
                value: "Europe/Amsterdam".to_string(),
            }],
            git_config: None,
        };
        let body = redeploy_body(&stack, "services:\n  app:\n    image: gitea/gitea:latest\n");

        assert_eq!(body["pullImage"], true);
        assert_eq!(body["prune"], false);
        assert_eq!(body["env"][0]["name"], "TZ");
        assert_eq!(body["env"][0]["value"], "Europe/Amsterdam");
        assert!(body["stackFileContent"]
            .as_str()
            .unwrap()
            .contains("gitea/gitea:latest"));
    }

    #[test]
    fn survives_a_non_json_error_body() {
        let msg = describe(StatusCode::BAD_GATEWAY, "<html>nginx</html>");
        assert!(msg.contains("502"));
    }
}
