# checkcheck

Meldt welke draaiende Docker-containers een image-update hebben, en welke
Portainer-stacks je daarvoor opnieuw moet uitrollen. Vergelijkt de digest van
het lokale image met wat de registry voor die tag serveert.

Elk resultaat wordt twee keer gelezen, met een pauze ertussen. Alleen als beide
metingen het eens zijn telt het. Dat filtert CDN-ruis weg, die bij multi-arch
`:latest`-tags beide kanten op kan liggen.

## Bouwen

```bash
cargo build --release
install -Dm755 target/release/checkcheck ~/.local/bin/checkcheck
```

Geen root nodig, mits je gebruiker in de `docker` group zit.

## Gebruik

```bash
checkcheck                    # standaard: twee passes, 5 seconden ertussen
checkcheck --pull             # pullen, daarna alleen de stacklijst
checkcheck --delay 0          # snel, één pass
```

```
✅ gitea
✅ gluetun
🔄 plex — lscr.io/linuxserver/plex:latest, built 2026-11-09 (4 days ago)
🌀 vaultwarden — unstable — treated as CDN noise, not an update
➖ homegrown — no registry digest (built or imported locally)
🔒 pinned — pinned to a digest
❌ paperless — registry rate limit reached (HTTP 429)
🔄 tautulli — lscr.io/linuxserver/tautulli:latest, built 2026-11-10 (3 days ago)

Redeploy in Portainer:
  media — plex, tautulli
```

## Stacks

De stacknaam komt uit het label `com.docker.compose.project`, dat Docker op elke
container zet die via compose is aangemaakt. Dat is dezelfde naam die Portainer
als stack toont — er wordt niets aan de Portainer-API gevraagd.

Eén verouderde container betekent dat je de hele stack opnieuw uitrolt. Een
stack staat er daarom één keer, met al zijn verouderde leden erachter. Staat een
container in geen enkele stack, dan krijgt die een aparte regel:

```
Not in a stack, restart these yourself: homegrown
```

## Pullen

`--pull` haalt **elke** bevestigde update binnen — alles met 🔄, in één keer. Er
is geen vlag om te selecteren: wat 🌀, ➖, 🔒 of ❌ is blijft met rust.

Omdat je daarna toch naar Portainer gaat, vervalt bij `--pull` het
regel-per-container rapport. Je ziet de pulls en wat je moet redeployen:

```
pulling henrygd/beszel:latest… ok
pulling henrygd/beszel-agent:latest… ok
pulling lscr.io/linuxserver/plex:latest… ok
pulling lscr.io/linuxserver/tautulli:latest… failed: registry rate limit reached (HTTP 429)

Redeploy in Portainer:
  beszel — beszel, beszel-agent
  media  — plex, tautulli
```

Draaien er meerdere containers op hetzelfde image, dan wordt dat image één keer
gepulld. Een mislukte pull stopt de rest niet en haalt de stack niet uit de
lijst: Portainer pullt bij een redeploy zelf.

Er wordt geen container aangeraakt. Het nieuwe image staat daarna in de lokale
store, de container draait door op het oude tot je de stack opnieuw uitrolt.
Ruim oude images af en toe op met `docker image prune`.

## Beperkingen

- Alleen publieke registries (anonieme token-auth).
- Twee passes verkleinen CDN-ruis, maar sluiten het niet uit.
- Docker Desktop's containerd-store geeft ook lokaal gebouwde images een
  `RepoDigest` onder hun eigen naam. Checkcheck kan ze daardoor niet van
  gepullde images onderscheiden en vraagt ze op bij de registry, wat een 404 of
  een 401 oplevert in plaats van "lokaal gebouwd".
- De stacklijst zegt welke stacks een redeploy nodig hebben, niet of die
  redeploy daarna geslaagd is. Dat doe je zelf in Portainer.

## Structuur

| Bestand | Rol |
|---|---|
| `src/main.rs` | CLI, statusbepaling, rapport, pull, stackgroepering |
| `src/docker.rs` | Daemon-verbinding, containers, compose-label, pull |
| `src/registry.rs` | Registry v2: digests en build-datums |
