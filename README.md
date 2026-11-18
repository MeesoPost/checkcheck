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
regel-per-container rapport. In plaats daarvan zie je de pull zelf, laag voor
laag, zoals `docker pull` hem toont:

```
→ lscr.io/linuxserver/tautulli:latest
latest: Pulling from linuxserver/tautulli
a1b2c3d4e5f6: Already exists
b2c3d4e5f6a7: Downloading [=========>         ]  12.3MB/24.1MB
c3d4e5f6a7b8: Download complete
d4e5f6a7b8c9: Extracting  [===============>   ]  41.2MB/52.7MB
Digest: sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
Status: Downloaded newer image for lscr.io/linuxserver/tautulli:latest

Redeploy in Portainer:
  beszel — beszel, beszel-agent
  media  — plex, tautulli
```

De voortgangsbalken komen kant-en-klaar van de Docker-daemon; checkcheck houdt
alleen bij welke laag op welke regel staat en herschrijft die regel ter plekke.
Een image met veel lagen wordt afgekapt op twintig regels plus een samenvatting,
zodat het blok in een normale terminal past.

Zonder terminal — in een pipe, een cronjob of een logbestand — werkt dat
herschrijven niet, want het gebruikt cursorcodes. Daar krijg je één regel per
statuswijziging en worden de voortgangsbalken overgeslagen.

De voortgang gaat naar stderr en de stacklijst naar stdout, dus
`checkcheck --pull > stacks.txt` levert alleen de lijst op.

Draaien er meerdere containers op hetzelfde image, dan wordt dat image één keer
gepulld. Mislukt een pull, dan verschijnt `failed: …` onder het blok; de rest
gaat gewoon door en de stack blijft op de lijst staan, want Portainer pullt bij
een redeploy zelf.

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
| `src/docker.rs` | Daemon-verbinding, containers, compose-label, pull + voortgang |
| `src/registry.rs` | Registry v2: digests en build-datums |
