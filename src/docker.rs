//! Read-only access to the local Docker daemon through `/var/run/docker.sock`.

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
    /// Image reference as used when the container was created.
    pub image: String,
    /// Local image ID (`sha256:...`).
    pub image_id: String,
    pub repo_digests: Vec<String>,
    /// Platform the local image was built for — needed to pick the right
    /// entry out of a multi-arch manifest list when asking the registry for
    /// that same platform's build date.
    pub os: String,
    pub architecture: String,
    /// When the locally cached image was built upstream (RFC 3339), if the
    /// image config records it.
    pub created: Option<String>,
    /// Compose project the container belongs to, i.e. its Portainer stack.
    /// `None` for containers created outside a stack.
    pub project: Option<String>,
}

impl RunningContainer {
    /// Group key: same reference *and* same local image counts as one check.
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}", self.image, self.image_id)
    }
}

#[derive(Debug, Clone, Default)]
struct ImageMeta {
    repo_digests: Vec<String>,
    os: String,
    architecture: String,
    created: Option<String>,
}

/// Open a daemon connection, trying the places a socket actually lives rather
/// than only `/var/run/docker.sock`: `$DOCKER_HOST` wins, and Docker Desktop on
/// macOS puts its socket under `~/.docker` and no longer always symlinks the
/// system path. Every candidate is pinged, because a socket that exists but
/// answers nothing is not a working connection.
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

/// Socket paths worth trying, most specific first. Docker Desktop's own socket
/// comes before the system path: when both exist the system one may be a stale
/// symlink left by an earlier install.
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
        let project = compose_project(&summary);
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
            created: meta.created,
            project,
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

fn compose_project(summary: &ContainerSummary) -> Option<String> {
    summary
        .labels
        .as_ref()?
        .get(crate::portainer::PROJECT_LABEL)
        .filter(|project| !project.is_empty())
        .cloned()
}

/// `summary.image` (from the container list) falls back to a bare image ID
/// once the tag it was created from has been reassigned to a different local
/// image — e.g. a `docker pull` of the same tag without recreating the
/// container. `Config.Image` from a per-container inspect keeps the original
/// reference regardless, so prefer that whenever it's available.
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

/// A failing inspect is not fatal: the image is then reported as "no digest".
async fn image_meta(docker: &Docker, image_id: &str) -> ImageMeta {
    let Some(info) = docker.inspect_image(image_id).await.ok() else {
        return ImageMeta::default();
    };
    ImageMeta {
        repo_digests: info.repo_digests.unwrap_or_default(),
        os: info.os.unwrap_or_default(),
        architecture: info.architecture.unwrap_or_default(),
        created: info.created,
    }
}

/// `docker pull {repository}:{tag}`, nothing more: the new image lands in the
/// local store and every running container keeps running the image it started
/// with. Recreating a container to actually adopt the pull stays a deliberate,
/// separate step.
pub async fn pull_image(docker: &Docker, repository: &str, tag: &str) -> Result<()> {
    let options = CreateImageOptions {
        from_image: repository,
        tag,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        // The daemon reports a failed layer in-band as well as through the
        // transport, so both have to be checked.
        let info = item.with_context(|| format!("pulling {repository}:{tag} failed"))?;
        if let Some(error) = info.error {
            bail!("pulling {repository}:{tag} failed: {}", error.trim());
        }
    }
    Ok(())
}
