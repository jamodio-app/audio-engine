# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

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

[Unreleased]: https://github.com/jamodio-app/audio-engine/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamodio-app/audio-engine/releases/tag/v0.1.0
