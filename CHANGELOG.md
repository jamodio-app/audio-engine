# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

## [0.4.0] — 2026-05-17

### Added — VST3 host Windows + parité MIDI

Sprint majeur : Windows passe de "audio + SRTP" à **parité fonctionnelle 95 %
avec macOS** sur les plugins INSERT + MIDI. Les musiciens Windows peuvent
maintenant utiliser des effets/synthés VST3 dans Jamodio comme leurs
collègues sur Mac avec leurs AU.

- **Nouveau crate `jamodio-vst3-host`** (Windows uniquement) implémentant le
  trait `PluginHost` partagé avec `jamodio-au-host`. Stack 100 % Rust pur
  via `vst3 = "0.3"` (bindings coupler-rs du SDK Steinberg) + `libloading`
  pour le chargement dynamique des `.vst3`. ~1500 LoC, 6 modules :
  `discovery` (paths VST3 système Win), `loader` (LoadedModule + ComPtr),
  `host` (Instance + setup_stereo/process_stereo lifecycle complet),
  `events` (MidiEventList impl IEventList VST3 + conversion MIDI 3 bytes
  → Event NoteOn/NoteOff), `state` (MemoryStream impl IBStream pour sync
  component↔controller), `host_app` (IHostApplication minimal pour les
  plugins commerciaux), `editor` (HWND attached + thread STA dédié).
- **Scan VST3 background au boot** sur Windows (= analogue du scan AU mac).
  Instancie chaque plugin pour lire latence / bus / has_editor. Cache
  partagé avec l'UI. Typique ~1-2 s pour 5-10 plugins installés.
- **Load / unload / bypass / process audio** validés en runtime sur
  Valhalla FutureVerb (effet, 0 sample latence intrinsèque, ~75 µs avg
  wall-clock @ 48k/64). Pre-warm 8 blocs au load pour absorber le warmup
  du 1er process (~3000 µs → 75 µs en steady-state).
- **MIDI input Windows** : `ListMidiDevices` + `SetInputSource` +
  `PlayMidiNote` (clavier HTML virtuel) activés. midir/winmm pour les
  ports physiques USB. Pseudo-entrée "Jamodio Virtual MIDI" en tête de
  liste (= permet de basculer source=MIDI et débloque le clavier HTML
  même sans clavier USB physique).
- **Plugin instrument (synthé) → audio** : `MidiEventList` injectée dans
  `ProcessData.inputEvents`, plugin reçoit NoteOn/NoteOff, génère audio
  sur bus 0 → mixé dans la pipeline → SRTP. Surge XT validé en runtime
  17/05.
- **`PluginHostImpl` type alias** (`AuHost` mac / `Vst3Host` win) dans
  `pipeline.rs` : champ unique `plugin_host` aux méthodes OS-agnostic
  via le trait. Zero divergence Mac dans la pipeline.
- **UI** : retrait du guard `agentOs !== 'macos'` dans `groupe.js
  renderFxSlot`. Slot FX visible sur les 2 OSes. Click handler du picker
  refactor format-agnostic (data-plugin-ref JSON) pour gérer AU + VST3
  sans cas particulier. MIDI device picker OS-aware avec hints
  Windows-spécifiques mentionnant le port virtuel OS-wide à venir.

### Pièges VST3 documentés (capitalisés pour les futurs sprints)

- **`activateBus` requis sur les bus events** (= `MediaTypes_::kEvent`),
  pas que les bus audio. Sans ça les synthés ignorent silencieusement
  l'IEventList qu'on leur passe.
- **N'activer que le bus 0** (= main I/O). Activer les bus aux (= aux
  sends Surge XT) sans fournir leurs buffers dans `ProcessData` cause un
  silence inexplicable (le plugin écrit "ailleurs"). Multi-bus support
  reporté post-beta.
- **COM apartment STA + msg pump pre-attached** + IPlugFrame + state sync
  + IHostApplication + IConnectionPoint::connect + setComponentHandler :
  tous nécessaires pour `IPlugView::attached()` sur les plugins
  commerciaux. Code complet en place.

### Known limitations Windows v0.4.0

- **Éditeur GUI VST3** : `IPlugView::attached()` hang sur Windows 11 ARM
  sous émulation x64 (VMware Fusion). Reproduit sur Valhalla FutureVerb
  ET Surge XT (= pas un quirk plugin spécifique, plutôt émulation
  graphique ARM problématique). À valider sur vrai Windows x64 natif
  avant tirer conclusion — le code COM/HWND/msg-pump est complet et
  conforme à la spec Steinberg.
- **Port virtuel "Jamodio Virtual MIDI" OS-wide** : sur Mac, CoreMIDI
  fournit nativement un port visible depuis Logic/Ableton/etc. Windows
  n'a pas d'équivalent built-in. En attente de license commerciale
  teVirtualMIDI (Tobias Erichsen) pour S2.5. Le clavier HTML Jamodio
  intégré fonctionne déjà sans le port virtuel.

### Refactor / cleanup

- Type alias `PluginHostImpl` dans pipeline.rs (au lieu de cfg-gating
  séparé `au_host` mac / `vst3_host` win). Méthodes plugin passent toutes
  par le trait → code body identique sur les 2 OSes.
- `InputSource`, `input_source`, `midi_input`, `midi_event_rx` étendus
  `cfg(any(macos, windows))`. Le port virtuel keepalive Mac (CoreMIDI)
  reste `cfg(target_os = "macos")`.

## [0.3.2] — 2026-05-15

### Changed — Polish Windows beta
- **Latence capture smart** : `capture.rs` pré-check via
  `device.supported_input_configs()` si le device expose `BufferSize::Range`
  qui contient 128. Si oui (ASIO Windows + CoreAudio mac + parfois WASAPI
  exclusive Win 11) → `Fixed(128)` = ~2.7 ms latence. Sinon (mic onboard
  Realtek WASAPI shared) → fallback `Default` = ~10 ms. Donne la latence
  pro aux users avec carte audio externe sans casser les users sans.
- **Format d'install Windows recommandé = MSI** : nouvel asset stable
  `Jamodio-Audio-Engine-Windows.msi` publié sur chaque release. Site web
  jamodio.com pointe désormais sur ce MSI au lieu du `.exe` NSIS. Le MSI
  utilise le Restart Manager Windows = pas de "Error opening file for
  writing" lors de l'auto-update (bug observé en v0.3.0/0.3.1).
  Le `.exe` NSIS reste publié pour rétro-compat des installs existantes.

### Notes
- **Beta testers Windows v0.3.0/0.3.1** : votre installation NSIS continuera
  de s'auto-updater (le NSIS reste dans le bundle). Pour bénéficier des
  updates lisses sans Retry, désinstaller l'agent actuel et réinstaller via
  le nouveau `Jamodio-Audio-Engine-Windows.msi` depuis jamodio.com (1 fois).

## [0.3.1] — 2026-05-15

### Fixed — Beta Windows critical
- **Capture micro Windows onboard** : remplacé le `SampleRate(48000)` forcé
  par le SR natif du device + resampling Rust via `rubato 0.16` vers 48 kHz
  avant Opus encode. Sur Windows WASAPI shared mode, le mix format Realtek
  onboard impose 44100 Hz et refusait l'ouverture CPAL en 48 kHz (erreur
  `The requested stream configuration is not supported by the device`).
  Mac CoreAudio masquait le problème via resampling implicite. Désormais
  fonctionne sur tout device, indépendamment du SR natif.
- **Buffer CPAL Windows** : `BufferSize::Default` sur Windows (WASAPI
  shared mode impose ~10 ms minimum) au lieu de `Fixed(128)`. Mac garde
  `Fixed(128)` (~2.7 ms latence). Sur Windows ASIO le buffer est de toute
  façon piloté par le control panel ASIO.

### Fixed — Polish
- **Plus de console CMD au démarrage Windows** : ajout
  `windows_subsystem = "windows"` en release (préservé en dev pour le diag).

### Notes
- Latence resampler Rubato Sinc : ~5.8 ms (sinc_len=256 / 44.1 kHz). Dominé
  par le buffer WASAPI shared 10 ms de toute façon. Bypass total quand
  `native_sr == 48000` (mac, cartes pro Windows).
- Aucun changement comportemental côté binaire mac (resampler bypass +
  buffer Fixed(128) inchangé).

## [0.3.0] — 2026-05-15

### Added
- **Support Windows x64** (ASIO + WASAPI via cpal). Première release
  multi-plateforme depuis v0.1.1. Le runner `windows-2022` est réactivé
  dans le matrix CI, l'EXE NSIS est publié sous le nom stable
  `Jamodio-Audio-Engine-Windows.exe` aux côtés du DMG mac.

### Changed
- **SRTP backend split par plateforme** :
  - macOS / Linux : libsrtp2 + OpenSSL vendorés (inchangé, code wrapper
    renommé en `srtp_libsrtp.rs` sans modification fonctionnelle).
  - Windows : `webrtc-srtp` pure Rust (nouveau wrapper `srtp_webrtc.rs`,
    même API publique). Contournement du bug ABI `unsigned long` LP64/LLP64
    du crate `srtp 0.7` (repo upstream HyeonuPark/srtp mort depuis
    2020-12). Aucun changement côté binaire mac.
- CI Windows allégé : plus besoin de `vcpkg install openssl libsrtp` (pure
  Rust). Conserve `vcpkg install opus` + ajout step `choco install llvm`
  + téléchargement ASIO SDK Steinberg pour la feature `cpal asio`.

### Notes
- Plugins INSERT (AU host) restent macOS-only — phase 2 = host VST3 pour
  Windows, post-beta.
- macOS Intel reste désactivé (cross-compile libsrtp2 cassé, runner
  macos-13 saturé). À traiter séparément.

## [0.2.25] — 2026-05-13

### Hotfix — Crash agent au boot avec entitlements v0.2.24

v0.2.24 a introduit les entitlements `disable-library-validation` +
`allow-jit` qui permettent de charger les plugins AU 3rd party. Effet de
bord : ces plugins se chargent maintenant **in-process** dans notre agent,
au lieu d'être rejetés. Et certains plugins (FIN-NEO d'UJAM observé chez
Yannick) ont des destructeurs C++ buggés qui throw une exception
(`std::thread::~thread()` quand le thread n'a pas été join), ce qui
déclenche `std::terminate()` → crash de l'agent.

Le crash se produisait pendant le SCAN au boot, parce qu'on probe chaque
plugin pour récupérer sa latence et son nombre de bus input (via
`[[AUAudioUnit alloc] initWithComponentDescription:options:error:]`).
L'allocation transitoire pour la mesure suffisait à instancier puis
détruire le plugin → trigger le destructeur foireux.

### Fix

Dans `au_host.mm::scanAndCallback:`, ne probe désormais que les plugins
**Apple natifs** (manufacturer == `'appl'` = 0x6170706c). Pour les 3rd
party, valeurs par défaut sûres :
- `latency_samples = 0` (pas filtré comme incompatible)
- `has_input_bus = 1` (traité comme effet par défaut)

Trade-off : on perd le filtrage automatique des plugins 3rd party à
latence intrinsèque > 64 samples, et l'auto-switch MIDI ne se déclenche
qu'au load réel (pas au scan). Mais l'agent boot toujours, même avec un
plugin défectueux installé sur la machine.

Le user paye encore le prix au LOAD réel d'un plugin défectueux (qui
peut crasher l'agent — comme dans Logic Pro), mais le scan est fiable
et la majorité des plugins fonctionneront.

## [0.2.24] — 2026-05-13

### Entitlements AU host — vraie cause du bug Yannick (v0.2.23 ne suffisait pas)

Bug toujours présent en v0.2.23 malgré le main-thread dispatch + fallback
v3↔v2 : sur Mac M-series macOS 15.7.5, **TOUS** les plugins 3rd party
(BFD Player, AmpliTube 5, Cherry GX-80, UJAM, Splice…) échouaient avec
`OSStatus -1` sur les DEUX chemins. Apple natives marchaient.

Grâce au logging 4-CC ajouté en v0.2.23, on a pu confirmer le pattern :
le -1 vient du **hardened runtime + library validation** de Sequoia. Sans
les bonnes entitlements, macOS refuse de charger des dylibs/bundles AU
signés par un autre team ID que le nôtre → AudioComponentInstanceNew et
`[AUAudioUnit alloc init…]` retournent -1 sur tout plugin 3rd party.
Apple natives (AUSampler, AUMatrixReverb…) sont chargés in-process et
bypass la validation, c'est pourquoi ils fonctionnaient.

### Changements

`entitlements.plist` : ajout des 4 clés requises pour héberger des AU
3rd party sous hardened runtime macOS 15+ :

- `com.apple.security.cs.disable-library-validation` — **LA clé**.
  Permet de charger des libs signées par un autre team ID.
- `com.apple.security.cs.allow-jit` — pour les plugins NI/UAD/Waves
  modernes qui utilisent du JIT pour leur DSP.
- `com.apple.security.cs.allow-unsigned-executable-memory` — pour les
  pages mémoire JIT marquées exécutables.
- `com.apple.security.cs.allow-dyld-environment-variables` — pour BFD,
  Kontakt, EastWest qui utilisent DYLD_* pour pointer vers leurs samples.

C'est le pattern standard pour tout host AU (Logic Pro, Ableton Live,
Bitwig, Reaper, MainStage utilisent tous ces entitlements).

### Tests

Pas de nouveau test unitaire (entitlements ne s'appliquent qu'au binaire
signé). Validation manuelle Yannick au déploiement v0.2.24.

## [0.2.23] — 2026-05-13

### Sprint robustesse plugin AU — fix bug Yannick (BFD + AmpliTube `-1`)

Plusieurs plugins lourds (BFD Player, AmpliTube 5, Kontakt, etc.) plantaient
silencieusement avec `v2 InstanceNew failed: -1` chez certains utilisateurs,
alors que `auval` les validait à 100 %. Diagnostic : on appelait
`AudioComponentInstanceNew` depuis un tokio worker thread sans CFRunLoop, ce
qui faisait timeouter le XPC interne du plugin (licensing daemon, sample
engine background, etc.). Sur Mac assez rapides l'XPC se résolvait avant
l'inspection, sur Mac plus chargés ça plantait.

### Changements

- **(b)** `au_host.mm` : tout le chemin d'instanciation AU (v2 et v3) est
  désormais wrappé dans un `dispatch_sync(dispatch_get_main_queue(), …)` via
  l'helper `jmo_run_on_main_sync`. Inline si déjà sur le main thread (évite
  le deadlock `dispatch_sync` ↦ main quand on est main). Couvre
  `AudioComponentInstanceNew`, `AudioUnitSetProperty` (format, render
  callback, max frames), `AudioUnitInitialize`, et l'équivalent v3
  `AUAudioUnit alloc`, `setFormat`, `allocateRenderResources`.
- **(c)** Fallback v3 ↔ v2 : si le chemin préféré (selon le flag
  `kAudioComponentFlag_IsV3AudioUnit`) échoue, on retente l'autre chemin
  automatiquement. Beaucoup de plugins publient les deux interfaces avec des
  comportements différents — l'un peut planter là où l'autre passe.
  L'erreur finale annexe les deux messages (« primary: … (fallback: …) »).
- **(a)** `ws_server.rs` : `LoadInstrumentPlugin failed` loggue maintenant
  `au_type`, `subtype`, `manufacturer` et `error`. Plus de ping-pong Chrome
  console pour identifier le plugin tenté au prochain bug report.
- **(e)** `PluginInfo` gagne `#[serde(rename_all = "camelCase")]`. Le wire
  est maintenant cohérent : `pluginRef`, `latencySamples`, `hasEditor`,
  `hasInputBus` partout (avant : snake_case dans `PluginList`, camelCase
  dans `InstrumentPluginLoaded` — inconsistance subtile qui rendait le
  debug Chrome console déroutant). Browser lit les deux formes pendant la
  transition (compat agent ≤ v0.2.22 ↔ navigateur v0.2.23).
- **(d) côté browser** : nouveau cache `localStorage:jamodio-fx-failures`
  des plugins ayant échoué au load. Pas de grisage — juste un badge ⚠
  informatif dans la modal FX (cliquable, tooltip explicatif).
  Invalidation intelligente :
  - TTL 30 jours (laisse une chance à une MAJ du plugin)
  - Effacement si la version de l'agent change (peut-être qu'on a fixé)
  - Effacement immédiat dès qu'un load réussit
  L'utilisateur n'est jamais bloqué, et bénéficie automatiquement des
  fixes futurs.

### Tests

- Nouveau `plugin_info_serializes_camel_case` dans `jamodio-audio-core` :
  vérifie que la sérialisation est en camelCase et qu'aucun champ
  snake_case ne fuit.
- Tests existants `au-host` passent inchangés (process_passes_through_eq,
  double_process_keeps_working).

## [0.2.22] — 2026-05-12

### Bug D — Crossfade au drift drain du jitter buffer

Le `JitterBuffer::pull` (`ring_buffer.rs`) ramenait la latence à target en
droppant brutalement les samples excédentaires quand `available > 3 ×
target_samples` → discontinuité PCM audible ("clic") sur sessions
multi-peers. Le drain reste nécessaire pour borner la latence post-burst
ou post-drift clock skew, mais on lisse maintenant la transition.

- `crossfade_tail: Vec<f32>` conserve les 480 samples interleaved (= 5 ms
  stéréo à 48 kHz) les plus récents de la zone drainée.
- Sur les pulls suivants, fade-out de ce tail vs fade-in des samples
  poppés : `out[i] = tail[i] · (1 − t) + out[i] · t`. Le fade s'étale
  automatiquement sur plusieurs pulls si la callback CPAL livre des
  blocs plus courts que 5 ms.
- **Zéro latence ajoutée** : `target_samples` du jitter buffer inchangé,
  le crossfade vit uniquement dans le moment du drain.
- Log : `tracing::warn!` filtré sur `events > 4 && is_power_of_two` →
  warns à 8, 16, 32… au lieu de 1, 2, 4, 8, 16… (spam log -70 %).
- 2 tests unitaires : `drift_drain_no_audible_discontinuity` (push
  échelon ±1, vérifie `max(|step|) < 0.20` — un drain sec donne 2.0)
  + `drift_drain_counts_all_dropped_samples` (drift_drops compte
  pre_drop + tail).

## [0.2.21] — 2026-05-12

### Alignement avec les hotfixes browser MIDI (S2.10)

Bump d'alignement de version après les hotfixes côté browser (piano
flottant + glissando + raccourcis clavier + 4 octaves + VU SELF
post-plugin en mode MIDI + bouton "Rafraîchir MIDI" pour hot-plug).
Côté agent, pas de changement de comportement — bump uniquement pour
garder browser et agent en phase.

## [0.2.20] — 2026-05-12

### Piano flottant + glissando + raccourcis clavier + 4 octaves (S2.10)

Évolutions du clavier virtuel HTML côté agent (port virtuel MIDI
"Jamodio Virtual MIDI") : fenêtre piano détachable et draggable,
glissando (clic-drag pour enchaîner les notes), raccourcis clavier
QWERTY (A W S E D F T G Y H U J → C D E F G A B sur 1 octave + Z X
pour transposer ±1 octave), extension de 2 à 4 octaves.

## [0.2.19] — 2026-05-12

### Clavier virtuel MIDI HTML 2 octaves + badge MIDI (S2.9 browser)

Côté browser : nouveau clavier virtuel HTML sur 2 octaves intégré à la
modal Source instrument. Affiche un badge MIDI quand un device MIDI
externe ou virtuel est actif. Communique avec l'agent via le handler
WS `PlayMidiNote { note, velocity, on }` côté Rust.

## [0.2.18] — 2026-05-12

### Port virtuel MIDI "Jamodio Virtual MIDI" + UX Audio/MIDI radio (S2.7)

Le port MIDI virtuel `Jamodio Virtual MIDI` est créé automatiquement
au boot agent (via `coremidi-rs`). Visible dans Logic Pro / Ableton
Live / GarageBand côté DAW. Permet à un DAW externe de driver les
plugins instruments hébergés dans Jamodio sans clavier USB physique.
Côté browser : la modal Source instrument bascule via radio
Audio / MIDI (au lieu d'un toggle ambigu) pour clarifier le mode.

## [0.2.17] — 2026-05-12

### MIDI input pour les plugins instruments (S2 backend)

Routing MIDI complet côté agent. Nouveau `InputSource { Audio, Midi(device_id) }`
dans `PipelineState`. En mode MIDI, l'`encoder_thread` force les samples
audio à zéro mais conserve le tick CPAL 48 kHz / 128 samples. Drain
max 64 events MIDI par bloc Opus (120 samples) depuis le channel
`midir` → passe au plugin via `process_stereo(handle, audio_zeros,
&midi_events)`. Le plugin instrument génère l'audio → Opus → RTP
peers + self-monitor. Wire `SetMidiInput { device_id }`.

## [0.2.16] — 2026-05-12

### Fenêtre plugin non resizable (S1.11)

Petit hotfix UI : la fenêtre native macOS du plugin (AUGenericView ou
CocoaUI custom) n'est plus resizable par l'user. La taille est fixée
par la `requestedSize` reportée par le plugin. Évite de casser des
layouts custom (AmpliTube notamment) qui ne s'adaptent pas
correctement aux contraintes de redimensionnement.

## [0.2.15] — 2026-05-12

### `autoresizingMask` sur la view du plugin (S1.10)

Hotfix layout : `autoresizingMask = NSViewWidthSizable |
NSViewHeightSizable` sur la view racine du plugin → quand la fenêtre
parente est redimensionnée par le plugin (cas TONEX qui change sa
taille au load), la view interne s'adapte.

## [0.2.14] — 2026-05-12

### Scan multi-types + détection audio-in au load (S1.9)

Le scan AU couvre maintenant tous les types (`aufx`, `aumu`, `aumi`,
etc.) au lieu de filtrer aux effects-only. Cas pratique : AmpliTube
est listé en `aumu` hybride avec audio in → était exclu par le scan
v0.2.13 strict. Au load, on détecte audio-in via
`au.inputBusses.count > 0` (v3) ou `kAudioUnitProperty_ElementCount
scope:Input` (v2). Sans bus input → skip `setFormat input` + render
callback (plugin instrument MIDI chargeable, silencieux sans MIDI).
Résout le `setFormat in: -10877 InvalidScope` pour PIANO.

## [0.2.13] — 2026-05-12

### Scan effects-only + CocoaUI v2 custom path (S1.8 hotfix)

Path v2 custom UI : `openEditor:` tente d'abord
`kAudioUnitProperty_CocoaUI` qui retourne un bundle URL + class
name. Charge le bundle dynamiquement (`NSBundle bundleWithURL:` +
`load`), instancie la factory class (conforms à `AUCocoaUIBase`),
appelle `uiViewForAudioUnit:withSize:`. Fallback `AUGenericView` si
pas de CocoaUI. Validation utilisateur : UI TONEX et AmpliTube
fonctionnelles en path v2.

## [0.2.12] — 2026-05-12

### Path hybride AU v2 / v3 pour custom UI plugins v3 (S1.7 hotfix)

AmpliTube 5 affichait un AUGenericView (sliders bruts) au lieu de son
UI 3D. Cause : AmpliTube 5 = AU v3 dont l'UI custom est uniquement
accessible via `requestViewControllerWithCompletionHandler:` sur
`AUAudioUnit`. Détection au load via `componentFlags &
kAudioComponentFlag_IsV3AudioUnit`. Path v3 → `AUAudioUnit` +
`scheduleMIDIEventListBlock` + custom UI ; path v2 →
`AudioComponentInstance` + `AudioUnitRender` + `AUGenericView`.
Instance partagée GUI ↔ processing dans chaque chemin.

## [0.2.11] — 2026-05-12

### Sprint S1 INSERT plugins natifs AudioUnit (S1.1 à S1.5)

La mixette agent peut maintenant héberger des plugins natifs
AudioUnit (effets, instruments MIDI) sur la tranche instrument self,
avec UI custom du plugin ouverte dans une fenêtre native macOS.

- **S1.1 — Trait `PluginHost`** : module
  `jamodio-audio-core/src/plugin_host.rs` avec types partagés
  (`PluginInfo`, `PluginRef`, `PluginHandle`, `MidiEvent`).
  Constante `MAX_PLUGIN_LATENCY_SAMPLES = 64`. Archi cross-platform :
  impl macOS = `jamodio-au-host`, impl Windows future =
  `jamodio-vst3-host`.
- **S1.1 — Nouveau crate `jamodio-au-host`** (~790 LoC) : C API
  Objective-C++ (`au_host_create/scan/load/unload/process_stereo/
  dispatch_midi/open_editor/close_editor`) via `build.rs`
  + `cc::Build::cpp(true)`. Wrapper Rust `AuHost` impl `PluginHost`.
  Frameworks : AudioToolbox, AudioUnit, CoreAudio, CoreAudioKit,
  CoreMIDI, AVFoundation, AppKit. 6 tests unitaires.
- **S1.2 — `process_stereo` dans le capture path** :
  `encoder_thread` route le signal instrument self à travers le
  plugin entre `remap_to_stereo` et `accumulator`. Self-monitor
  entend le WET. Sous-blocs de 128 samples par canal pour respecter
  `maximumFramesToRender`.
- **S1.3 — Protocole WS + scan cache background** : 6
  `BrowserMessage` (`list-plugins`, `load-instrument-plugin`,
  `unload`, `set-bypass`, `open-editor`, `close-editor`) + 4
  `AgentMessage`. Scan AU au boot agent, ~122 ms typique.
- **S1.4 — Hotfixes critiques** : `bus.enabled = NO` par défaut sur
  AUAudioUnit neuve → fix `[bus setEnabled:YES]` explicite
  (`-10876 NoConnection`). Window plugin ouverte derrière Chrome →
  `[NSApp activateIgnoringOtherApps:YES]`. `providesUserInterface`
  forcé à `true` (AUGenericView fallback). Refactor en API C legacy
  partout (`AudioComponentInstanceNew` + `AudioUnitRender` +
  `AURenderCallback` C + `AUGenericView initWithAudioUnit:`) pour
  partager l'instance entre processing et GUI (l'AUAudioUnit cache
  son AudioComponentInstance sous-jacente).
- **S1.5 — Sync state au reconnect WS** : au connect WS, l'agent
  push automatiquement `InstrumentPluginLoaded` si un plugin est
  déjà chargé. Browser persiste le dernier plugin dans
  `localStorage 'jamodio-fx-self'` et tente un re-load auto après
  800 ms si l'agent ne push pas (cas agent redémarré).

Pièges hosting AudioUnit appris dans la mémoire `au_hosting_pitfalls.md`.

## [0.2.10] — 2026-05-11

### DIM ducking instruments (SetDim) + correction sémantique tap REC

Nouveau bouton DIM sur la tranche Voix côté browser : quand l'utilisateur
veut entendre la conversation talkback clairement, il active DIM →
les instruments s'atténuent de -12dB (sans toucher backing/métro/voix
qui restent à plein volume).

- `AudioMixer::dim_factor: f32` (default 1.0, range [0.0, 1.0]).
- `set_dim(factor)` avec clamp défensif.
- Appliqué dans `mix_into` APRÈS la somme des streams et AVANT master_gain.
- Nouveau wire `BrowserMessage::SetDim { factor }` + handler.

Correction sémantique du tap REC :
- Avant v0.2.10 : push_mix(output) AVAIT master_gain et clamp (= mix
  tel qu'écouté, incluant les réglages d'écoute locaux).
- Maintenant : push_mix(output) AVANT dim_factor + master_gain + clamp.

Sémantique : le fichier MIX enregistré reflète "le mix post-fader des
instruments seul" (= ce qu'un peer théorique entendrait), indépendant
de mes réglages d'écoute locaux dim/master. Cohérent avec le tap
browser sur `instrumentMixBus` qui est aussi pre-dim/pre-master côté
Web Audio.

## [0.2.9] — 2026-05-11

### Pan L/R par stream en mode agent (SetPan)

Bug remonté post-v0.2.8 : le pan L/R des tranches instrument
(self + peers) n'agissait pas en mode agent. Cause double :
- `selfPanNode` / `p.panNode` (StereoPannerNode Web Audio) silencieux
  en mode agent (les flux passent par CPAL agent, pas audioCtx).
- Côté Rust, le mixer n'avait aucune notion de pan par stream —
  il sommait les samples stéréo sans répartition L/R.

Fix :
- `StreamState::pan: f32` (default 0.0, range [-1.0, 1.0]).
- `AudioMixer::set_pan(producer_id, pan)` avec clamp défensif.
- Constant-power panning dans `mix_into` :
    angle = (pan+1) · π/4 ∈ [0, π/2]
    gain_L = cos(angle), gain_R = sin(angle)
  Puissance totale constante (-3dB au centre), évite le drop de
  volume perçu au centre des panners linéaires. Fast path si pan≈0
  (skip cos/sin et boucle simplifiée — cas par défaut majoritaire).
- Nouveau wire `BrowserMessage::SetPan { producerId, pan }`. Convention
  producer_id="self" pour le self-monitor (= SELF_MONITOR_ID), sinon
  producer_id agent du peer (= agentMusicProducerId côté browser).

Le browser envoie set-pan à chaque mouvement de slider PAN ET au
capture-started (sync initial via applySoloMute) pour qu'une reconnexion
agent ne reset pas les pans à 0 si l'UI était à L40 / R30 d'avant.

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
