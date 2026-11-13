# checkcheck

Meldt welke draaiende Docker-containers een image-update hebben. Vergelijkt de
digest van het lokale image met wat de registry voor die tag serveert.

Elk resultaat wordt twee keer gelezen, met een pauze ertussen. Alleen als beide
metingen het eens zijn telt het. Dat filtert CDN-ruis weg, die bij multi-arch
`:latest`-tags beide kanten op kan liggen.

## Bouwen

```bash
cargo build --release
install -m 755 target/release/checkcheck ~/.local/bin/checkcheck
```

Geen root nodig, mits je gebruiker in de `docker` group zit.

## Gebruik

```bash
checkcheck                    # standaard: twee passes, 5 seconden ertussen
checkcheck --pull             # check, daarna alle updates binnenhalen
checkcheck --delay 0          # snel, één pass
```

```
✅ nginx-test
🔄 sonarr — lscr.io/linuxserver/sonarr:latest, built 2026-09-10 (5 days ago)
🌀 vaultwarden — unstable — treated as CDN noise, not an update
➖ homegrown — no registry digest (built or imported locally)
🔒 pinned — pinned to a digest
❌ paperless — registry rate limit reached (HTTP 429)
```

## Pullen

`--pull` haalt **elke** bevestigde update binnen — alles met 🔄, in één keer.
Er is geen vlag om te selecteren: wat 🌀, ➖, 🔒 of ❌ is blijft met rust.

```
🔄 tautulli — lscr.io/linuxserver/tautulli:latest, built 2026-11-10 (3 days ago) — pulled, restart to adopt
```

Draaien er meerdere containers op hetzelfde image, dan wordt dat image één keer
gepulld. Een mislukte pull stopt de rest niet; die container krijgt
`pull failed: …` achter zijn regel.

Er wordt geen container aangeraakt. Het nieuwe image staat daarna in de lokale
store, de container draait door op het oude tot je hem zelf herstart of de stack
opnieuw uitrolt.

## Beperkingen

- Alleen publieke registries (anonieme token-auth).
- Twee passes verkleinen CDN-ruis, maar sluiten het niet uit.
- Docker Desktop's containerd-store geeft ook lokaal gebouwde images een
  `RepoDigest` onder hun eigen naam. Checkcheck kan ze daardoor niet van
  gepullde images onderscheiden en vraagt ze op bij de registry, wat een 404 of
  een 401 oplevert in plaats van "lokaal gebouwd".

## Structuur

| Bestand | Rol |
|---|---|
| `src/main.rs` | CLI, statusbepaling, rapport, pull |
| `src/docker.rs` | Daemon-verbinding, containers, pull |
| `src/registry.rs` | Registry v2: digests en build-datums |
