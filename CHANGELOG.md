# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

## [0.2.0] — 2026-05-09

### Robustesse agent — refonte production-grade

Refonte complète du cycle de vie agent + protocole, suite à un bug observé
en session 2-peers où un peer (avec agent actif) était silencieux pour les
autres : sa music n'était jamais produite (ni en plain ni en WebRTC).
Cause racine : double `detectAgent()` créant 2 WebSockets, état `agentConnected`
divergent entre `setupAudio` et `produceLocalStreams`, callbacks zombies au
disconnect, `musicStream = null` jamais restauré post-fallback.

#### Protocole — handshake explicite

- **Nouveau message `Hello`** envoyé par l'agent dès l'open WS.
  Inclut `protocolVersion`, `agentVersion`, `os`, `arch`, `capabilities`.
  Le browser bascule en `CONNECTED` sur réception (ou après timeout 1.5 s
  en mode legacy compat pour les agents v0.1.x).
- **Nouveau message `Shutdown`** broadcasté par l'agent avant `app.restart()`
  (auto-update). Le browser peut afficher un toast et préparer un fallback
  gracieux au lieu de subir un TCP close brutal + watchdog 3 s.
- **Nouveau message `Rejected`** envoyé à un client WS quand un autre client
  est déjà connecté (single-client policy ci-dessous).
- **Nouveau message `HelloAck`** browser → agent (optionnel, futur tracking).
- Constante `PROTOCOL_VERSION = 1` exposée dans `protocol.rs`.

#### Single-client policy

Le serveur WS rejette désormais les connexions concurrentes. Si une 2e
WebSocket arrive alors qu'une 1re est active, l'agent envoie `Rejected`
puis ferme la WS. Évite les races sur le shared `PipelineState` (qui
provoquaient le bug ci-dessus quand le browser ouvrait 2 WS en concurrence).

#### Origin check WS

Le upgrade WebSocket vérifie l'en-tête `Origin`. Whitelist :
`https://jamodio.com`, `https://*.vercel.app`, `http://localhost:*`,
`http://127.0.0.1:*`, `file://`. Empêche une page web tierce sur
localhost de piloter l'agent silencieusement.

#### Lock timeout sur heartbeat

Le handler `GetStats` (heartbeat 1.5 s) acquiert maintenant `pipeline.lock()`
avec un timeout de 200 ms. Si dépassé (CPU saturé, contention encoder),
il répond `Error{overloaded}` au lieu de bloquer — évite le faux-positif
où le watchdog browser tue la session à 3.5 s alors que l'agent est juste
lent.

#### Deep-link handler implémenté

`jamodio://launch` reçu via `tauri-plugin-deep-link` montre + focus
maintenant la fenêtre principale (au lieu de l'ignorer silencieusement).
Le bouton "Lancer" depuis la pill browser fait donc bien remonter l'app
existante au premier plan.

#### Auto-update broadcast

Avant `app.restart()`, l'agent broadcaste `Shutdown{reason:"update"}` à
tous les clients WS connectés. Petit délai 500 ms pour leur laisser le
temps de recevoir + handler. Browser affiche un toast "L'Audio Engine
se met à jour" + relance la détection auto après 5 s.

### Breaking changes

- **Protocole bumped** : `Hello` est désormais le 1er message envoyé par
  l'agent. Les browsers v0.1.x ignorent ce message inconnu (compat
  ascendante OK). Les browsers v0.2.0+ peuvent fonctionner avec un agent
  v0.1.x via `legacyMode` (timeout handshake 1.5 s).
- **Single-client policy** : si un browser tente d'ouvrir une 2e WS au
  même agent (anciennement permis silencieusement), il reçoit `Rejected`.
  L'utilisateur voit un toast "Audio Engine déjà utilisé par un autre onglet".

## [0.1.7] — 2026-05-07

### Qualité audio — fin des clicks numériques sur sortie
- **`JitterBuffer::push` : drop-oldest au lieu de truncation mid-paquet.**
  Quand le ring est plein, on jette les samples les plus anciens (côté
  consumer) pour faire la place au paquet entier. Avant, `push_slice`
  partial-write coupait le paquet en deux côté producer → discontinuité
  PCM mid-paquet = click numérique audible (`Max difference 0.336` sur
  2 samples float détecté par ffmpeg astats sur l'enregistrement MIX du
  2026-05-07 17h14 — 4500 overflows en 9 minutes, ~4 % de l'audio droppé).
- **`JitterBuffer::pull` : drift drain pré-emptif.** Si le buffer dépasse
  3 × `target_samples`, on draine les plus anciens samples pour ramener
  à target. Sans ça, post-burst SFU ou drift d'horloge producer↔consumer,
  le buffer pouvait rester à 80-90 ms indéfiniment → latence silencieuse
  9× la cible + push-overflows en cascade au moindre nouveau jitter.
- **Capacité ring 100 → 250 ms.** Marge confortable au-dessus du seuil
  drift drain (3 × MAX_TARGET_MS = 120 ms) pour absorber les bursts SFU
  sans drop. Coût RAM négligeable (~96 KB / stream). **N'AFFECTE PAS la
  latence** : la latence est driven par `target_samples` (5-40 ms), pas
  par `capacity_ms`.
- **Diagnostic SR mismatch device output.** Warn explicite à `start_playback`
  si la SR native du device ≠ 48 kHz (ex. casque jack Mac 44.1, BlackHole
  2ch). CoreAudio fait alors un resampling implicite de qualité variable.
  Aide le diag en cas de glitches résiduels.
- Nouveaux compteurs `overflow_drops` / `drift_drops` exposés sur le jitter
  buffer + warns rate-limited (puissances de 2) côté `mixer.rs`. Plus de
  spam `full_count=4500` toutes les ms — un événement = un log.

### Latence préservée
Le budget ear-to-ear < 25 ms n'est pas affecté : `target_ms` (10 ms init,
5-40 range) inchangé, `opusPtime` 2.5 ms inchangé. Les fixes corrigent la
qualité audio **sans ajouter** de latence — le drift drain ramène
activement vers la cible quand un burst l'a fait dériver.

## [0.1.6] — 2026-05-06

### Auto-update — vraiment fonctionnel
- **`bundle.createUpdaterArtifacts: true`** dans `tauri.conf.json`. Sans
  ce flag (Tauri 2 par défaut `false`), le `.app.tar.gz.sig` n'était pas
  généré → tauri-action skipait `latest.json` → l'updater côté agent ne
  pouvait pas trouver de version à proposer. Diagnostic confirmé dans
  les logs CI v0.1.5 :
  > `Signature not found for the updater JSON. Skipping upload...`
- À partir de cette release, le `latest.json` est publié avec la release
  GitHub. Les agents 0.1.6+ vérifient et s'updatent automatiquement.

### Logs — bruit éliminé pendant le shutdown
- Distinction `Full` vs `Disconnected/Closed` dans les logs `try_send` :
  - capture → encoder (`crossbeam_channel::TrySendError`)
  - encoder → tokio mpsc (`tokio::sync::mpsc::error::TrySendError`)
- Avant : milliers de warns parasites pendant ~45 s après stop_capture
  (callback CPAL macOS continue à pousser après drop, le canal devient
  Disconnected → on logguait "channel full" à tort).
- Après : Disconnected/Closed = `debug` une seule fois (cas attendu de
  shutdown). Full reste `warn` power-of-2 (vrai signal d'overload).

### Note interne
- Le bug "CPAL stream continue 45 s après drop sur macOS" reste à
  investiguer (probablement besoin d'un `stream.pause()` explicite avant
  drop). Pas critique pour le user, juste cosmétique.

## [0.1.5] — 2026-05-06

### Auto-update débloqué (BUG B mémoire `agent_release_checklist`)
- **`tauri-plugin-updater` ajouté** : le bloc `updater` de `tauri.conf.json`
  était inerte depuis l'origine (plugin jamais installé). À partir de cette
  release, l'agent vérifie au démarrage s'il y a une nouvelle version sur
  GitHub releases et l'installe automatiquement (puis restart).
- **Permission `updater:default`** ajoutée dans `capabilities/default.json`.
- Check exécuté 5 s après le démarrage (laisse le boot se finir avant de
  hit le réseau). Fire-and-forget, échec silencieux si offline.
- Note : les binaires < 0.1.5 (déjà installés) n'ont PAS le plugin et ne se
  mettront jamais à jour seuls. Le banner browser (côté jamodio.com) est le
  filet pour ces users.

### Protocole WS — détection version côté browser (BUG A mémoire)
- `AgentMessage::Status` enrichi avec `version` / `os` / `arch`. Champs
  optionnels (rétro-compatibilité). Le browser jamodio.com lit ces champs
  au handshake pour afficher un banner "agent obsolète" sur les binaires
  ≤ 0.1.4 qui n'envoient pas la version.

## [0.1.4] — 2026-05-06

### Audio — corrections audit pipeline (Sprints 1+2+3)
- **JitterBuffer** : pre-fill gate via état `primed` — corrige le silence
  systématique au démarrage du mode agent (le callback CPAL démarrait
  avant l'arrivée du 1er paquet RTP). Re-prime sur underrun pour éviter
  les rafales de glitches sur burst de jitter.
- **`stereo: 1`** ajouté dans les rtpParameters de `plain-produce` côté
  SFU — corrige un décodage potentiellement mono côté peer browser.
- **`underruns`** : compteur réel exposé via `total_underruns()` dans
  GetStats (auparavant hardcodé à 0). Le BUG est maintenant visible
  dans le dashboard de l'UI agent.
- **`SetBuffer`** : handler implémenté — la cible jitter buffer est
  désormais réglable depuis l'interface. `default_target_ms` mémorisé
  pour les futurs streams.
- **Decoder** : `pcm_buf` / `f32_buf` pré-alloués, retour `&[f32]`
  zéro-copie sur le hot path (auparavant 2 Vec/paquet × 400 pps × N
  streams).
- **`block_in_place`** autour des `mixer.lock()` dans `recv_decode_task`
  pour éviter le blocage du worker tokio sous contention.
- **`try_send` silencieux** loggés (encoder ET capture) — sous charge
  CPU/réseau les paquets droppés sont maintenant tracés.

### Logging structuré (PROD-grade)
- Migration de **54 `eprintln!`** vers `tracing` avec targets
  `jamodio::*` (lifecycle, ws, pipeline, encoder, recv, mixer, decoder,
  devices, capture, playback, udp, srtp, drift, tray).
- Nouveau module `agent/jamodio-agent/src/logging.rs` : double sortie
  stderr (compact) + fichier rolling daily non-bloquant.
- Path OS-aware : `~/Library/Logs/Jamodio/agent.log.YYYY-MM-DD` (macOS),
  `%APPDATA%/Jamodio/logs/` (Windows), `~/.local/state/jamodio/` (Linux).
- Filtre par défaut : `info,jamodio_agent=debug,jamodio_audio_core=debug`,
  override via `RUST_LOG`.
- Nouvelle commande Tauri `open_log_dir` : bouton "Exporter les logs"
  dans l'UI ouvre le dossier via le file manager natif.

### UI agent — corrections affichage
- **Tray icon** : `iconAsTemplate: true` — l'icône (noire pure sur
  transparent) était invisible en dark mode macOS. macOS l'inverse
  maintenant automatiquement noir/blanc selon le thème.
- **Stats latence** : ancien calcul `capMs + playMs + bufMs` triple-comptait
  `bufMs` (~10 ms affichés au lieu de ~20 ms réels). `total_latency_ms`
  pré-calculé côté agent et exposé via le protocole, l'UI le lit tel quel.
- **Nouveaux champs UI** : Latence end-to-end / Jitter target adaptatif /
  CPAL buffer / Streams / Underruns. Labels clarifiés.

### Notes
- Cible CI inchangée : **macOS Apple Silicon uniquement**. Windows
  (bug ABI `srtp 0.7`) et Intel macOS (runner `macos-13` saturé) restent
  à débloquer en v0.1.5+ — voir mémoire `agent_windows_blocker.md`.

## [0.1.3] — 2026-05-03

### Performance — Phase 2 codec & pipeline
- Opus ptime **10 ms → 2.5 ms** (−7,5 ms latence end-to-end)
- Bitrate Opus **256 → 320 kbps** (sweet spot stéréo musique)
- **RT priority** sur le thread encodeur (crate `thread-priority`)
- **Zéro alloc** dans le mixer `mix_into` (préallocation `temp_buf`)

### Synchronisation — T4
- **DriftEstimator par stream** : mesure de la dérive d'horloge ppm via
  progression des timestamps RTP vs horloge locale. Log auto toutes les
  30 s. Compensation rubato (T4.2b) reportée jusqu'à observation des
  valeurs réelles sur sessions longues.

### Audio settings — Sprint 1+2+3
- **Single-instance lock** (`tauri-plugin-single-instance`) : empêche
  un 2e process agent de démarrer (clic répété "Lancer", deep link,
  double-click DMG…). Le 1er process reprend le focus.
- **Restart live du playback CPAL** quand l'output device change via
  `select-devices` : plus besoin de quit/rejoin pour basculer la sortie
  audio. Swap atomique via `mem::replace` (mixer Arc partagé, ring
  buffer continue d'accumuler).

### Sécurité — héritage Phase 1
- SRTP (AEAD AES-256-GCM) sur PlainTransport agent ↔ SFU
- ANNOUNCED_IP auto-détecté + fail-fast (plus de fallback silencieux)
- Secrets via dotenv (plus en clair dans `ecosystem.config.js`)

### Notes
- Cible CI : **macOS Apple Silicon uniquement** pour cette release.
  Windows (bug ABI `srtp 0.7`) et Intel macOS (runner `macos-13`
  saturé) restent à débloquer en v0.1.4 — voir mémoire interne
  `agent_windows_blocker.md`.

## [0.1.0] — 2026-04-??

Première release publique.

### Ajouté
- Agent Tauri v2 (macOS Apple Silicon / macOS Intel / Windows x64)
- Capture audio bas-niveau via CPAL (CoreAudio / ASIO)
- Encodage Opus 10 ms + RTP / UDP avec comedia punch vers le SFU
- Réception : jitter buffer par stream, PLC, mixer N streams → sortie CPAL
- WebSocket local `ws://localhost:9876` pour la détection et le contrôle
  depuis l'app web (jamodio.com)
- Tray icon macOS + auto-start (LaunchAgent)
- Deep link `jamodio://launch` pour réveiller l'agent depuis le navigateur
- Updater Tauri signé pointant sur `releases/latest/download/latest.json`

### Notes d'installation
- **macOS (non signé)** : première ouverture, clic-droit sur l'app →
  Ouvrir, ou bien `xattr -cr /Applications/Jamodio\ Audio\ Engine.app` en
  Terminal. Une signature Apple Developer sera ajoutée dans une release
  ultérieure.
- **Windows** : installeur NSIS standard. Autoriser Windows SmartScreen à
  la première ouverture (« Informations complémentaires » → « Exécuter
  quand même ») tant que la signature Authenticode n'est pas en place.

[Unreleased]: https://github.com/jamodio-app/audio-engine/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.2.0
[0.1.7]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.7
[0.1.6]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.6
[0.1.5]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.5
[0.1.4]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.4
[0.1.3]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.3
[0.1.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.0
