# checkcheck — status report (test run on macOS, 2026-09-14)

## Purpose

`checkcheck` is a read-only CLI that reports which **currently running Docker
containers** have a real image update available. It never pulls, recreates,
or restarts anything — purely informational, meant to replace a bash
one-liner the maintainer was using before.

It compares two digests per container:
- **local** — the `RepoDigest` of the image the container is currently running
- **remote** — the digest the registry currently serves for that same tag

To avoid false positives from CDN/replication lag on multi-arch `:latest`
tags (esp. Docker Hub / lscr.io), a difference is only reported as an update
after a **second confirmation pass** a few seconds later returns the same
remote digest again.

## End goal

Run this as a recurring, unattended check (e.g. cron) on the **mini-PC**
that actually hosts the maintainer's production containers (Traefik,
*arr stack, paperless-ngx, vaultwarden, etc.), so they get a simple signal
of "these containers have real updates waiting" without any auto-pull /
auto-restart risk. `--json` + `--exit-code` are there to support that
cron/scripting use case.

## Environment tested so far

- MacBook (Apple Silicon, arm64), Docker Desktop
- Rust via Homebrew, `cargo build`/`cargo run` (debug profile only so far)
- **Not yet tested**: the actual target mini-PC, `cargo build --release`,
  Linux / non-Docker-Desktop socket behavior

## What's confirmed working

- **Project builds and compiles cleanly** on macOS once `main.rs`,
  `docker.rs`, `registry.rs` were moved into `src/` (they were initially
  flat next to `Cargo.toml` — user error, not a code issue).
- **Docker daemon connection works** — `docker::running_containers()`
  successfully lists running containers via the socket without any config
  (Docker Desktop's default `/var/run/docker.sock` symlink is sufficient).
- **`cargo test` passes** — unit tests for image-ref parsing and status
  rendering all green.
- **✅ `UpToDate` path confirmed live** against several real images:
  `nginx:latest`, `nginx:1.24`, `redis:6`, `debian:12-slim`.
- **➖ `NoDigest` path confirmed live** (see open issue below — triggered,
  but possibly not for the intended reason).
- Non-JSON report formatting (icons, padding, summary line) looks correct
  in practice, matches the README's documented example output.

## What's NOT yet confirmed / open issues

1. **🔄 `UpdateAvailable` has never actually fired in a live test.**
   Every real image tried so far (`nginx:latest`, `nginx:1.24`, `redis:6`,
   `debian:12-slim`) came back up to date — likely because these specific
   tags simply haven't been rebuilt recently, not because the detection
   logic is broken (it *is* covered by unit tests with synthetic data, just
   not end-to-end against a real registry). **Need to find/force a real
   stale image to confirm this path fires correctly in practice.**

2. **🌀 `Unstable` (noise-filter) path is completely untested live** — no
   real case where pass 1 and pass 2 disagreed has been observed yet.

3. **❌ `Error` paths are untested live** — rate limiting (429), private
   repo / auth failure, unreachable registry, nonexistent tag. All only
   exercised via unit tests with synthetic status values, not against a
   real registry response.

4. **Unexplained `NoDigest` result for a cross-platform pull.**
   `docker pull --platform linux/amd64 alpine:3.18` on the arm64 Mac
   produced a container that `docker inspect alpine:3.18 --format
   '{{.RepoDigests}}'` shows *does* have a matching RepoDigest
   (`alpine@sha256:de0eb0...`), yet checkcheck reported it as
   `➖ no registry digest on this image`. Working theory: the *container's*
   internal `image_id` (used as the lookup key in `docker.rs` /
   `registry.rs::local_digest`) doesn't match the RepoDigest lookup after a
   forced cross-platform pull — possibly a platform-specific manifest ID
   vs. the manifest-list digest. **Not confirmed as a real bug** (this is
   an unusual manual test scenario, not something that should occur with
   native-arch containers on the mini-PC), but worth a quick look at
   `RunningContainer.image_id` vs `ImageRef.local_digest()` matching logic
   to rule out a real edge case.

5. **Never tested**: `--filter`, `--json` output shape against real data,
   `--exit-code` (exit 10), `--no-color` / `NO_COLOR`, `--delay 0` vs
   default 5s confirmation flow with an actual disagreeing pair, behavior
   with a **pinned-by-digest** container (`image@sha256:...`) in a live run.

6. **`original` field in `ImageRef` is dead code** (compiler warning,
   harmless) — never read anywhere, candidate for removal or for actually
   using it (e.g. in error messages) if it was intended for that.

7. **Not yet run on the actual target host** (the mini-PC) — this Mac test
   only validates the logic and the Docker Desktop socket path; the
   README explicitly flags that a real run on the target machine (correct
   `docker` group membership, correct socket, real production containers
   like lscr.io/ghcr.io images) is still outstanding.

## Suggested next steps

- Find a way to reliably trigger `🔄 UpdateAvailable` in a live test
  (e.g. pull an image, then check if the upstream tag has since been
  rebuilt, or manually retag an older local image under a tag whose
  registry copy is newer).
- Investigate the `alpine:3.18` cross-platform `NoDigest` case to confirm
  whether it's a genuine matching bug in `ImageRef::local_digest()` or
  purely an artifact of the forced `--platform` pull.
- Once 🔄/🌀/❌ paths are confirmed live, move testing to the actual
  mini-PC target with real production containers.
