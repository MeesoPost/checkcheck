use anyhow::{bail, Context, Result};
use bollard::container::ListContainersOptions;
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerSummary, CreateImageInfo};
use bollard::{Docker, API_DEFAULT_VERSION};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

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

/// Breedte van de balk in tekens.
const BAR_WIDTH: usize = 20;

/// Hoe zwaar een verse snelheidsmeting meeweegt. Laag houdt het getal rustig,
/// zodat één trage seconde het niet laat schieten.
const SPEED_SMOOTHING: f64 = 0.3;

/// Kortste tijd tussen twee snelheidsmetingen. De daemon stuurt vaker dan dat,
/// en delen door een paar milliseconden levert onzin op.
const SPEED_INTERVAL: Duration = Duration::from_millis(250);

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

fn bar(filled: f64) -> String {
    let blocks = (filled.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize;
    let mut out = String::with_capacity(BAR_WIDTH * 3);
    out.push_str(&"█".repeat(blocks));
    out.push_str(&"░".repeat(BAR_WIDTH - blocks));
    out
}

/// Wat een laag op dit moment doet. De daemon beschrijft dat in losse
/// zinnetjes; alleen de gevallen die de weergave anders maken staan hier.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Waiting,
    Downloading,
    Verifying,
    Downloaded,
    Extracting,
    Done,
    Cached,
    /// Een melding die niet over een laag gaat, zoals de regel waarmee de
    /// daemon aankondigt uit welke repository hij pullt.
    Other,
}

impl Phase {
    fn of(status: &str) -> Self {
        match status {
            "Pulling fs layer" | "Waiting" | "Retrying" => Phase::Waiting,
            "Downloading" => Phase::Downloading,
            "Verifying Checksum" => Phase::Verifying,
            "Download complete" => Phase::Downloaded,
            "Extracting" => Phase::Extracting,
            "Pull complete" => Phase::Done,
            "Already exists" => Phase::Cached,
            _ => Phase::Other,
        }
    }

    /// Hoe vol de balk staat zonder getallen van de daemon. Een laag die klaar
    /// is weet je vol, een laag die wacht weet je leeg; de rest niet.
    fn implied(self) -> Option<f64> {
        match self {
            Phase::Waiting => Some(0.0),
            Phase::Verifying | Phase::Downloaded | Phase::Done | Phase::Cached => Some(1.0),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Verifying => "verifying",
            Phase::Downloaded => "downloaded",
            Phase::Extracting => "extracting",
            Phase::Done => "done",
            Phase::Cached => "exists",
            Phase::Downloading | Phase::Other => "",
        }
    }

    /// Gedimd zolang er niets gebeurt, cyaan terwijl er bytes bewegen, groen
    /// zodra ze binnen zijn.
    fn color(self) -> &'static str {
        match self {
            Phase::Downloading | Phase::Extracting => "36",
            Phase::Verifying | Phase::Downloaded | Phase::Done | Phase::Cached => "32",
            Phase::Waiting | Phase::Other => "2",
        }
    }
}

#[derive(Default)]
struct Layer {
    phase: Phase,
    status: String,
    current: i64,
    total: i64,
    speed: Option<f64>,
    sample: Option<(Instant, i64)>,
}

impl Layer {
    /// Schat de snelheid uit het verschil met de vorige meting.
    fn observe(&mut self, now: Instant) {
        let Some((taken, bytes)) = self.sample else {
            self.sample = Some((now, self.current));
            return;
        };

        let elapsed = now.saturating_duration_since(taken);
        if elapsed < SPEED_INTERVAL {
            return;
        }

        let rate = (self.current - bytes).max(0) as f64 / elapsed.as_secs_f64();
        self.speed = Some(match self.speed {
            Some(previous) => previous + (rate - previous) * SPEED_SMOOTHING,
            None => rate,
        });
        self.sample = Some((now, self.current));
    }

    /// Een fase die zichzelf verraadt gaat voor op de getallen: de laatste
    /// cijfers van de daemon gaan over het uitpakken en stoppen halverwege,
    /// terwijl de laag wel degelijk klaar is.
    fn filled(&self) -> Option<f64> {
        self.phase.implied().or_else(|| {
            (self.total > 0).then(|| self.current.clamp(0, self.total) as f64 / self.total as f64)
        })
    }

    fn detail(&self) -> String {
        match self.phase {
            Phase::Downloading | Phase::Extracting if self.total > 0 => {
                let trailing = match (self.phase, self.speed) {
                    (Phase::Downloading, Some(speed)) => {
                        format!("   {}/s", human_bytes(speed as i64))
                    }
                    _ => format!("   {}", self.phase.label()),
                };
                format!(
                    "{} / {}{}",
                    human_bytes(self.current),
                    human_bytes(self.total),
                    trailing.trim_end()
                )
            }
            Phase::Downloading | Phase::Extracting => human_bytes(self.current),
            Phase::Downloaded | Phase::Done if self.total > 0 => {
                format!("{}   {}", human_bytes(self.total), self.phase.label())
            }
            _ => self.phase.label().to_string(),
        }
    }

    fn render(&self, id: &str, color: bool) -> String {
        if self.phase == Phase::Other {
            return format!("  {id}: {}", self.status);
        }

        let filled = self.filled();
        let percentage = match filled {
            Some(filled) => format!("{}%", (filled * 100.0).round()),
            None => "—".to_string(),
        };

        let line = format!(
            "  {id:<12}  {}  {percentage:>4}  {}",
            paint(color, self.phase.color(), &bar(filled.unwrap_or(0.0))),
            self.detail()
        );
        line.trim_end().to_string()
    }
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Toont de pull laag voor laag en herschrijft elke regel ter plekke terwijl de
/// daemon voortgang stuurt. Zonder TTY kan dat niet, en valt het terug op één
/// regel per statuswijziging.
struct PullRender {
    tty: bool,
    color: bool,
    order: Vec<String>,
    layers: HashMap<String, Layer>,
    drawn: usize,
}

impl PullRender {
    fn new(tty: bool, color: bool) -> Self {
        Self {
            tty,
            color,
            order: Vec::new(),
            layers: HashMap::new(),
            drawn: 0,
        }
    }

    fn update(&mut self, info: &CreateImageInfo) {
        let Some(status) = info.status.as_deref().filter(|status| !status.is_empty()) else {
            return;
        };

        // Geen id: een bericht over het image als geheel, zoals
        // "Digest: sha256:…" of "Status: Downloaded newer image for …".
        let Some(id) = info.id.as_deref().filter(|id| !id.is_empty()) else {
            self.drawn = 0;
            eprintln!("{status}");
            return;
        };

        if !self.layers.contains_key(id) {
            self.order.push(id.to_string());
        }

        let layer = self.layers.entry(id.to_string()).or_default();
        let changed = layer.status != status;

        layer.phase = Phase::of(status);
        layer.status = status.to_string();
        if let Some(detail) = &info.progress_detail {
            layer.current = detail.current.unwrap_or(layer.current);
            layer.total = detail.total.unwrap_or(layer.total);
        }

        if layer.phase == Phase::Downloading {
            layer.observe(Instant::now());
        } else {
            layer.speed = None;
            layer.sample = None;
        }

        if self.tty {
            self.redraw();
        } else if changed {
            eprintln!("{id}: {status}");
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
            if let Some(layer) = self.layers.get(id) {
                out.push_str(&layer.render(id, self.color));
            }
            out.push('\n');
        }
        if hidden > 0 {
            out.push_str(&format!("\r\x1b[2K  … and {hidden} more layer(s)\n"));
        }

        self.drawn = shown + usize::from(hidden > 0);
        let mut err = std::io::stderr();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }
}

pub async fn pull_image(
    docker: &Docker,
    repository: &str,
    tag: &str,
    tty: bool,
    color: bool,
) -> Result<()> {
    let options = CreateImageOptions {
        from_image: repository,
        tag,
        ..Default::default()
    };

    let mut render = PullRender::new(tty, color);
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
    use bollard::models::ProgressDetail;

    fn plain(id: &str, status: &str) -> CreateImageInfo {
        CreateImageInfo {
            id: Some(id.to_string()),
            status: Some(status.to_string()),
            ..Default::default()
        }
    }

    fn sized(id: &str, status: &str, current: i64, total: Option<i64>) -> CreateImageInfo {
        CreateImageInfo {
            progress_detail: Some(ProgressDetail {
                current: Some(current),
                total,
            }),
            ..plain(id, status)
        }
    }

    fn line(render: &PullRender, id: &str) -> String {
        render.layers[id].render(id, false)
    }

    #[test]
    fn keeps_one_line_per_layer_in_arrival_order() {
        let mut render = PullRender::new(false, false);
        render.update(&plain("latest", "Pulling from linuxserver/tautulli"));
        render.update(&plain("a1b2", "Pulling fs layer"));
        render.update(&plain("c3d4", "Pulling fs layer"));
        render.update(&sized("a1b2", "Downloading", 1_200_000, Some(4_500_000)));
        render.update(&plain("a1b2", "Pull complete"));

        assert_eq!(render.order, vec!["latest", "a1b2", "c3d4"]);
    }

    #[test]
    fn draws_a_block_bar_with_percentage_and_bytes() {
        let mut render = PullRender::new(false, false);
        render.update(&sized(
            "b2c3d4e5",
            "Downloading",
            22_100_000,
            Some(38_100_000),
        ));
        assert_eq!(
            line(&render, "b2c3d4e5"),
            "  b2c3d4e5      ████████████░░░░░░░░   58%  22.1MB / 38.1MB"
        );
    }

    #[test]
    fn a_finished_layer_is_full_and_labelled() {
        let mut render = PullRender::new(false, false);
        render.update(&sized(
            "a1b2c3d4",
            "Downloading",
            4_100_000,
            Some(4_100_000),
        ));
        render.update(&plain("a1b2c3d4", "Pull complete"));
        assert_eq!(
            line(&render, "a1b2c3d4"),
            "  a1b2c3d4      ████████████████████  100%  4.1MB   done"
        );
    }

    #[test]
    fn a_completed_layer_is_full_despite_stale_extract_numbers() {
        let mut render = PullRender::new(false, false);
        render.update(&sized("a1b2", "Downloading", 38_100_000, Some(38_100_000)));
        render.update(&sized("a1b2", "Extracting", 1, Some(38_100_000)));
        render.update(&plain("a1b2", "Pull complete"));
        assert_eq!(
            line(&render, "a1b2"),
            "  a1b2          ████████████████████  100%  38.1MB   done"
        );
    }

    #[test]
    fn a_layer_that_was_already_there_needs_no_numbers() {
        let mut render = PullRender::new(false, false);
        render.update(&plain("c3d4e5f6", "Already exists"));
        assert_eq!(
            line(&render, "c3d4e5f6"),
            "  c3d4e5f6      ████████████████████  100%  exists"
        );
    }

    #[test]
    fn a_waiting_layer_shows_an_empty_bar() {
        let mut render = PullRender::new(false, false);
        render.update(&plain("d4e5f6a7", "Pulling fs layer"));
        assert_eq!(
            line(&render, "d4e5f6a7"),
            "  d4e5f6a7      ░░░░░░░░░░░░░░░░░░░░    0%  waiting"
        );
    }

    #[test]
    fn an_unknown_total_leaves_the_percentage_blank() {
        let mut render = PullRender::new(false, false);
        render.update(&sized("e5f6a7b8", "Downloading", 2_500, None));
        assert_eq!(
            line(&render, "e5f6a7b8"),
            "  e5f6a7b8      ░░░░░░░░░░░░░░░░░░░░     —  2.5kB"
        );
    }

    #[test]
    fn a_known_total_survives_an_update_without_one() {
        let mut render = PullRender::new(false, false);
        render.update(&sized("a1b2", "Downloading", 1_000, Some(4_000)));
        render.update(&plain("a1b2", "Verifying Checksum"));
        assert_eq!(
            render.layers["a1b2"].total, 4_000,
            "een statuswijziging zonder getallen mag het totaal niet wissen"
        );
    }

    #[test]
    fn progress_never_adds_a_second_line_for_the_same_layer() {
        let mut render = PullRender::new(false, false);
        for current in [1_000, 2_000, 3_000, 4_000] {
            render.update(&sized("a1b2", "Downloading", current, Some(4_000)));
        }
        assert_eq!(render.order, vec!["a1b2"]);
        assert!(line(&render, "a1b2").contains("100%"));
    }

    #[test]
    fn a_message_without_an_id_closes_the_block() {
        let mut render = PullRender::new(true, false);
        render.update(&plain("a1b2", "Pull complete"));
        assert_eq!(render.drawn, 1);

        render.update(&CreateImageInfo {
            status: Some("Status: Downloaded newer image for x:latest".to_string()),
            ..Default::default()
        });
        assert_eq!(
            render.drawn, 0,
            "een volgend blok begint onder de al afgedrukte regels"
        );
    }

    #[test]
    fn caps_the_block_so_it_fits_a_terminal() {
        let mut render = PullRender::new(true, false);
        for i in 0..MAX_LAYER_LINES + 5 {
            render.update(&plain(&format!("layer{i}"), "Pulling fs layer"));
        }
        assert_eq!(
            render.drawn,
            MAX_LAYER_LINES + 1,
            "het maximum aan laagregels plus de regel die de rest samenvat"
        );
    }

    #[test]
    fn ignores_an_empty_status() {
        let mut render = PullRender::new(false, false);
        render.update(&plain("a1b2", ""));
        assert!(render.order.is_empty());
    }

    #[test]
    fn colour_wraps_only_the_bar() {
        let mut render = PullRender::new(false, true);
        render.update(&plain("a1b2", "Already exists"));
        let rendered = render.layers["a1b2"].render("a1b2", true);
        assert!(rendered.contains("\x1b[32m████"));
        assert!(rendered.ends_with("\x1b[0m  100%  exists"));
    }

    #[test]
    fn human_bytes_switches_unit_at_a_thousand() {
        assert_eq!(human_bytes(999), "999B");
        assert_eq!(human_bytes(1_000), "1.0kB");
        assert_eq!(human_bytes(38_100_000), "38.1MB");
    }
}
