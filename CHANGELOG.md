# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

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

[Unreleased]: https://github.com/jamodio-app/audio-engine/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.4
[0.1.3]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.3
[0.1.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.0
