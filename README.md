# Jamodio Audio Engine

Agent desktop natif (Tauri v2 + Rust) qui pilote la latence minimale entre la
carte son du musicien et le SFU Jamodio :

```
Carte son (CoreAudio / ASIO)
    ↓ CPAL capture (buffer 64 samples, ~1,33 ms — repli 128 si la machine ne tient pas)
    ↓ Opus encode (10 ms)
    ↓ RTP / UDP comedia punch
    ↓ mediasoup SFU (sfu.jamodio.com)
```

Côté UI : fenêtre Tauri minimale + tray icon + WS local `ws://localhost:9876`
que l'app web ([jamodio.com/app](https://jamodio.com/app)) détecte pour
basculer automatiquement du mode navigateur (WebRTC) au mode agent (RTP direct).

Dépôt **privé** — code confidentiel (voir [LICENSE](./LICENSE)).

---

## Structure

```
.
├── Cargo.toml                   # Workspace Cargo (4 crates)
├── jamodio-audio-core/          # Crate lib (Opus, RTP, UDP, SRTP, mixer, jitter, record)
├── jamodio-au-host/             # Hôte plugins AudioUnit (macOS uniquement, FFI ObjC++)
├── jamodio-vst3-host/           # Hôte plugins VST3 (Windows uniquement, COM)
├── jamodio-agent/               # Binaire Tauri (UI + orchestration)
│   ├── tauri.conf.json          # Configuration Tauri (updater, deep-link, tray)
│   ├── Cargo.toml
│   ├── src/                     # Code Rust de l'agent
│   ├── ui/                      # HTML/JS de la fenêtre Tauri
│   ├── icons/                   # Icônes app + tray
│   ├── entitlements.plist       # macOS entitlements
│   └── info.plist               # macOS Info.plist
├── .cargo/config.toml           # Override PKG_CONFIG local (Mac M* / Rosetta)
├── deps/opus-arm64/             # libopus ARM pré-compilée (local uniquement, .gitignore)
└── .github/workflows/release.yml # CI multi-plateforme (GitHub Actions)
```

> `jamodio-au-host` ne compile que sur macOS, `jamodio-vst3-host` que sur
> Windows (chacun est vide sur l'autre OS) — ils exposent le même trait
> `PluginHost` de `jamodio-audio-core`.

---

## Permissions système macOS / Windows

### macOS — microphone + Gatekeeper

Au **premier lancement** de l'app :

1. **Dialog microphone système** (toujours — signé ou non) :
   > « Jamodio Audio Engine needs microphone access to capture your instrument audio for low-latency streaming. »

   Configuré via [`NSMicrophoneUsageDescription`](./jamodio-agent/info.plist)
   (Info.plist) + l'entitlement [`com.apple.security.device.audio-input`](./jamodio-agent/entitlements.plist).
   L'utilisateur clique **Autoriser** → la permission est mémorisée par macOS.

2. **Gatekeeper** — dépend de la signature :
   - **Sans Apple Developer cert** (situation actuelle) : macOS bloque au 1er
     lancement (« *l'app ne peut pas être ouverte, développeur non vérifié* »).
     Workaround utilisateur :
     - **Méthode A** : clic-droit sur l'app dans Applications → **Ouvrir**
       (une seule fois — ensuite double-clic normal).
     - **Méthode B** : `xattr -cr /Applications/Jamodio\ Audio\ Engine.app`
       en Terminal, puis lancement normal.
   - **Avec Apple Developer cert + notarisation** : zéro friction,
     double-clic fonctionne directement.

### Windows — SmartScreen

Sans signature Authenticode (situation actuelle), Windows Defender
SmartScreen affiche au 1er lancement :
> « Windows a protégé votre ordinateur ».

Workaround : cliquer **Informations complémentaires** → **Exécuter quand même**.

Pas de permission microphone système sur Windows (le device audio est
sélectionné dans l'agent, pas de prompt).

---

## Build & release

Les releases sont produites automatiquement par GitHub Actions
([`release.yml`](.github/workflows/release.yml)) au push d'un tag `vX.Y.Z` :
compilation macOS (Apple Silicon) + Windows, signature de l'updater Tauri,
installeurs sous des noms stables, fichiers de symboles privés (`.dSYM`,
`.pdb`). La diffusion aux musiciens passe par `npm run agent:publish` (dépôt
web), qui publie la version sur https://jamodio.com/installer. Procédure
complète : runbook mainteneur du dépôt web.

---

## Intégration côté web

Le site [jamodio.com](https://jamodio.com) propose le téléchargement de l'agent
via des URLs stables (toujours la dernière version définitive) :

```
https://jamodio.com/installer/Jamodio-Audio-Engine-macOS-AppleSilicon.dmg
https://jamodio.com/installer/Jamodio-Audio-Engine-Windows.msi
https://jamodio.com/installer/Jamodio-Audio-Engine-Windows-en.msi
```

L'updater lit `https://jamodio.com/installer/latest.json`.

Une fois installé, l'agent est détecté automatiquement par l'app web grâce au
WebSocket local `ws://localhost:9876`, qui bascule le mode audio du navigateur
(WebRTC) vers le mode agent (RTP direct).
