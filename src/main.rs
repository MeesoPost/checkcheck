mod docker;
mod registry;

use anyhow::Result;
use clap::Parser;
use futures_util::stream::{self, StreamExt};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use docker::RunningContainer;
use registry::{ImageRef, RegistryClient};

#[derive(Parser, Debug)]
#[command(
    name = "checkcheck",
    version,
    about = "Report which running Docker containers have an image update available"
)]
struct Cli {
    /// Seconds between the first and the confirmation pass (0 skips confirmation)
    #[arg(long, default_value_t = 5, value_name = "SECONDS")]
    delay: u64,

    /// Registry requests to keep in flight at once (lower this on HTTP 429)
    #[arg(long, default_value_t = 8, value_name = "N")]
    concurrency: usize,

    /// HTTP timeout per registry request
    #[arg(long, default_value_t = 15, value_name = "SECONDS")]
    timeout: u64,

    /// Pull confirmed updates into the local image store (does not restart anything)
    #[arg(long)]
    pull: bool,

    /// Never colourise the output
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone)]
enum Status {
    Pending,
    UpToDate,
    UpdateAvailable { remote: String },
    Unstable,
    Pinned,
    NoDigest,
    Error(String),
}

struct Check {
    parsed: Option<ImageRef>,
    local_digest: Option<String>,
    status: Status,
    first_remote: Option<String>,
    os: String,
    architecture: String,
    remote_created: Option<String>,
}

impl Check {
    fn new(
        container: &RunningContainer,
        parsed: Option<ImageRef>,
        local_digest: Option<String>,
        status: Status,
    ) -> Self {
        Self {
            parsed,
            local_digest,
            status,
            first_remote: None,
            os: container.os.clone(),
            architecture: container.architecture.clone(),
            remote_created: None,
        }
    }

    fn for_container(container: &RunningContainer) -> Self {
        match registry::parse(&container.image) {
            Ok(parsed) if parsed.pinned_digest.is_some() => {
                Check::new(container, Some(parsed), None, Status::Pinned)
            }
            Ok(parsed) => match parsed.local_digest(&container.repo_digests) {
                Some(local) => Check::new(container, Some(parsed), Some(local), Status::Pending),
                None => Check::new(container, Some(parsed), None, Status::NoDigest),
            },
            Err(err) => Check::new(container, None, None, Status::Error(format!("{err:#}"))),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("checkcheck: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let color =
        !cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();

    let docker = docker::connect().await?;
    let containers = docker::running_containers(&docker).await?;

    if containers.is_empty() {
        println!("No running containers to check.");
        return Ok(());
    }

    let mut index: HashMap<String, usize> = HashMap::new();
    let mut checks: Vec<Check> = Vec::new();

    for container in &containers {
        if let Entry::Vacant(slot) = index.entry(container.key()) {
            slot.insert(checks.len());
            checks.push(Check::for_container(container));
        }
    }

    let to_check = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Pending))
        .count();

    eprintln!(
        "checkcheck — {} running container(s), {} image(s) to check…",
        containers.len(),
        to_check
    );

    let client = RegistryClient::new(cli.timeout)?;

    let limit = cli.concurrency.max(1);

    let jobs: Vec<(usize, ImageRef)> = checks
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.status, Status::Pending))
        .filter_map(|(i, c)| c.parsed.clone().map(|parsed| (i, parsed)))
        .collect();

    let results: Vec<(usize, Result<String>)> = stream::iter(jobs)
        .map(|(i, parsed)| {
            let client = &client;
            async move { (i, client.manifest_digest(&parsed).await) }
        })
        .buffer_unordered(limit)
        .collect()
        .await;

    for (i, result) in results {
        match result {
            Ok(remote) => checks[i].first_remote = Some(remote),
            Err(err) => checks[i].status = Status::Error(format!("{err:#}")),
        }
    }

    let read_once: Vec<usize> = checks
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.status, Status::Pending) && c.first_remote.is_some())
        .map(|(i, _)| i)
        .collect();

    if !read_once.is_empty() && cli.delay > 0 {
        eprintln!(
            "{} image(s) checked — confirming in {}s to filter out registry noise…",
            read_once.len(),
            cli.delay
        );
        tokio::time::sleep(Duration::from_secs(cli.delay)).await;
    }

    if cli.delay == 0 {
        for i in read_once {
            let (Some(first), Some(local)) = (
                checks[i].first_remote.clone(),
                checks[i].local_digest.clone(),
            ) else {
                continue;
            };
            checks[i].status = if first == local {
                Status::UpToDate
            } else {
                Status::UpdateAvailable { remote: first }
            };
        }
    } else {
        let jobs: Vec<(usize, ImageRef, String, String)> = read_once
            .into_iter()
            .filter_map(|i| {
                let check = &checks[i];
                Some((
                    i,
                    check.parsed.clone()?,
                    check.first_remote.clone()?,
                    check.local_digest.clone()?,
                ))
            })
            .collect();

        let results: Vec<(usize, String, String, Result<String>)> = stream::iter(jobs)
            .map(|(i, parsed, first, local)| {
                let client = &client;
                async move { (i, first, local, client.manifest_digest(&parsed).await) }
            })
            .buffer_unordered(limit)
            .collect()
            .await;

        for (i, first, local, result) in results {
            checks[i].status = match result {
                Ok(second) if first == local && second == local => Status::UpToDate,
                Ok(second) if first != local && second == first => {
                    Status::UpdateAvailable { remote: first }
                }
                Ok(_) => Status::Unstable,
                Err(err) => Status::Error(format!("confirmation pass failed: {err:#}")),
            };
        }
    }

    let jobs: Vec<(usize, ImageRef, String, String, String)> = checks
        .iter()
        .enumerate()
        .filter_map(|(i, check)| {
            let Status::UpdateAvailable { remote } = &check.status else {
                return None;
            };
            Some((
                i,
                check.parsed.clone()?,
                remote.clone(),
                check.os.clone(),
                check.architecture.clone(),
            ))
        })
        .collect();

    let results: Vec<(usize, Option<String>)> = stream::iter(jobs)
        .map(|(i, parsed, remote, os, arch)| {
            let client = &client;
            async move {
                (
                    i,
                    client.manifest_created(&parsed, &remote, &os, &arch).await,
                )
            }
        })
        .buffer_unordered(limit)
        .collect()
        .await;

    for (i, created) in results {
        checks[i].remote_created = created;
    }

    if cli.pull {
        pull_updates(&docker, &checks, cli.no_color).await;
    } else {
        print_report(&containers, &index, &checks, color);
    }

    let (stacks, loose) = stacks_to_redeploy(&containers, &index, &checks);
    if stacks.is_empty() && loose.is_empty() {
        if cli.pull {
            println!("Everything up to date — nothing to pull.");
        }
    } else {
        print_stacks(&stacks, &loose, color);
    }
    Ok(())
}

async fn pull_updates(docker: &bollard::Docker, checks: &[Check], no_color: bool) {
    // Het laag-voor-laag blok herschrijft regels met cursorcodes; dat kan
    // alleen op een echte terminal. Kleur volgt dezelfde stroom, want het blok
    // gaat naar stderr en het rapport naar stdout.
    let tty = std::io::stderr().is_terminal();
    let color = tty && !no_color && std::env::var_os("NO_COLOR").is_none();
    let mut seen: HashSet<String> = HashSet::new();

    for check in checks {
        if !matches!(check.status, Status::UpdateAvailable { .. }) {
            continue;
        }
        let Some(parsed) = &check.parsed else {
            continue;
        };

        let reference = format!("{}:{}", parsed.name_no_tag, parsed.tag);
        if !seen.insert(reference.clone()) {
            continue;
        }

        eprintln!("\n→ {reference}");
        if let Err(err) =
            docker::pull_image(docker, &parsed.name_no_tag, &parsed.tag, tty, color).await
        {
            eprintln!("failed: {err:#}");
        }
    }
}

/// Groups the confirmed updates by their Compose project — the name Portainer
/// shows as the stack. One outdated container means the whole stack gets
/// redeployed, so every stack appears once with all its outdated members.
fn stacks_to_redeploy(
    containers: &[RunningContainer],
    index: &HashMap<String, usize>,
    checks: &[Check],
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let mut stacks: Vec<(String, Vec<String>)> = Vec::new();
    let mut loose: Vec<String> = Vec::new();

    for container in containers {
        if !matches!(
            checks[index[&container.key()]].status,
            Status::UpdateAvailable { .. }
        ) {
            continue;
        }
        match &container.project {
            Some(project) => match stacks.iter_mut().find(|(name, _)| name == project) {
                Some((_, members)) => members.push(container.name.clone()),
                None => stacks.push((project.clone(), vec![container.name.clone()])),
            },
            None => loose.push(container.name.clone()),
        }
    }

    stacks.sort_by(|a, b| a.0.cmp(&b.0));
    (stacks, loose)
}

fn print_stacks(stacks: &[(String, Vec<String>)], loose: &[String], color: bool) {
    if !stacks.is_empty() {
        println!("\nRedeploy in Portainer:");
        let width = stacks
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        for (name, members) in stacks {
            let pad = " ".repeat(width - name.chars().count());
            println!(
                "  {}{} — {}",
                paint(color, "1", name),
                pad,
                members.join(", ")
            );
        }
    }

    if !loose.is_empty() {
        println!(
            "\nNot in a stack, restart these yourself: {}",
            loose.join(", ")
        );
    }
}

fn print_report(
    containers: &[RunningContainer],
    index: &HashMap<String, usize>,
    checks: &[Check],
    color: bool,
) {
    println!();
    for container in containers {
        let check = &checks[index[&container.key()]];
        let (icon, style) = icon_for(&check.status);
        let name = paint(color, style, &container.name);

        match detail_suffix(container, check) {
            Some(suffix) => println!("{icon} {name} — {suffix}"),
            None => println!("{icon} {name}"),
        }
    }
}

fn detail_suffix(container: &RunningContainer, check: &Check) -> Option<String> {
    match &check.status {
        Status::UpdateAvailable { .. } => {
            let mut suffix = container.image.clone();
            if let Some(built) = &check.remote_created {
                suffix.push_str(&format!(", built {}{}", short_date(built), age(built)));
            }
            Some(suffix)
        }
        Status::Unstable => Some("unstable — treated as CDN noise, not an update".to_string()),
        Status::NoDigest => Some("no registry digest (built or imported locally)".to_string()),
        Status::Pinned => Some("pinned to a digest".to_string()),
        Status::Error(msg) => Some(msg.clone()),
        Status::UpToDate | Status::Pending => None,
    }
}

fn short_date(rfc3339: &str) -> &str {
    rfc3339.get(0..10).unwrap_or(rfc3339)
}

fn age(rfc3339: &str) -> String {
    match days_ago(rfc3339) {
        Some(0) => " (today)".to_string(),
        Some(1) => " (1 day ago)".to_string(),
        Some(n) if n > 1 => format!(" ({n} days ago)"),
        _ => String::new(),
    }
}

fn days_ago(rfc3339: &str) -> Option<i64> {
    let mut parts = rfc3339.get(0..10)?.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;

    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mm = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let target_days = era * 146_097 + doe - 719_468;

    Some(unix_now()? as i64 / 86_400 - target_days)
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn icon_for(status: &Status) -> (&'static str, &'static str) {
    match status {
        Status::UpToDate => ("✅", "1;32"),
        Status::UpdateAvailable { .. } => ("🔄", "1"),
        Status::Unstable => ("🌀", ""),
        Status::Pinned => ("🔒", ""),
        Status::NoDigest | Status::Pending => ("➖", ""),
        Status::Error(_) => ("❌", "1;31"),
    }
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color && !code.is_empty() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, image: &str, id: &str) -> RunningContainer {
        RunningContainer {
            name: name.to_string(),
            image: image.to_string(),
            image_id: id.to_string(),
            repo_digests: Vec::new(),
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            project: None,
        }
    }

    fn in_stack(name: &str, project: Option<&str>) -> RunningContainer {
        RunningContainer {
            project: project.map(str::to_string),
            ..container(name, &format!("{name}:latest"), &format!("sha256:{name}"))
        }
    }

    fn checks_for(
        containers: &[RunningContainer],
        statuses: Vec<Status>,
    ) -> (HashMap<String, usize>, Vec<Check>) {
        let mut index = HashMap::new();
        let mut checks = Vec::new();
        for (i, (c, status)) in containers.iter().zip(statuses).enumerate() {
            index.insert(c.key(), i);
            let mut check = Check::for_container(c);
            check.status = status;
            checks.push(check);
        }
        (index, checks)
    }

    fn update() -> Status {
        Status::UpdateAvailable {
            remote: "sha256:1111111111111111".to_string(),
        }
    }

    #[test]
    fn groups_updates_by_stack() {
        let containers = vec![
            in_stack("plex", Some("media")),
            in_stack("tautulli", Some("media")),
            in_stack("beszel", Some("beszel")),
            in_stack("loose", None),
            in_stack("gitea", Some("gitea")),
        ];
        let (index, checks) = checks_for(
            &containers,
            vec![update(), update(), update(), update(), Status::UpToDate],
        );

        let (stacks, loose) = stacks_to_redeploy(&containers, &index, &checks);

        assert_eq!(
            stacks.len(),
            2,
            "gitea is up to date, so its stack stays out"
        );
        assert_eq!(stacks[0].0, "beszel");
        assert_eq!(stacks[1].0, "media");
        assert_eq!(
            stacks[1].1,
            vec!["plex", "tautulli"],
            "one stack, both outdated members listed"
        );
        assert_eq!(loose, vec!["loose"]);
    }

    #[test]
    fn one_outdated_container_pulls_in_its_whole_stack() {
        let containers = vec![
            in_stack("sonarr", Some("media")),
            in_stack("radarr", Some("media")),
        ];
        let (index, checks) = checks_for(&containers, vec![update(), Status::UpToDate]);

        let (stacks, loose) = stacks_to_redeploy(&containers, &index, &checks);

        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].0, "media");
        assert_eq!(
            stacks[0].1,
            vec!["sonarr"],
            "radarr is current, so not named"
        );
        assert!(loose.is_empty());
    }

    #[test]
    fn renders_every_status() {
        let containers = vec![
            container("traefik", "traefik:v3.1", "sha256:aaa"),
            container("sonarr", "lscr.io/linuxserver/sonarr:latest", "sha256:bbb"),
            container(
                "paperless",
                "ghcr.io/paperless-ngx/paperless-ngx:latest",
                "sha256:ccc",
            ),
            container("vaultwarden", "vaultwarden/server:latest", "sha256:ddd"),
            container("homegrown", "homegrown-app:dev", "sha256:eee"),
            container("pinned", "nginx@sha256:fff", "sha256:fff"),
        ];

        let statuses = vec![
            Status::UpToDate,
            Status::UpdateAvailable {
                remote: "sha256:1111111111111111".to_string(),
            },
            Status::Error("registry rate limit reached (HTTP 429)".to_string()),
            Status::Unstable,
            Status::NoDigest,
            Status::Pinned,
        ];

        let mut index = HashMap::new();
        let mut checks = Vec::new();
        for (i, (c, status)) in containers.iter().zip(statuses).enumerate() {
            index.insert(c.key(), i);
            let mut check = Check::for_container(c);
            check.status = status;
            check.local_digest = Some("sha256:0000000000000000".to_string());
            check.remote_created = Some("2026-09-10T08:00:00Z".to_string());
            checks.push(check);
        }

        print_report(&containers, &index, &checks, false);
    }

    #[test]
    fn days_ago_counts_whole_days() {
        let stamp = rfc3339_today();
        let today = short_date(&stamp);
        assert_eq!(days_ago(today), Some(0));
        assert_eq!(age(today), " (today)");
    }

    #[test]
    fn days_ago_rejects_garbage() {
        assert_eq!(days_ago("not-a-date"), None);
        assert_eq!(age("not-a-date"), "");
    }

    fn rfc3339_today() -> String {
        let mut days = unix_now().unwrap() as i64 / 86_400 + 719_468;
        let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
        days -= era * 146_097;
        let yoe = (days - days / 1460 + days / 36_524 - days / 146_096) / 365;
        let doy = days - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
    }
}

/// Not a unit test: a stopwatch for picking `--concurrency` on a given host,
/// since the right value depends on that machine's latency to the registries it
/// actually uses. Hits the network, so it only runs when asked for by name:
///
///     cargo test --release bench -- --ignored --nocapture
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    const IMAGES: &[&str] = &[
        "nginx:latest",
        "redis:latest",
        "alpine:latest",
        "postgres:16",
        "mariadb:11",
        "traefik:v3.1",
        "busybox:latest",
        "debian:12-slim",
        "python:3.12-slim",
        "node:20-alpine",
        "memcached:alpine",
        "ghcr.io/paperless-ngx/paperless-ngx:latest",
    ];

    async fn time_pass(limit: usize, refs: &[ImageRef]) -> (std::time::Duration, usize) {
        let client = RegistryClient::new(20).unwrap();
        let started = Instant::now();
        let results: Vec<bool> = stream::iter(refs.to_vec())
            .map(|parsed| {
                let client = &client;
                async move { client.manifest_digest(&parsed).await.is_ok() }
            })
            .buffer_unordered(limit)
            .collect()
            .await;
        (started.elapsed(), results.iter().filter(|ok| **ok).count())
    }

    /// Best of two, because a single slow response from any host otherwise
    /// decides the number and the sweep stops being readable.
    async fn best_of_two(limit: usize, refs: &[ImageRef]) -> (std::time::Duration, usize) {
        let first = time_pass(limit, refs).await;
        let second = time_pass(limit, refs).await;
        if first.0 <= second.0 {
            first
        } else {
            second
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn concurrency_sweep() {
        let refs: Vec<ImageRef> = IMAGES.iter().map(|i| registry::parse(i).unwrap()).collect();

        let (baseline, ok) = best_of_two(1, &refs).await;
        println!("concurrency  1: {baseline:>9.2?}  ({ok}/{} ok)", refs.len());

        for limit in [4usize, 8, 16] {
            let (elapsed, ok) = best_of_two(limit, &refs).await;
            println!(
                "concurrency {limit:>2}: {elapsed:>9.2?}  ({ok}/{} ok)  {:.1}x",
                refs.len(),
                baseline.as_secs_f64() / elapsed.as_secs_f64()
            );
        }
        println!();
        println!(
            "Fewer than {}/{} ok means a registry was unwell, and that run's",
            refs.len(),
            refs.len()
        );
        println!("timing says more about that host than about the concurrency level.");
    }
}
