# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

## [0.2.8] — 2026-05-11

### Fader MASTER en mode agent (SetMasterVolume)

Bug remonté post-v0.2.7 : le fader MASTER du browser n'agissait pas sur
l'écoute en mode agent. Cause : côté browser le fader pilotait uniquement
`masterGain` Web Audio, qui est silencieux en mode agent (l'écoute passe
par CPAL côté agent). Et côté Rust, le mixer n'avait simplement aucun
master_gain — `mix_into` envoyait l'output au CPAL playback directement
après le clamp.

Fix : nouveau message wire + champ master_gain dans AudioMixer.

- `BrowserMessage::SetMasterVolume { volume: f32 }` dans protocol.rs.
- `AudioMixer::master_gain: f32` (default 1.0) + `set_master_gain(gain)`
  avec clamp défensif dans [0.0, 1.5] et NaN→1.0.
- Application dans `mix_into` AVANT le clamp final pour qu'un master à
  0.5 atténue proprement un mix qui aurait dépassé 1.0. Skip la
  multiplication si gain == 1.0 (cas par défaut, économise N muls sur
  le hot path callback CPAL).
- Tap REC-3 push_mix reçoit l'output APRÈS master_gain — cohérent avec
  le browser qui enregistre aussi le mix post master fader, et le user
  qui entend le mix final pondéré par son master.

Côté browser : fader master oninput envoie `set-master-volume` à l'agent
quand connecté, ET applySoloMute() le ré-envoie à chaque sync (notamment
au capture-started) pour garantir que le mixer Rust connaît la valeur
courante après une reconnexion agent.

## [0.2.7] — 2026-05-11

### ENTRÉE OFF (SetInputCut) en mode agent

Bug post-v0.2.6 : le bouton « ENTRÉE OFF » du browser ne coupait plus
le flux envoyé aux pairs en mode agent. Implémentation browser-only
faisait `track.enabled = false` sur le MediaStream WebRTC, mais en
mode agent ce stream n'existe pas (capture pilotée par CPAL côté agent).

Fix : nouveau message wire `SetInputCut { cut: bool }` + flag atomic
`input_cut: Arc<AtomicBool>` partagé avec l'encoder_thread. Quand `cut == true`,
l'encoder remplit le buffer stéréo par des zéros immédiatement après
`remap_to_stereo`, AVANT tout traitement aval (RMS, self-monitor mixer,
record self stem, accumulation Opus, envoi RTP/SRTP au SFU).

Coût latence : 1 atomic load Relaxed par chunk capture (~400/s),
négligeable face à l'encode Opus (30-80μs/frame). Pas de coût quand
pas coupé.

## [0.2.6] — 2026-05-11

### Enregistrement multi-stems côté agent (REC-3)

Avant : en mode agent, le bouton REC du browser produisait un fichier
silencieux ou incomplet sur certaines configurations (notamment Scarlett
4-canaux), parce que les flux peers transitent par PlainTransport RTP
côté agent et ne sont jamais visibles depuis audioCtx browser — le
MediaRecorder local enregistrait alors un mix vide.

Fix : l'agent encode lui-même les stems Ogg/Opus, le browser délègue.

- Nouveau module `jamodio-audio-core/src/record/` :
  - `ogg.rs` : OggWriter + CRC32 (RFC 3533 / 7845).
  - `opus_ogg.rs` : OpusOggRecorder (Opus 20ms 128 kbps VBR + Ogg en mémoire).
  - `mod.rs` : Recorder (multi-stems) + RecorderHandle (thread record
    dédié + crossbeam channel non-bloquant).
- AudioMixer : injection record_tx + 3 tap sites (push_self_samples,
  push_samples remote, mix_into). Quand pas d'enregistrement : 1 if
  check, zéro alloc.
- PipelineState : start_recording(stems) / stop_recording() — démarre
  le thread record, retourne les fichiers Ogg/Opus.
- Protocole WS étendu : StartRecording / StopRecording côté browser,
  RecordingStarted / RecordingDone / RecordingError côté agent.
- Transfert en base64 dans recording-done unique (un seul message au
  stop ; WS supporte les MB en frame).

Garanties latence :
- Tap sites RT path : 1 if + 1 to_vec + 1 try_send (~200ns).
- Encode Opus + écriture Ogg : isolés dans le thread record dédié.
- Channel bounded à 256 : drop sample-side avec warn rate-limité si
  record en retard — jamais le jam temps-réel n'est bloqué.

2 unit tests OpusOggRecorder passent (magic OggS, OpusHead/Tags, pages
audio, finalize empty case).

Le path browser-only (sans agent) reste inchangé pour rétrocompat.

## [0.2.5] — 2026-05-11

### Latency-align en mode agent (sprint B)

Avant : le serveur SFU calculait déjà `delay = maxHalfRtt − peerHalfRtt`
(EMA α=0.3, broadcast `latency-align` toutes les 2 s, cf.
`server/latency-equalizer.js`) et le browser appliquait MON delay sur
chaque `consumer.rtpReceiver.playoutDelayHint`. Mais en **mode agent**,
le RTP arrive direct SFU → agent CPAL → mixer → playback, et le delay
n'était **jamais transmis à l'agent**. Donc à 3+ peers ou avec un peer
distant, on perdait l'alignement automatique qui fonctionne en mode
browser. Invisible à 2 peers FR-FR fibre (test Ben+Yannick 22 ms reste
22 ms), critique dès qu'on passe à 3+ peers ou qu'un peer est sur un
continent différent.

Fix : nouveau message wire + handler agent.

- Nouveau `BrowserMessage::SetPeerDelay { producer_id, delay_ms }` dans
  [protocol.rs](jamodio-audio-core/src/protocol.rs).
- Nouvelle méthode `AudioMixer::set_peer_delay()` qui ajuste le
  `target_samples` du jitter buffer du stream concerné : target final =
  `REMOTE_BASE_TARGET_MS (10) + delay_ms`, clampé par le ring buffer.
- **Hystérèse** : sans filtre, chaque broadcast 2 s reseterait le
  pre-fill gate → micro-coupure audible 2×/s. On ne re-set que si
  `|new − current| > 5 ms` (PEER_DELAY_HYSTERESIS_MS).
- Ring buffer élargi côté `set_target_ms` : clamp désormais
  `[MIN_TARGET_MS, MAX_ALIGN_TARGET_MS=200]` (vs `MAX_TARGET_MS=40`
  avant). L'adaptation automatique up/down reste bornée à 40 ms — seul
  le pilotage externe peut monter au-delà pour l'alignement WAN.
- Capacité ring 250 → 300 ms pour marge confortable au-dessus de
  MAX_ALIGN_TARGET_MS. Coût RAM : ~115 KB / stream (vs 96 KB avant).
- Handler `BrowserMessage::SetPeerDelay` dans ws_server appelle
  `mixer.set_peer_delay()`. Pas de log info ici (broadcast 2 s, on
  loggue uniquement les changements significatifs côté mixer en debug).

### Côté browser

- Dans `handleLatencyAlign(delays)`, après l'appel `applyReceiverHint`
  existant, on forward MON delay (`delays[myUserId].delay`) à l'agent
  pour chaque peer remote en mode agent
  (`p.agentMusic && p.agentMusicProducerId`). Sémantique cohérente avec
  Chrome `playoutDelayHint` : on retarde MA sortie sur tous les streams
  remote pour m'aligner avec le peer le plus lent.

### Précision attendue

±2.5 ms (1 frame Opus). Largement suffisant musicalement. Si la dérive
adaptative se révèle problématique en pratique (adapt_down sous le
delay d'alignement), V2 : séparer `align_delay` du target adaptatif
(~30 lignes en plus).

## [0.2.4] — 2026-05-11

### Self-monitor via agent — 25 ms → ≈10 ms ear-to-ear

Yannick puis Ben rapportaient s'entendre « légèrement décalé » dans leur
casque pendant les sessions, même avec un setup correctement configuré
(Scarlett bien matchée, agent v0.2.3, DSCP EF en place, ping 22 ms peer).
Diagnostic : en mode agent, **deux captures simultanées** du même device
audio cohabitaient — l'agent CPAL pour le flux RTP (parfait, 2.7 ms), et
le browser via `getUserMedia` pour le monitoring local (≈25 ms à cause du
buffer Chrome opaque, non désactivable). Si l'utilisateur ouvrait le
fader « moi » dans la tranche, il s'entendait via la chaîne browser
≈30 ms, pas via l'agent.

**Fix architectural** : le self-monitor passe maintenant **dans l'agent**.

- Nouveau stream réservé `"self"` dans `AudioMixer` ([mixer.rs](jamodio-audio-core/src/mixer/mixer.rs)),
  volume initial **0.0** (anti-larsen au démarrage). Jitter target forcé
  à 5 ms (MIN_TARGET_MS) — pas de gigue réseau, on prend le minimum stable.
- Encoder thread ([pipeline.rs](jamodio-agent/src/pipeline.rs)) **forke** les samples post-remap :
  branche 1 → encode Opus → SFU (inchangé), branche 2 → `push_self_samples`
  dans le mixer local. Lock `parking_lot` contended ≤ µs, négligeable
  pour un thread RT à frame 2.7 ms.
- Le stream local est exclu de `stream_count` / `total_underruns` /
  `mean_target_ms` — l'UI agent continue de refléter la santé des flux
  remote uniquement.
- Nouveau message wire `SetSelfMonitorVolume { volume: f32 }` ([protocol.rs](jamodio-audio-core/src/protocol.rs)) →
  handler côté ws_server qui clampe NaN/négatif et appelle
  `mixer.set_self_monitor_volume(v)`. Le browser pilote le fader « moi »
  via ce message.
- Le stream est créé dans `start_capture()` et supprimé dans
  `stop_capture()` — symétrique avec le cycle capture.

**Latence ear-to-ear self** :

| Avant (chaîne browser) | Après (chaîne agent) |
|---|---|
| getUserMedia 25 ms + audioCtx 5–10 ms ≈ **30 ms** | capture 2.7 + target 5 + playback 2.7 ≈ **10 ms** |

Compatible avec le plan futur d'effets natifs (AMPLITUBE-like) : la
chaîne d'insertion entre `accumulator` et le fork est un point d'entrée
unique pour des effets Rust ou des plugins VST3 hostés.

### Côté browser

- En mode agent, suppression de `getUserMedia` instrument du graphe Web
  Audio. Plus de `selfPanNode` / `localMusicGain` / `localMusicSource`
  alimentés par la capture local. Le fader « moi » envoie maintenant
  `set-self-monitor-volume` à l'agent.
- VU-mètre self alimenté par `input_rms` du Stats agent (déjà exposé
  depuis v0.2.0, mais inutilisé).
- `_agentFallbackMusicStream` conservé — si l'agent meurt mid-session,
  le browser recrée un `getUserMedia` éphémère pour le fallback WebRTC.

## [0.2.3] — 2026-05-10

### Bug-report end-to-end pour la BETA

- Nouveau handler WS `GetLogsArchive { maxDays, maxBytes }` qui retourne
  les N derniers jours de logs agent concaténés en plain text, plafonnés
  à `maxBytes` (tronqués anciens-d'abord). Utilisé par le module Support
  browser pour packager un bug-report avec les logs des 2 côtés en un
  seul fichier `.txt` sous la limite Resend de 25 MB.
- `sessionId` (UUID v4 généré côté browser, persisté en `sessionStorage`)
  désormais logué côté agent au `HelloAck` → permet au support de croiser
  les logs browser et agent à partir d'un identifiant unique apparaissant
  dans les deux fichiers.
- Endpoint WS read-only `ws://localhost:9876/?op=logs` exposé pour
  l'export hors-studio (UI agent dashboard).
- Cosmétique : 6 warnings clippy fixés sur `ws_server` et `pipeline` (chore).

## [0.2.2] — 2026-05-07

### Strict device identification — fin des silent fallbacks

Suite session avec un peer (Scarlett 4th Gen, agent connecté mais signal
absent côté autre peer ; symptôme « son OK avec BlackHole, pas avec
Scarlett ») : la sélection « Scarlett Solo USB » dans Settings ne tombait
pas toujours sur le bon device CPAL, et l'agent fallbackait silencieusement
sur le default système (= mic interne du Mac, qui ne capture rien).

**Fix racine — pas d'approximation possible** :

- **Id stable et unique par device** (`{index}:{name}`).
  CPAL n'expose pas d'id stable plateforme ; on génère un id composite à
  l'enumeration. L'index disambigue les noms dupliqués (deux dongles USB
  génériques étiquetés pareil). Le browser stocke et renvoie EXACTEMENT
  cet id ; au resolve, l'agent vérifie que l'index pointe toujours sur un
  device au même nom.

- **`get_input_device(id)` strict** : parse l'id, récupère le device à
  cet index, vérifie le nom. Si quoi que ce soit ne match pas (index hors
  borne, hot-plug entre Settings et StartCapture, format d'id inconnu) →
  `None` propagé en erreur explicite. **Suppression du fuzzy match et du
  fallback `host.default_input_device()`**. Idem pour l'output.

- **Nouveaux messages protocol** :
  - `CaptureStarted { deviceId, deviceName, channels }` — confirmation
    explicite à chaque démarrage capture, le browser peut afficher
    « Capture active sur : Scarlett Solo USB ».
  - `CaptureError { reason, requestedDevice, detail }` — si le device
    demandé est introuvable. Le browser doit afficher un toast bloquant
    et forcer l'ouverture de Settings (jamais de session muette
    silencieuse).

- **Erreur typée `CaptureStartError`** côté pipeline.rs (variants
  InputDeviceNotFound / OutputDeviceNotFound / Other) — le ws_server peut
  router vers le bon message wire selon la cause.

- **Resolution input avant tout side-effect** : la résolution device se
  fait AVANT le `stop_capture()` et l'allocation du socket UDP. Si le
  device est introuvable, on échoue tout de suite, sans perturber l'état
  pipeline existant.

### Audit des fallbacks restants

- Output : si le browser n'a pas explicitement sélectionné d'id (`None`),
  on prend le `default_output_device()` système — c'est conforme à la
  décision audio_output_decision (sortie déléguée à l'OS, pas de picker
  côté browser). Mais si un id explicite est sélectionné et qu'il échoue,
  on renvoie `OutputDeviceNotFound` — pas de fallback hybride.

## [0.2.1] — 2026-05-09

### Hotfix — UI Tauri webview à nouveau visible

La single-client policy de v0.2.0 rejetait silencieusement la WS de
l'UI Tauri webview interne (la fenêtre dashboard de l'agent qui affiche
DEVICE / LATENCE / JITTER / streams en live). Conséquence : l'UI restait
sur "—" partout et "ws://localhost:9876 — déconnecté" en bas.

**Fix** : whitelist des origins `tauri://localhost` et `http://tauri.localhost`
comme **clients internes** qui bypass la single-client policy. Ces clients
sont en LECTURE SEULE (heartbeat get-stats uniquement, ne touchent pas
le `PipelineState`), donc aucun risque de race avec le client externe
jamodio.com.

Le cleanup pipeline (stop_all) ne tourne PLUS pour les clients internes
au disconnect — fermer la fenêtre Tauri n'arrête plus la session studio
en cours côté browser.

### Aussi dans cette release

- Migration des 2 derniers `console.*` agent-related restants côté
  groupe.js vers `log.*` structuré (`agent-produce`, `audio`).

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

[Unreleased]: https://github.com/jamodio-app/audio-engine/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.2.1
[0.2.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.2.0
[0.1.7]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.7
[0.1.6]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.6
[0.1.5]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.5
[0.1.4]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.4
[0.1.3]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.3
[0.1.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.0
