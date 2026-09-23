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
checkcheck --delay 0          # snel, één pass
checkcheck --filter arr       # alleen containers met "arr" in de naam
checkcheck --pull             # updates binnenhalen (herstart niets)
checkcheck --redeploy         # Portainer-stack uitrollen, na bevestiging
checkcheck --json             # machineleesbaar
checkcheck --exit-code        # exit 10 als er updates zijn
```

```
✅ nginx-test
🔄 sonarr — lscr.io/linuxserver/sonarr:latest, built 2026-09-10 (5 days ago)
🌀 vaultwarden — unstable — treated as CDN noise, not an update
➖ homegrown — no registry digest (built or imported locally)
🔒 pinned — pinned to a digest
❌ paperless — registry rate limit reached (HTTP 429)
```

## Portainer

`--redeploy` rolt de stack opnieuw uit van elke container met een bevestigde
update. Portainer haalt de images daarbij zelf op, dus `--pull` is geen
voorwaarde — samen gebruiken zet het downloaden wel vóór de downtime.

```bash
export CHECKCHECK_PORTAINER_URL=https://portainer.local:9443
export CHECKCHECK_PORTAINER_TOKEN=ptr_...
checkcheck --redeploy
```

Het token komt alleen uit de omgeving, nooit uit een vlag: argv is via `ps` voor
elke gebruiker op de host leesbaar. Bij een self-signed certificaat:
`--portainer-insecure`.

Er wordt altijd om bevestiging gevraagd en zonder terminal weigert het. Het
compose-bestand gaat ongewijzigd terug, `prune` staat uit, en git-gebaseerde
stacks worden overgeslagen.

## Beperkingen

- Alleen publieke registries (anonieme token-auth).
- Twee passes verkleinen CDN-ruis, maar sluiten het niet uit.
- Docker Desktop's containerd-store geeft ook lokaal gebouwde images een
  `RepoDigest` onder hun eigen naam. Checkcheck kan ze daardoor niet van
  gepullde images onderscheiden en vraagt ze op bij de registry, wat een 404 of
  een 401 oplevert in plaats van "lokaal gebouwd".
- Een geslaagde redeploy betekent "aangenomen", niet "draait en is gezond".
  Er is geen health-check en geen rollback.

## Structuur

| Bestand | Rol |
|---|---|
| `src/main.rs` | CLI, statusbepaling, rapport |
| `src/docker.rs` | Daemon-verbinding, containers, pull |
| `src/registry.rs` | Registry v2: digests en build-datums |
| `src/portainer.rs` | Stacks ophalen en redeployen |
