use anyhow::{bail, Context, Result};
use bollard::container::ListContainersOptions;
use bollard::image::CreateImageOptions;
use bollard::models::ContainerSummary;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures_util::StreamExt;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RunningContainer {
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub repo_digests: Vec<String>,
    pub os: String,
    pub architecture: String,
}

impl RunningContainer {
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}", self.image, self.image_id)
    }
}

#[derive(Debug, Clone, Default)]
struct ImageMeta {
    repo_digests: Vec<String>,
    os: String,
    architecture: String,
}

pub async fn connect() -> Result<Docker> {
    let mut failures = Vec::new();

    if std::env::var_os("DOCKER_HOST").is_some() {
        match Docker::connect_with_defaults() {
            Ok(docker) => match docker.ping().await {
                Ok(_) => return Ok(docker),
                Err(err) => failures.push(format!("$DOCKER_HOST: {err}")),
            },
            Err(err) => failures.push(format!("$DOCKER_HOST: {err}")),
        }
    }

    for path in socket_candidates() {
        let connected = Docker::connect_with_socket(&path, 120, API_DEFAULT_VERSION);
        match connected {
            Ok(docker) => match docker.ping().await {
                Ok(_) => return Ok(docker),
                Err(err) => failures.push(format!("{path}: {err}")),
            },
            Err(err) => failures.push(format!("{path}: {err}")),
        }
    }

    bail!(
        "could not reach the Docker daemon. Tried:\n  {}\n\
         Check that Docker is running, that your user is in the `docker` group, \
         and if `docker context ls` points somewhere else, export DOCKER_HOST to match.",
        failures.join("\n  ")
    )
}

fn socket_candidates() -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        paths.push(format!("{home}/.docker/run/docker.sock"));
        paths.push(format!("{home}/.colima/default/docker.sock"));
        paths.push(format!("{home}/.orbstack/run/docker.sock"));
    }
    paths.push("/var/run/docker.sock".to_string());
    paths.retain(|path| std::path::Path::new(path).exists());
    paths
}

pub async fn running_containers(docker: &Docker) -> Result<Vec<RunningContainer>> {
    let options = ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    };
    let summaries = docker
        .list_containers(Some(options))
        .await
        .context("could not list running containers")?;

    let mut meta_cache: HashMap<String, ImageMeta> = HashMap::new();
    let mut containers = Vec::with_capacity(summaries.len());

    for summary in summaries {
        let name = container_name(&summary);
        let image_id = summary.image_id.clone().unwrap_or_default();
        let image = image_reference(docker, &summary).await;
        if image.is_empty() || image_id.is_empty() {
            continue;
        }

        let meta = match meta_cache.get(&image_id) {
            Some(cached) => cached.clone(),
            None => {
                let meta = image_meta(docker, &image_id).await;
                meta_cache.insert(image_id.clone(), meta.clone());
                meta
            }
        };

        containers.push(RunningContainer {
            name,
            image,
            image_id,
            repo_digests: meta.repo_digests,
            os: meta.os,
            architecture: meta.architecture,
        });
    }

    containers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(containers)
}

fn container_name(summary: &ContainerSummary) -> String {
    summary
        .names
        .as_ref()
        .and_then(|names| names.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "<unnamed>".to_string())
}

async fn image_reference(docker: &Docker, summary: &ContainerSummary) -> String {
    let from_inspect = match &summary.id {
        Some(id) => docker
            .inspect_container(id, None)
            .await
            .ok()
            .and_then(|info| info.config)
            .and_then(|config| config.image)
            .filter(|image| !image.is_empty()),
        None => None,
    };
    from_inspect.unwrap_or_else(|| summary.image.clone().unwrap_or_default())
}

async fn image_meta(docker: &Docker, image_id: &str) -> ImageMeta {
    let Some(info) = docker.inspect_image(image_id).await.ok() else {
        return ImageMeta::default();
    };
    ImageMeta {
        repo_digests: info.repo_digests.unwrap_or_default(),
        os: info.os.unwrap_or_default(),
        architecture: info.architecture.unwrap_or_default(),
    }
}

pub async fn pull_image(docker: &Docker, repository: &str, tag: &str) -> Result<()> {
    let options = CreateImageOptions {
        from_image: repository,
        tag,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        let info = item.with_context(|| format!("pulling {repository}:{tag} failed"))?;
        if let Some(error) = info.error {
            bail!("pulling {repository}:{tag} failed: {}", error.trim());
        }
    }
    Ok(())
}
