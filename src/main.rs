//! checkcheck — report which running Docker containers have a real image
//! update available. It never recreates and never restarts a container; with
//! `--pull` it does fetch confirmed updates into the local image store, so
//! adopting one stays a deliberate, separate step.
//!
//! How it decides:
//!   local  = the RepoDigest of the image the container currently runs
//!   remote = the digest the registry currently serves for that tag
//! Every result needs a second, independent read a few seconds later that
//! agrees with the first before it's trusted — whether the two digests
//! matched or differed. Multi-arch `:latest` tags on Docker Hub / lscr.io
//! can serve a stale digest from a CDN edge for longer than the delay, in
//! either direction: a false "up to date" as easily as a false positive.

mod docker;
mod portainer;
mod registry;

use anyhow::Result;
use clap::Parser;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use docker::RunningContainer;
use portainer::{PortainerClient, Stack};
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

    /// HTTP timeout per registry request
    #[arg(long, default_value_t = 15, value_name = "SECONDS")]
    timeout: u64,

    /// Only check containers whose name contains this substring
    #[arg(long, value_name = "SUBSTRING")]
    filter: Option<String>,

    /// Machine-readable output
    #[arg(long)]
    json: bool,

    /// Pull confirmed updates into the local image store (does not restart anything)
    #[arg(long)]
    pull: bool,

    /// Redeploy the Portainer stack of every confirmed update, after asking
    #[arg(long)]
    redeploy: bool,

    /// Portainer base URL, e.g. https://portainer.local:9443 (or $CHECKCHECK_PORTAINER_URL)
    #[arg(long, value_name = "URL")]
    portainer_url: Option<String>,

    /// Skip TLS verification for Portainer (only for a self-signed certificate)
    #[arg(long)]
    portainer_insecure: bool,

    /// Exit with code 10 when at least one update is available
    #[arg(long)]
    exit_code: bool,

    /// Never colourise the output
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone)]
enum Status {
    Pending,
    UpToDate,
    UpdateAvailable {
        remote: String,
    },
    /// The two passes disagreed — treated as noise, not as an update.
    Unstable {
        first: String,
        second: String,
    },
    /// Container runs an image pinned by digest.
    Pinned,
    /// No registry digest locally (built or imported image).
    NoDigest,
    Error(String),
}

#[derive(Debug, Clone)]
enum Pull {
    Done,
    Failed(String),
}

struct Check {
    parsed: Option<ImageRef>,
    local_digest: Option<String>,
    status: Status,
    /// Pass-1 reading. Held aside rather than folded into `status`: on its own
    /// it proves nothing, matching or not, until pass 2 agrees with it.
    first_remote: Option<String>,
    /// Platform of the locally running image — needed to resolve the right
    /// entry in a multi-arch manifest list when asking the registry for its
    /// build date.
    os: String,
    architecture: String,
    /// When the image currently running was built upstream (RFC 3339).
    local_created: Option<String>,
    /// When the update (if any) was built upstream (RFC 3339). This — not a
    /// locally-tracked "first seen" — answers "since when has this been out of
    /// date" without keeping state between runs.
    remote_created: Option<String>,
    /// Result of `--pull`, `None` when no pull was attempted.
    pulled: Option<Pull>,
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
            local_created: container.created.clone(),
            remote_created: None,
            pulled: None,
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

    let color = !cli.json
        && !cli.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal();

    // Fail on a missing token or URL now rather than after the checks: a
    // misconfigured cron job should complain on its first run, not weeks later
    // when an update finally appears.
    let portainer = cli
        .redeploy
        .then(|| {
            PortainerClient::from_env(
                cli.portainer_url.as_deref(),
                cli.timeout,
                cli.portainer_insecure,
            )
        })
        .transpose()?;

    let docker = docker::connect().await?;
    let mut containers = docker::running_containers(&docker).await?;
    if let Some(needle) = &cli.filter {
        containers.retain(|c| c.name.contains(needle.as_str()));
    }

    if containers.is_empty() {
        if cli.json {
            println!("{}", serde_json::json!({ "containers": [] }));
        } else {
            println!("No running containers to check.");
        }
        return Ok(());
    }

    // One check per unique (image reference, local image id) pair.
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

    if !cli.json {
        eprintln!(
            "checkcheck — {} running container(s), {} image(s) to check…",
            containers.len(),
            to_check
        );
    }

    let mut client = RegistryClient::new(cli.timeout)?;

    for check in checks.iter_mut() {
        if !matches!(check.status, Status::Pending) {
            continue;
        }
        let Some(parsed) = &check.parsed else {
            continue;
        };
        let result = client.manifest_digest(parsed).await;
        match result {
            Ok(remote) => check.first_remote = Some(remote),
            Err(err) => check.status = Status::Error(format!("{err:#}")),
        }
    }

    let read_once: Vec<usize> = checks
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.status, Status::Pending) && c.first_remote.is_some())
        .map(|(i, _)| i)
        .collect();

    if !read_once.is_empty() && cli.delay > 0 {
        if !cli.json {
            eprintln!(
                "{} image(s) checked — confirming in {}s to filter out registry noise…",
                read_once.len(),
                cli.delay
            );
        }
        tokio::time::sleep(Duration::from_secs(cli.delay)).await;
    }

    for i in read_once {
        let (Some(first), Some(local)) = (
            checks[i].first_remote.clone(),
            checks[i].local_digest.clone(),
        ) else {
            continue;
        };

        if cli.delay == 0 {
            checks[i].status = if first == local {
                Status::UpToDate
            } else {
                Status::UpdateAvailable { remote: first }
            };
            continue;
        }

        let Some(parsed) = &checks[i].parsed else {
            continue;
        };
        let result = client.manifest_digest(parsed).await;
        checks[i].status = match result {
            Ok(second) if first == local && second == local => Status::UpToDate,
            Ok(second) if first != local && second == first => {
                Status::UpdateAvailable { remote: first }
            }
            Ok(second) => Status::Unstable { first, second },
            Err(err) => Status::Error(format!("confirmation pass failed: {err:#}")),
        };
    }

    // For confirmed updates, look up when that build was published upstream.
    // Best-effort: a miss here just means the date lines are omitted.
    for check in checks.iter_mut() {
        let (Status::UpdateAvailable { remote }, Some(parsed)) = (&check.status, &check.parsed)
        else {
            continue;
        };
        check.remote_created = client
            .manifest_created(parsed, remote, &check.os, &check.architecture)
            .await;
    }

    if cli.pull {
        pull_updates(&docker, &mut checks, cli.json).await;
    }

    if cli.json {
        print_json(&containers, &index, &checks);
    } else {
        print_report(&containers, &index, &checks, color);
    }

    if let Some(client) = &portainer {
        redeploy_stacks(client, &containers, &index, &checks).await?;
    }

    if cli.exit_code
        && checks
            .iter()
            .any(|c| matches!(c.status, Status::UpdateAvailable { .. }))
    {
        std::process::exit(10);
    }
    Ok(())
}

/// Pull every confirmed update. One pull per image reference even when several
/// containers share it, and a failure is recorded against the check rather than
/// aborting the run — the remaining images are still worth fetching.
async fn pull_updates(docker: &bollard::Docker, checks: &mut [Check], json: bool) {
    let targets: Vec<(usize, String, String)> = checks
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.status, Status::UpdateAvailable { .. }))
        .filter_map(|(i, c)| {
            let parsed = c.parsed.as_ref()?;
            Some((i, parsed.name_no_tag.clone(), parsed.tag.clone()))
        })
        .collect();

    let mut done: HashMap<String, Pull> = HashMap::new();
    for (i, repository, tag) in targets {
        let reference = format!("{repository}:{tag}");
        if !done.contains_key(&reference) {
            if !json {
                eprintln!("pulling {reference}…");
            }
            let outcome = match docker::pull_image(docker, &repository, &tag).await {
                Ok(()) => Pull::Done,
                Err(err) => Pull::Failed(format!("{err:#}")),
            };
            done.insert(reference.clone(), outcome);
        }
        checks[i].pulled = done.get(&reference).cloned();
    }
}

/// One stack that needs redeploying, with the containers that motivated it.
struct Target {
    stack: Stack,
    containers: Vec<String>,
}

/// Redeploy the Portainer stack behind every confirmed update. Portainer pulls
/// the images itself as part of the deploy, so `--pull` is not a prerequisite —
/// running both only moves the download ahead of the downtime window.
async fn redeploy_stacks(
    client: &PortainerClient,
    containers: &[RunningContainer],
    index: &HashMap<String, usize>,
    checks: &[Check],
) -> Result<()> {
    let outdated: Vec<&RunningContainer> = containers
        .iter()
        .filter(|c| {
            matches!(
                checks[index[&c.key()]].status,
                Status::UpdateAvailable { .. }
            )
        })
        .collect();

    if outdated.is_empty() {
        println!("\nNothing to redeploy.");
        return Ok(());
    }

    let stacks = client.stacks().await?;

    let (targets, unmanaged) = plan_redeploy(&outdated, &stacks);

    if !unmanaged.is_empty() {
        println!(
            "\nNot in a Portainer stack, update these yourself: {}",
            unmanaged.join(", ")
        );
    }

    let (git_backed, targets): (Vec<Target>, Vec<Target>) =
        targets.into_iter().partition(|t| t.stack.is_git_backed());
    for target in &git_backed {
        println!(
            "Stack `{}` deploys from git — redeploy it from Portainer instead.",
            target.stack.name
        );
    }

    if targets.is_empty() {
        return Ok(());
    }

    println!("\nAbout to redeploy {} stack(s):", targets.len());
    for target in &targets {
        println!(
            "  {} (stack {}, environment {}) — for {}",
            target.stack.name,
            target.stack.id,
            target.stack.endpoint_id,
            target.containers.join(", ")
        );
    }
    println!("Every container in these stacks restarts. Expect brief downtime.");

    if !confirm("Redeploy? [y/N] ")? {
        println!("Cancelled — nothing was redeployed.");
        return Ok(());
    }

    for target in &targets {
        let name = &target.stack.name;
        print!("redeploying {name}… ");
        std::io::stdout().flush().ok();

        // Send the stack's own file back untouched: the compose file pins no
        // digests, so pulling is what moves it to the new image.
        let result = match client.stack_file(target.stack.id).await {
            Ok(compose) => client.redeploy(&target.stack, &compose).await,
            Err(err) => Err(err),
        };
        match result {
            Ok(()) => println!("done"),
            Err(err) => println!("failed: {err:#}"),
        }
    }
    Ok(())
}

/// Group outdated containers by the stack that owns them, and report the ones
/// no stack claims. Portainer names a stack's compose project after the stack
/// itself, but compose lowercases project names, so the match ignores case.
fn plan_redeploy(outdated: &[&RunningContainer], stacks: &[Stack]) -> (Vec<Target>, Vec<String>) {
    let mut targets: Vec<Target> = Vec::new();
    let mut unmanaged: Vec<String> = Vec::new();

    for container in outdated {
        let stack = container
            .project
            .as_deref()
            .and_then(|project| stacks.iter().find(|s| s.name.eq_ignore_ascii_case(project)));

        match stack {
            Some(stack) => match targets.iter_mut().find(|t| t.stack.id == stack.id) {
                Some(target) => target.containers.push(container.name.clone()),
                None => targets.push(Target {
                    stack: stack.clone(),
                    containers: vec![container.name.clone()],
                }),
            },
            None => unmanaged.push(container.name.clone()),
        }
    }
    (targets, unmanaged)
}

/// Ask on the terminal. Anything other than an interactive answer of `y` is a
/// no, so an unattended run (cron, a pipe) declines instead of restarting
/// services with nobody watching.
fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        println!("Not running on a terminal — refusing to redeploy without confirmation.");
        return Ok(false);
    }
    print!("{prompt}");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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

/// What to print after the container name — `None` for the boring cases
/// (up to date, not checked) so those stay a single bare line; only the
/// statuses worth acting on get anything extra, and never a raw digest.
fn detail_suffix(container: &RunningContainer, check: &Check) -> Option<String> {
    match &check.status {
        Status::UpdateAvailable { .. } => {
            let mut suffix = container.image.clone();
            if let Some(built) = &check.remote_created {
                suffix.push_str(&format!(", built {}{}", short_date(built), age(built)));
            }
            match &check.pulled {
                Some(Pull::Done) => suffix.push_str(" — pulled, restart to adopt"),
                Some(Pull::Failed(err)) => suffix.push_str(&format!(" — pull failed: {err}")),
                None => {}
            }
            Some(suffix)
        }
        Status::Unstable { .. } => {
            Some("unstable — treated as CDN noise, not an update".to_string())
        }
        Status::NoDigest => Some("no registry digest (built or imported locally)".to_string()),
        Status::Pinned => Some("pinned to a digest".to_string()),
        Status::Error(msg) => Some(msg.clone()),
        Status::UpToDate | Status::Pending => None,
    }
}

/// `YYYY-MM-DD` prefix of an RFC 3339 timestamp.
fn short_date(rfc3339: &str) -> &str {
    rfc3339.get(0..10).unwrap_or(rfc3339)
}

/// ` (today)`, ` (3 days ago)`, or empty when the date cannot be read.
fn age(rfc3339: &str) -> String {
    match days_ago(rfc3339) {
        Some(0) => " (today)".to_string(),
        Some(1) => " (1 day ago)".to_string(),
        Some(n) if n > 1 => format!(" ({n} days ago)"),
        _ => String::new(),
    }
}

/// Whole days between an RFC 3339 timestamp's date and today (UTC). Howard
/// Hinnant's `days_from_civil`, so one field needs no date crate.
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

/// Every status already has a distinct icon and text label, so colour is never
/// the only signal. `dim` (ANSI `2`) is deliberately avoided: terminals render
/// it as a faint grey that regularly fails to stay legible on light themes.
fn icon_for(status: &Status) -> (&'static str, &'static str) {
    match status {
        Status::UpToDate => ("✅", "1;32"),
        Status::UpdateAvailable { .. } => ("🔄", "1"),
        Status::Unstable { .. } => ("🌀", ""),
        Status::Pinned => ("🔒", ""),
        Status::NoDigest | Status::Pending => ("➖", ""),
        Status::Error(_) => ("❌", "1;31"),
    }
}

fn print_json(containers: &[RunningContainer], index: &HashMap<String, usize>, checks: &[Check]) {
    let rows: Vec<serde_json::Value> = containers
        .iter()
        .map(|container| {
            let check = &checks[index[&container.key()]];
            let (status, remote, detail) = match &check.status {
                Status::UpToDate => ("up_to_date", None, None),
                Status::UpdateAvailable { remote } => {
                    ("update_available", Some(remote.clone()), None)
                }
                Status::Unstable { first, second } => (
                    "unstable",
                    Some(first.clone()),
                    Some(format!("second pass returned {second}")),
                ),
                Status::Pinned => ("pinned", None, None),
                Status::NoDigest => ("no_digest", None, None),
                Status::Pending => ("not_checked", None, None),
                Status::Error(msg) => ("error", None, Some(msg.clone())),
            };

            serde_json::json!({
                "container": container.name,
                "image": container.image,
                "image_id": container.image_id,
                "status": status,
                "local_digest": check.local_digest,
                "remote_digest": remote,
                "local_built_at": check.local_created,
                "remote_built_at": check.remote_created,
                "pulled": match &check.pulled {
                    Some(Pull::Done) => Some(true),
                    Some(Pull::Failed(_)) => Some(false),
                    None => None,
                },
                "pull_error": match &check.pulled {
                    Some(Pull::Failed(err)) => Some(err.clone()),
                    _ => None,
                },
                "detail": detail,
            })
        })
        .collect();

    let updates = rows
        .iter()
        .filter(|r| r["status"] == "update_available")
        .count();

    let output = serde_json::json!({
        "checked_at_unix": unix_now().unwrap_or(0),
        "containers": rows,
        "summary": { "update_available": updates },
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&output).unwrap_or_default()
    );
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

    fn stack(id: i64, name: &str, git: bool) -> Stack {
        Stack {
            id,
            name: name.to_string(),
            endpoint_id: 2,
            env: Vec::new(),
            git_config: git.then(|| serde_json::json!({"URL": "https://example.invalid"})),
        }
    }

    fn in_project(name: &str, project: Option<&str>) -> RunningContainer {
        RunningContainer {
            project: project.map(str::to_string),
            ..container(name, "nginx:latest", "sha256:aaa")
        }
    }

    #[test]
    fn groups_containers_by_their_stack() {
        let stacks = vec![stack(7, "media", false), stack(8, "gitea", false)];
        let containers = [
            in_project("sonarr", Some("media")),
            in_project("radarr", Some("media")),
            in_project("gitea", Some("gitea")),
        ];
        let refs: Vec<&RunningContainer> = containers.iter().collect();

        let (targets, unmanaged) = plan_redeploy(&refs, &stacks);
        assert!(unmanaged.is_empty());
        assert_eq!(targets.len(), 2, "two stacks, one redeploy each");
        assert_eq!(targets[0].containers, vec!["sonarr", "radarr"]);
        assert_eq!(targets[1].containers, vec!["gitea"]);
    }

    /// Compose lowercases project names, so a stack named `Media` in Portainer
    /// still has to match a `media` label.
    #[test]
    fn matches_a_stack_name_case_insensitively() {
        let stacks = vec![stack(7, "Media", false)];
        let containers = [in_project("sonarr", Some("media"))];
        let refs: Vec<&RunningContainer> = containers.iter().collect();

        let (targets, unmanaged) = plan_redeploy(&refs, &stacks);
        assert!(unmanaged.is_empty());
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn reports_containers_that_no_stack_claims() {
        let stacks = vec![stack(7, "media", false)];
        let containers = [
            in_project("loose", None),
            in_project("other", Some("not-a-portainer-stack")),
        ];
        let refs: Vec<&RunningContainer> = containers.iter().collect();

        let (targets, unmanaged) = plan_redeploy(&refs, &stacks);
        assert!(targets.is_empty());
        assert_eq!(unmanaged, vec!["loose", "other"]);
    }

    #[test]
    fn keeps_git_backed_stacks_separable() {
        let stacks = vec![stack(7, "media", true)];
        let containers = [in_project("sonarr", Some("media"))];
        let refs: Vec<&RunningContainer> = containers.iter().collect();

        let (targets, _) = plan_redeploy(&refs, &stacks);
        assert!(targets[0].stack.is_git_backed());
    }

    fn container(name: &str, image: &str, id: &str) -> RunningContainer {
        RunningContainer {
            name: name.to_string(),
            image: image.to_string(),
            image_id: id.to_string(),
            repo_digests: Vec::new(),
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            created: Some("2026-06-01T12:00:00Z".to_string()),
            project: None,
        }
    }

    /// Renders one of every status so the layout can be eyeballed with
    /// `cargo test -- --nocapture`, and so a missing index would panic here
    /// instead of in production.
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
            Status::Unstable {
                first: "sha256:2222222222222222".to_string(),
                second: "sha256:3333333333333333".to_string(),
            },
            Status::NoDigest,
            Status::Pinned,
        ];

        // Only the second container carries an update, so only it can show a
        // pull outcome; the rest exercise the `None` path.
        let mut pulls = [None, Some(Pull::Done), None, None, None, None].into_iter();

        let mut index = HashMap::new();
        let mut checks = Vec::new();
        for (i, (c, status)) in containers.iter().zip(statuses).enumerate() {
            index.insert(c.key(), i);
            let mut check = Check::for_container(c);
            check.status = status;
            check.local_digest = Some("sha256:0000000000000000".to_string());
            check.remote_created = Some("2026-09-10T08:00:00Z".to_string());
            check.pulled = pulls.next().flatten();
            checks.push(check);
        }

        print_report(&containers, &index, &checks, false);
        print_json(&containers, &index, &checks);
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
        // Reverse of `days_ago`, only accurate enough for the round-trip test.
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
