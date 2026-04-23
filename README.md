# Jamodio Audio Engine

Agent desktop natif (Tauri v2 + Rust) qui pilote la latence minimale entre la
carte son du musicien et le SFU Jamodio :

```
Carte son (CoreAudio / ASIO)
    ↓ CPAL capture (buffer 128 samples, ~2.7 ms)
    ↓ Opus encode (10 ms)
    ↓ RTP / UDP comedia punch
    ↓ mediasoup SFU (sfu.jamodio.com)
```

Côté UI : fenêtre Tauri minimale + tray icon + WS local `ws://localhost:9876`
que l'app web ([jamodio.com/app](https://jamodio.com/app)) détecte pour
basculer automatiquement du mode navigateur (WebRTC) au mode agent (RTP direct).

Repo : [github.com/jamodio-app/audio-engine](https://github.com/jamodio-app/audio-engine)

---

## Structure

```
.
├── Cargo.toml                   # Workspace Cargo (2 crates)
├── jamodio-audio-core/          # Crate lib (Opus, RTP, UDP, mixer, jitter)
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

## Setup local (développement)

### Prérequis

- **Rust stable** : `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Tauri CLI** : `cargo install tauri-cli --version "^2.0" --locked`
- **Node** (pour le dev server de la vue UI) : non nécessaire, l'UI est statique

### macOS Apple Silicon — particularité libopus

Homebrew fourni sous Rosetta donne libopus x86_64 uniquement. Ce repo
embarque [`deps/opus-arm64/`](./deps/opus-arm64) — une libopus ARM
pré-compilée — et utilise [`.cargo/config.toml`](.cargo/config.toml) pour
pointer `PKG_CONFIG_PATH` dessus. **Rien à faire** en local, ça marche.

Si tu n'es pas sur Apple Silicon : supprime `deps/opus-arm64/` et
`.cargo/config.toml`, puis `brew install opus pkg-config` (macOS Intel) ou
via vcpkg (Windows). Le crate `audiopus` détectera libopus système.

### Régénérer les icônes (app + tray)

Les icônes sources sont des SVG dans [`jamodio-agent/icons/src/`](./jamodio-agent/icons/src/) (logo V5 + tray monochrome). Pour régénérer tous les formats attendus par Tauri (`32x32.png`, `128x128.png`, `icon.icns`, `icon.ico`, `tray.png`) :

```bash
# Prérequis une seule fois
brew install librsvg imagemagick

# Régénération depuis les SVG
./jamodio-agent/icons/src/regenerate.sh
```

À relancer chaque fois que tu modifies les SVG sources. Les PNG/ICNS/ICO générés sont commités tels quels (Tauri attend les rasters).

### Build local

```bash
# Depuis la racine du workspace
cd jamodio-agent

# Dev (hot reload de l'UI)
cargo tauri dev

# Release build (produit le .dmg / .exe dans target/release/bundle/)
cargo tauri build
```

Les artefacts :
- macOS : `target/release/bundle/dmg/Jamodio Audio Engine_0.1.0_aarch64.dmg`
- macOS Intel : `target/x86_64-apple-darwin/release/bundle/dmg/Jamodio Audio Engine_0.1.0_x64.dmg`
- Windows : `target/release/bundle/nsis/Jamodio Audio Engine_0.1.0_x64-setup.exe`

---

## Release (publication d'une nouvelle version)

### Principe

Toute release est **déclenchée par un tag Git** `vX.Y.Z`. Le workflow
[`release.yml`](.github/workflows/release.yml) tourne sur GitHub Actions :

1. **3 runners en parallèle** compilent l'agent :
   - `macos-14` → cible `aarch64-apple-darwin` (Apple Silicon)
   - `macos-13` → cible `x86_64-apple-darwin` (Intel)
   - `windows-2022` → cible `x86_64-pc-windows-msvc`
2. Chaque build signe l'updater Tauri (avec `TAURI_SIGNING_PRIVATE_KEY`).
3. Le job `publish` renomme les artefacts en noms stables :
   - `Jamodio-Audio-Engine-macOS-AppleSilicon.dmg`
   - `Jamodio-Audio-Engine-macOS-Intel.dmg`
   - `Jamodio-Audio-Engine-Windows.exe`
4. Le draft devient une release publiée.

**Tu n'as besoin que de ta machine de dev** — GitHub fournit les 3 runners.
Temps total : ~10–15 min après le `git push --tags`. 0 € pour les repos publics.

### Setup initial (une seule fois)

#### 1. Générer la keypair Tauri updater

```bash
cd jamodio-agent
cargo tauri signer generate -w ~/.tauri/jamodio-updater.key
```

Le CLI affiche la **public key** (longue chaîne base64). Copie-la dans
[`tauri.conf.json`](./jamodio-agent/tauri.conf.json) à la clé `plugins.updater.pubkey`.

```json
"plugins": {
  "updater": {
    "endpoints": ["https://github.com/jamodio-app/audio-engine/releases/latest/download/latest.json"],
    "pubkey": "dW50cnVzdGVkIGNvbW1lbnQ6..."
  }
}
```

#### 2. Ajouter les secrets GitHub

`github.com/jamodio-app/audio-engine` → **Settings** → **Secrets and variables** → **Actions** → **New repository secret** :

| Nom | Valeur |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Contenu complet du fichier `~/.tauri/jamodio-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Le password choisi à l'étape 1 |

### Publier une version

1. **Bump version** dans 2 endroits :
   - [`jamodio-agent/Cargo.toml`](./jamodio-agent/Cargo.toml) — ligne `version = "X.Y.Z"`
   - [`jamodio-agent/tauri.conf.json`](./jamodio-agent/tauri.conf.json) — clé `version`

2. **Ajouter une entrée au [CHANGELOG.md](./CHANGELOG.md)** (voir format plus bas).

3. **Commit + tag + push** :
   ```bash
   git add Cargo.toml jamodio-agent/Cargo.toml jamodio-agent/tauri.conf.json CHANGELOG.md
   git commit -m "Release vX.Y.Z"
   git tag vX.Y.Z
   git push && git push --tags
   ```

4. **Monitoring du build** : onglet **Actions** du repo. En cas d'échec sur
   un runner, cliquer sur le job pour voir les logs. Les erreurs les plus
   courantes : dépendances libopus manquantes (voir step "Install libopus"
   dans le workflow), ou keypair updater mal configurée.

5. Quand tout est vert (✓), la release apparaît sur la page du repo avec les
   3 artefacts renommés. Les URLs `releases/latest/download/<stable-name>`
   commencent immédiatement à pointer sur la nouvelle version → l'app web et
   l'updater Tauri reçoivent la mise à jour sans modification de code.

### Signature Apple / notarisation (optionnel, plus tard)

Sans Apple Developer certificat ($99/an), l'utilisateur macOS voit
Gatekeeper au 1er lancement (voir section **Permissions système** plus haut).
Pour supprimer cette friction :

1. Acheter un certificat "Developer ID Application" sur
   [developer.apple.com](https://developer.apple.com).

2. Exporter le .p12 (cert + clé privée) avec password.

3. Ajouter les secrets GitHub :
   - `APPLE_CERTIFICATE` (contenu du .p12 en base64)
   - `APPLE_CERTIFICATE_PASSWORD`
   - `APPLE_SIGNING_IDENTITY` (ex: `Developer ID Application: Nom (TEAM_ID)`)
   - `APPLE_ID` (email du compte Apple Developer)
   - `APPLE_ID_PASSWORD` (app-specific password, pas le mot de passe iCloud)
   - `APPLE_TEAM_ID`

4. Remplacer `"signingIdentity": "-"` par
   `"signingIdentity": "Developer ID Application: ..."` dans
   [`tauri.conf.json`](./jamodio-agent/tauri.conf.json).

5. Ajouter les entitlements **Hardened Runtime** (requis pour la notarisation)
   dans [`entitlements.plist`](./jamodio-agent/entitlements.plist) :
   ```xml
   <key>com.apple.security.cs.allow-jit</key>
   <true/>
   <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
   <true/>
   <key>com.apple.security.cs.disable-library-validation</key>
   <true/>
   ```

6. Décommenter les 6 lignes correspondantes dans
   [`release.yml`](.github/workflows/release.yml).

tauri-action gère automatiquement le processus signature + notarisation
(upload à Apple, attente de l'agrafe ~15 min, stapling). Le DMG produit
s'ouvre alors par double-clic sur n'importe quel Mac sans avertissement.

### Signature Windows Authenticode (optionnel, plus tard)

Même logique : acheter un certificat EV Code Signing (~300 €/an),
l'ajouter en secret GitHub, ajouter un step de signature dans le workflow.
Supprime l'écran SmartScreen au 1er lancement.

---

## Intégration côté web

Le frontend jamodio.com (app + landing) pointe sur les 3 URLs stables :

```
/releases/latest/download/Jamodio-Audio-Engine-macOS-AppleSilicon.dmg
/releases/latest/download/Jamodio-Audio-Engine-macOS-Intel.dmg
/releases/latest/download/Jamodio-Audio-Engine-Windows.exe
```

La détection ARM/Intel côté browser utilise :
1. `navigator.userAgentData.getHighEntropyValues(['architecture'])` (Chrome/Edge)
2. Fallback WebGL `WEBGL_debug_renderer_info` (détecte « Apple M1/M2 »)

Voir [`jamodio/app/js/lib/agent-status.js`](https://github.com/bengo82/jamodio/blob/main/app/js/lib/agent-status.js) dans le repo principal.
