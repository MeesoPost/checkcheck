use anyhow::{bail, Context, Result};
use bollard::container::ListContainersOptions;
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerSummary, CreateImageInfo, ProgressDetail};
use bollard::{Docker, API_DEFAULT_VERSION};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::Write;

/// Label Docker Compose puts on every container it creates; its value is the
/// stack name Portainer shows.
pub const PROJECT_LABEL: &str = "com.docker.compose.project";

#[derive(Debug, Clone)]
pub struct RunningContainer {
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub repo_digests: Vec<String>,
    pub os: String,
    pub architecture: String,
    pub project: Option<String>,
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
        .get(PROJECT_LABEL)
        .filter(|project| !project.is_empty())
        .cloned()
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

/// Hoeveel laagregels het blok hoogstens in-place bijwerkt. De cursor kan niet
/// voorbij de bovenkant van het scherm, dus een onbegrensd blok zou bij een
/// image met veel lagen door zichzelf heen gaan lopen.
const MAX_LAYER_LINES: usize = 20;

/// Breedte van de voortgangsbalk in tekens.
const BAR_WIDTH: usize = 24;

fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let bytes = bytes.max(0);
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Tekent de balk die de daemon zelf niet meestuurt: die geeft alleen
/// `progressDetail`, en de balk in `docker pull` wordt door de CLI gemaakt.
/// Zonder bekend totaal blijft het bij het aantal bytes, want een balk zonder
/// noemer suggereert een voortgang die er niet is.
fn progress_bar(detail: Option<&ProgressDetail>) -> Option<String> {
    let detail = detail?;
    let current = detail.current?;

    let Some(total) = detail.total.filter(|total| *total > 0) else {
        return (current > 0).then(|| human_bytes(current));
    };

    let done = current.clamp(0, total) as f64 / total as f64;
    let filled = (done * BAR_WIDTH as f64).round() as usize;

    let mut bar = String::with_capacity(BAR_WIDTH + 2);
    bar.push('[');
    for slot in 1..=BAR_WIDTH {
        bar.push(
            if slot < filled || (slot == filled && filled == BAR_WIDTH) {
                '='
            } else if slot == filled {
                '>'
            } else {
                ' '
            },
        );
    }
    bar.push(']');

    Some(format!(
        "{bar} {}/{}",
        human_bytes(current),
        human_bytes(total)
    ))
}

/// Bouwt de weergave van `docker pull` na: één regel per laag, die ter plekke
/// wordt herschreven terwijl de daemon voortgang stuurt. Zonder TTY kan dat
/// niet, en valt het terug op één regel per statuswijziging — de
/// voortgangsbalken worden dan overgeslagen.
struct PullRender {
    tty: bool,
    order: Vec<String>,
    lines: HashMap<String, String>,
    drawn: usize,
}

impl PullRender {
    fn new(tty: bool) -> Self {
        Self {
            tty,
            order: Vec::new(),
            lines: HashMap::new(),
            drawn: 0,
        }
    }

    fn update(&mut self, info: &CreateImageInfo) {
        let Some(status) = info.status.as_deref().filter(|s| !s.is_empty()) else {
            return;
        };

        match info.id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => {
                let bar = info
                    .progress
                    .as_deref()
                    .filter(|progress| !progress.is_empty())
                    .map(str::to_string)
                    .or_else(|| progress_bar(info.progress_detail.as_ref()));

                let line = match &bar {
                    Some(bar) => format!("{id}: {status} {bar}"),
                    None => format!("{id}: {status}"),
                };

                if !self.lines.contains_key(id) {
                    self.order.push(id.to_string());
                }
                let previous = self.lines.insert(id.to_string(), line.clone());
                let changed = previous.as_deref() != Some(line.as_str());

                if self.tty {
                    self.redraw();
                } else if changed && bar.is_none() {
                    eprintln!("{line}");
                }
            }
            // Geen id: een bericht over het image als geheel, zoals
            // "Digest: sha256:…" of "Status: Downloaded newer image for …".
            None => {
                self.drawn = 0;
                eprintln!("{status}");
            }
        }
    }

    fn redraw(&mut self) {
        let shown = self.order.len().min(MAX_LAYER_LINES);
        let hidden = self.order.len() - shown;

        let mut out = String::new();
        if self.drawn > 0 {
            out.push_str(&format!("\x1b[{}A", self.drawn));
        }
        for id in &self.order[..shown] {
            out.push_str("\r\x1b[2K");
            out.push_str(self.lines.get(id).map(String::as_str).unwrap_or(""));
            out.push('\n');
        }
        if hidden > 0 {
            out.push_str(&format!("\r\x1b[2K… and {hidden} more layer(s)\n"));
        }

        self.drawn = shown + usize::from(hidden > 0);
        let mut err = std::io::stderr();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }
}

pub async fn pull_image(docker: &Docker, repository: &str, tag: &str, tty: bool) -> Result<()> {
    let options = CreateImageOptions {
        from_image: repository,
        tag,
        ..Default::default()
    };

    let mut render = PullRender::new(tty);
    let mut stream = docker.create_image(Some(options), None, None);

    while let Some(item) = stream.next().await {
        let info = item.with_context(|| format!("pulling {repository}:{tag} failed"))?;
        if let Some(error) = &info.error {
            bail!("pulling {repository}:{tag} failed: {}", error.trim());
        }
        render.update(&info);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: Option<&str>, status: &str, progress: Option<&str>) -> CreateImageInfo {
        CreateImageInfo {
            id: id.map(str::to_string),
            status: Some(status.to_string()),
            progress: progress.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn builds_one_line_per_layer_in_arrival_order() {
        let mut render = PullRender::new(false);
        render.update(&info(
            Some("latest"),
            "Pulling from linuxserver/tautulli",
            None,
        ));
        render.update(&info(Some("a1b2"), "Pulling fs layer", None));
        render.update(&info(Some("c3d4"), "Pulling fs layer", None));
        render.update(&info(
            Some("a1b2"),
            "Downloading",
            Some("[===>  ]  1.2MB/4.5MB"),
        ));
        render.update(&info(Some("a1b2"), "Pull complete", None));

        assert_eq!(
            render.order,
            vec!["latest", "a1b2", "c3d4"],
            "volgorde van binnenkomst, elke laag precies één regel"
        );
        assert_eq!(render.lines["a1b2"], "a1b2: Pull complete");
        assert_eq!(render.lines["c3d4"], "c3d4: Pulling fs layer");
    }

    #[test]
    fn folds_the_progress_bar_into_the_layer_line() {
        let mut render = PullRender::new(false);
        render.update(&info(
            Some("a1b2"),
            "Downloading",
            Some("[==>   ]  1.2MB/4.5MB"),
        ));
        assert_eq!(
            render.lines["a1b2"],
            "a1b2: Downloading [==>   ]  1.2MB/4.5MB"
        );
    }

    #[test]
    fn a_message_without_an_id_closes_the_block() {
        let mut render = PullRender::new(true);
        render.update(&info(Some("a1b2"), "Pull complete", None));
        assert_eq!(render.drawn, 1);

        render.update(&info(
            None,
            "Status: Downloaded newer image for x:latest",
            None,
        ));
        assert_eq!(
            render.drawn, 0,
            "een volgend blok begint onder de al afgedrukte regels"
        );
    }

    #[test]
    fn caps_the_block_so_it_fits_a_terminal() {
        let mut render = PullRender::new(true);
        for i in 0..MAX_LAYER_LINES + 5 {
            render.update(&info(Some(&format!("layer{i}")), "Pulling fs layer", None));
        }
        assert_eq!(
            render.drawn,
            MAX_LAYER_LINES + 1,
            "het maximum aan laagregels plus de regel die de rest samenvat"
        );
    }

    fn detail(id: &str, status: &str, current: i64, total: Option<i64>) -> CreateImageInfo {
        CreateImageInfo {
            id: Some(id.to_string()),
            status: Some(status.to_string()),
            progress_detail: Some(ProgressDetail {
                current: Some(current),
                total,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn draws_its_own_bar_when_the_daemon_only_sends_byte_counts() {
        let mut render = PullRender::new(false);
        render.update(&detail("a1b2", "Downloading", 1_048_576, Some(3_359_301)));
        assert_eq!(
            render.lines["a1b2"],
            "a1b2: Downloading [======>                 ] 1.0MB/3.4MB"
        );
    }

    #[test]
    fn a_finished_layer_fills_the_whole_bar() {
        let mut render = PullRender::new(false);
        render.update(&detail("a1b2", "Downloading", 4_000, Some(4_000)));
        assert_eq!(
            render.lines["a1b2"],
            "a1b2: Downloading [========================] 4.0kB/4.0kB"
        );
    }

    #[test]
    fn falls_back_to_bytes_when_the_total_is_unknown() {
        let mut render = PullRender::new(false);
        render.update(&detail("a1b2", "Downloading", 2_500, None));
        assert_eq!(render.lines["a1b2"], "a1b2: Downloading 2.5kB");
    }

    #[test]
    fn a_bar_is_never_printed_line_by_line_without_a_terminal() {
        let mut render = PullRender::new(false);
        for current in [1_000, 2_000, 3_000] {
            render.update(&detail("a1b2", "Downloading", current, Some(4_000)));
        }
        assert!(
            render.lines["a1b2"].contains("3.0kB/4.0kB"),
            "de regel volgt de voortgang ook zonder terminal"
        );
    }

    #[test]
    fn ignores_an_empty_status() {
        let mut render = PullRender::new(false);
        render.update(&info(Some("a1b2"), "", None));
        assert!(render.order.is_empty());
    }
}
