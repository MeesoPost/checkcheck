# checkcheck

Klein CLI-tool dat per draaiende Docker-container laat zien of er echt een nieuwe
image-versie beschikbaar is. Het doet **niets** met je containers: geen pull, geen
recreate, geen restart. Puur informatief, bedoeld als vervanging van een
bash-eenregelaar.

## Hoe het bepaalt of er een update is

Voor elke container worden twee digests vergeleken:

| | |
|---|---|
| **lokaal** | de `RepoDigest` van de image die de container nu draait (uit `docker inspect`) |
| **remote** | de digest die de registry op dit moment voor die tag serveert |

De remote digest wordt opgehaald met een `HEAD` op het manifest-endpoint van de
registry (`/v2/<repo>/manifests/<tag>`), met anonieme token-auth. Er worden dus
**geen layers gedownload** — geen `docker pull`, geen schijfruimte, geen wachttijd.

De `Accept`-header vraagt expliciet ook om manifest-*lists* (multi-arch indexes),
zodat de remote digest van hetzelfde niveau is als de lokale `RepoDigest`.
Zonder dat zou elke multi-arch image permanent als "update available" verschijnen.

### Ruisfilter

Multi-arch `:latest` tags op Docker Hub en lscr.io kunnen door CDN- of
replicatievertraging kortstondig een afwijkende digest teruggeven zonder dat er
echt een nieuwe versie is. Daarom draait de check in twee passes:

1. **Pass 1** — alle images. Gelijk aan lokaal → ✅ klaar.
2. Verschil gevonden → kandidaat. Na een pauze (`--delay`, standaard 5 s) wordt
   alléén die kandidaat opnieuw bevraagd.
3. **Pass 2** geeft dezelfde remote digest → 🔄 update available.
   Pass 2 geeft de lokale digest of weer iets anders → 🌀 `unstable`, wordt
   **niet** als update gerapporteerd.

Met `--delay 0` sla je pass 2 over (sneller, maar gevoelig voor valse positieven).

## Bouwen

```bash
cargo build --release
install -m 755 target/release/checkcheck ~/.local/bin/checkcheck
```

Geen root nodig, mits je gebruiker in de `docker` group zit (`id -nG | grep docker`).
Standaard doet het tool alleen leesacties op `/var/run/docker.sock`. Alleen met
`--pull` schrijft het: het haalt bevestigde updates binnen in de lokale image-store.
Ook dan wordt geen enkele draaiende container herstart of opnieuw aangemaakt.

Op macOS compileert het prima (handig om te ontwikkelen), maar draaien is alleen
zinvol op de host waar de containers staan. Cross-compilen naar de mini-PC is niet
nodig; bouw daar gewoon met `cargo build --release`.

## Portainer

Met `--redeploy` rolt checkcheck de Portainer-stack opnieuw uit van elke container
met een bevestigde update. Portainer haalt de images daarbij zelf op, dus `--pull`
is geen voorwaarde; samen gebruiken zet het downloaden wél vóór de downtime, wat
de onderbreking korter maakt.

Aanmelden gaat met een API-token (*My account → Access tokens*). Dat token wordt
alleen uit de omgeving gelezen, nooit uit een vlag — argv is op een Linux-host voor
elke gebruiker zichtbaar via `ps`.

```bash
export CHECKCHECK_PORTAINER_URL=https://portainer.local:9443
export CHECKCHECK_PORTAINER_TOKEN=ptr_...
checkcheck --redeploy
```

Bij een self-signed certificaat: `--portainer-insecure`.

Wat checkcheck bewust niet doet:

- Het **vraagt altijd** om bevestiging en toont eerst welke stacks herstarten.
  Zonder terminal (cron, pipe) weigert het — een dienst herstarten terwijl er
  niemand meekijkt hoort een expliciete keuze te zijn.
- Het stuurt het compose-bestand **ongewijzigd** terug. Image-tags worden nooit
  herschreven; het opnieuw ophalen van de tag doet het werk.
- `prune` staat uit, zodat services die toevallig niet in het bestand staan niet
  worden verwijderd.
- **Git-gebaseerde stacks** worden overgeslagen met een melding. Die rollen uit
  vanaf hun repository; terugsturen van het opgeslagen bestand zou de versie in
  git stilletjes kunnen overschrijven.
- Containers die bij geen enkele stack horen worden benoemd, niet aangeraakt.

## Gebruik

```bash
checkcheck                    # standaard: twee passes, 5 seconden ertussen
checkcheck --delay 10         # rustiger ruisfilter
checkcheck --delay 0          # snel, één pass
checkcheck --filter arr       # alleen containers met "arr" in de naam
checkcheck --json             # machineleesbaar
checkcheck --pull             # bevestigde updates binnenhalen (herstart niets)
checkcheck --redeploy         # Portainer-stack opnieuw uitrollen, na bevestiging
checkcheck --exit-code        # exit 10 als er updates zijn (voor scripts/cron)
checkcheck --no-color         # of zet NO_COLOR=1
```

Voorbeeldoutput:

```
checkcheck — 6 running container(s), 5 image(s) to check…
2 possible update(s) — confirming in 5s to filter out registry noise…

✅  traefik      traefik:v3.1
🔄  sonarr       lscr.io/linuxserver/sonarr:latest
      local sha256:9c1f2a0b1d33  →  remote sha256:41ab77e0c2de
❌  paperless    ghcr.io/paperless-ngx/paperless-ngx:latest
      registry rate limit reached (HTTP 429)
🌀  vaultwarden  vaultwarden/server:latest
      registry served two different digests (…) — treated as CDN noise, not an update
➖  homegrown    homegrown-app:dev
      no registry digest on this image (built or imported locally)
🔒  pinned       nginx@sha256:fff…
      pinned to a digest — nothing to compare

🔄 1 update(s) available   ·   ✅ 1 up to date   ·   🌀 1 unstable (ignored)   ·   ➖ 2 not checked   ·   ❌ 1 error(s)
```

| Symbool | Betekenis |
|---|---|
| ✅ | up to date |
| 🔄 | update available (bevestigd in twee passes) |
| 🌀 | registry gaf wisselende digests — genegeerd als ruis |
| 🔒 | container draait een image die op digest is vastgezet |
| ➖ | geen registry-digest lokaal (zelf gebouwd of geïmporteerd) |
| ❌ | fout bij deze container (registry onbereikbaar, rate limit, private repo) |

Een fout bij één container stopt de run niet; de rest wordt gewoon afgemaakt.
Exit code is standaard 0, of 1 als de Docker-daemon zelf onbereikbaar is.

## Beperkingen

- **Alleen anonieme pulls.** Private repositories die inloggen vereisen geven
  `registry refused anonymous access`. Ondersteuning voor `~/.docker/config.json`
  is bewust niet ingebouwd; makkelijk toe te voegen als het ooit nodig is.
- **Docker Hub rate limits** gelden per IP. `HEAD` op een manifest is licht, maar
  bij veel containers kan een 429 voorkomen — die wordt netjes per container gemeld.
- Containers die een image op digest draaien worden overgeslagen; daar valt per
  definitie niets te vergelijken zonder tag.
- Alleen registries over HTTPS.

## Structuur

```
src/main.rs       CLI, twee passes, rapportage (tekst + JSON)
src/docker.rs     bollard: lopende containers + lokale RepoDigests
src/registry.rs   image-ref parsing, anonieme bearer auth, manifest-digest
```

Dependencies bewust beperkt: `bollard`, `reqwest` (rustls), `clap`, `anyhow`,
`serde`/`serde_json`, `sha2`, `tokio`. Kleuren gaan via ANSI-codes zonder extra
crate, met TTY-detectie en `NO_COLOR`-support.

## Testen op de mini-PC

Nog niet live getest — wel gecompileerd en unit-getest (`cargo test`, 7 tests:
image-ref parsing en rendering van alle statussen).

Zinvolle eerste checks daar:

```bash
cargo test                      # parsing + rendering
checkcheck --delay 0            # snelle eerste run
docker pull <een-image>         # daarna: die container moet ✅ worden
checkcheck --json | jq '.summary'
```

Verwachte oplettendheid bij de eerste echte run: of de lokale `RepoDigest` correct
matcht bij images uit lscr.io/ghcr.io, en of er onverwacht veel 🌀 verschijnt (dan is
`--delay` te kort).
