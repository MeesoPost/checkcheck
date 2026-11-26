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
checkcheck --concurrency 16   # meer registry-verzoeken tegelijk
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
  a1b2c3d4e5f6  ████████████████████  100%  exists
  b2c3d4e5f6a7  ████████████░░░░░░░░   58%  22.1MB / 38.1MB   7.0MB/s
  c3d4e5f6a7b8  ███░░░░░░░░░░░░░░░░░   14%  1.2MB / 8.7MB   2.1MB/s
  d4e5f6a7b8c9  ░░░░░░░░░░░░░░░░░░░░    0%  waiting
Digest: sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
Status: Downloaded newer image for lscr.io/linuxserver/tautulli:latest

Redeploy in Portainer:
  beszel — beszel, beszel-agent
  media  — plex, tautulli
```

De daemon stuurt geen kant-en-klare balken mee, alleen het aantal gedownloade
en te downloaden bytes per laag; de balk in `docker pull` wordt door de Docker
CLI zelf getekend. Checkcheck tekent zijn eigen balk uit die getallen en
herschrijft de regel van die laag ter plekke. De snelheid is een lopend
gemiddelde, zodat één trage seconde het getal niet laat schieten.

De balk schaalt mee met je terminal: minimaal twintig tekens, maximaal zestig.
Dat maximum is geen smaakkwestie — een regel die breder is dan het venster
breekt af op een tweede regel, en dan klopt de cursor-rekensom van het blok niet
meer en loopt het beeld door zichzelf heen. De breedte wordt één keer bij de
start bepaald, zodat het blok niet gaat schuiven als je het venster versleept.

De kleur zegt wat er gebeurt: gedimd betekent dat de laag nog wacht, cyaan dat
er bytes bewegen, groen dat ze binnen zijn. Met `--no-color` of de
omgevingsvariabele `NO_COLOR` blijft het bij blokken zonder kleur.

Is het totaal onbekend, dan staat er een streepje in plaats van een percentage
en alleen het aantal binnengehaalde bytes — een balk zonder noemer suggereert
een voortgang die je niet kent. Een laag die klaar is staat altijd vol, ook als
de laatste getallen van de daemon over het uitpakken gingen. Een image met veel
lagen wordt afgekapt op twintig regels plus een samenvatting, zodat het blok in
een normale terminal past.

Zonder terminal — in een pipe, een cronjob of een logbestand — werkt dat
herschrijven niet, want het gebruikt cursorcodes. Daar krijg je één regel per
statuswijziging en worden de voortgangsbalken overgeslagen, zodat een logbestand
niet volloopt met honderden tussenstanden per laag.

De voortgang gaat naar stderr en de stacklijst naar stdout, dus
`checkcheck --pull > stacks.txt` levert alleen de lijst op.

Draaien er meerdere containers op hetzelfde image, dan wordt dat image één keer
gepulld. Mislukt een pull, dan verschijnt `failed: …` onder het blok; de rest
gaat gewoon door en de stack blijft op de lijst staan, want Portainer pullt bij
een redeploy zelf.

Er wordt geen container aangeraakt. Het nieuwe image staat daarna in de lokale
store, de container draait door op het oude tot je de stack opnieuw uitrolt.
Ruim oude images af en toe op met `docker image prune`.

## Snelheid

De registry-verzoeken gaan parallel, standaard 8 tegelijk. Dat scheelt niet
alleen tijd, het maakt de looptijd ook ongevoelig voor één trage registry: die
overlapt dan met de rest in plaats van bij het totaal op te tellen.

Gemeten over 12 images, waarvan één op een registry die 502 gaf:

| Tegelijk | Tijd |
|---|---|
| 1 (zoals voorheen) | 3,6 s – 16 s, afhankelijk van de traagste host |
| 8 (standaard) | ~1,1 s |
| 16 | ~0,8 s |

Een 5xx wordt twee keer opnieuw geprobeerd met een korte pauze; registries die
achter een CDN zitten geven die in vlagen af. Blijft hij falen, dan staat er
`registry is unavailable` en is de registry zelf aan de beurt, niet checkcheck.

Hoger kan sneller zijn, maar vergroot de kans op een HTTP 429 van de registry.
Krijg je die, zet `--concurrency` dan lager. Het juiste getal hangt af van de
latency van de machine naar de registries die je gebruikt; meet het met:

```bash
cargo test --release bench -- --ignored --nocapture
```

Wat overblijft is de vaste `--delay` tussen de twee passes. Dat is wachttijd,
geen werk — met `--delay 0` vervalt hij, maar dan ook het ruisfilter.

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
