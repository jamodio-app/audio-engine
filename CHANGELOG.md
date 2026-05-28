# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).

## [Unreleased]

## [0.4.12] — 2026-05-28

### Fixed — Chantier A : chargement/déchargement de plugin non-bloquant

**Glitches audio pendant un swap de plugin** (remontés par la session test
28/05, Mac Mini M1, AmpliTube/BFD/Piano). Diagnostic via `agent.log` :
les 3 seuls bursts de drops de la session coïncidaient **exactement** avec
un load/unload de plugin, avec un `process_max` de **2,6 à 4,1 secondes**
(= le thread audio gelé pendant tout l'init/teardown natif).

Cause : `load`/`unload` tenaient `plugin_host.lock()` (et le lock
`PipelineState`) pendant l'init/teardown natif AU (0,4–4 s sur les gros
plugins). Le thread audio attendait ce même lock → débordement du ringbuf
de capture → drops → glitch. (Et perfstats_task était bloqué → trous dans
les métriques.) **AmpliTube/BFD ne sont pas en cause** : en régime établi
ils tournent à p99 ~1,7 ms, bien sous le budget.

Fix en deux pièces :

1. **Thread audio : `try_lock` au lieu de `lock`** (process stage). En
   régime établi le thread audio est seul à prendre le lock → `try_lock`
   réussit toujours (zéro changement, zéro latence ajoutée). Pendant un
   (dé)chargement, `try_lock` échoue → **dry passthrough** ce bloc (signal
   sec, aucune coupure) au lieu de bloquer. La file MIDI est purgée dans
   les chemins dry pour éviter un burst d'events périmés au prochain plugin.

2. **Load/unload hors du lock `PipelineState`.** Nouveau `PluginControl`
   (bundle d'`Arc`, clone cheap) : le handler WS clone le bundle, relâche
   le lock `PipelineState`, puis exécute l'opération native lente sur
   `spawn_blocking`. Le handle est posé à `None` AVANT le travail natif
   (→ dry instantané), puis à `Some(h)` une fois prêt (→ wet). Sérialisé
   via `PLUGIN_OPS_LOCK` (jamais deux init/teardown natifs concurrents).
   Supprime les `pipeline.lock() timeout — skipping handler` pendant un load.

Résultat attendu : charger/changer/retirer un plugin **n'interrompt plus
jamais l'audio** (dry le temps du load, puis wet). Plugin-agnostic, aucun
nom en dur. Le crash fix v0.4.11 (teardown sur le main thread) est conservé.

### Notes

- cargo test --workspace : 32 verts (dont 4 nouveaux tests `PluginControl`
  chargeant de vrais AudioUnits Apple — load/unload/swap/échec).
- **Latence inchangée** : hot path identique (`lock`→`try_lock`, même coût
  non-contendu). Les opérations lentes sont sur le cold path (spawn_blocking).
- Validation « 0 drops pendant un load » à confirmer on-device (nécessite
  le hardware audio) — la session de test utilisateur la couvrira.
- Parité Windows (VST3) du non-blocking à valider lors de la session Windows.

## [0.4.11] — 2026-05-28

### Fixed — Crash unload AU + bypass plugin trop agressif

Deux bugs bloquants remontés par la session de test du 28/05 (Mac Mini
M1, AmpliTube 5 + BFD Player). **AmpliTube et BFD sont des plugins
stables : les deux bugs étaient de NOTRE côté**, pas dans les plugins.

#### Crash « JAMODIO AUDIO ENGINE a quitté de manière imprévue »

Cause (diagnostic via le rapport `.ips`) : SIGSEGV null-deref sur le
**main thread**, dans le timer `CFRunLoop` interne du plugin
(`BFDPlayer::MessagePort::ProcessEditorMessages` via `ATC_Tick`).

L'ancien `unload` disposait l'`AudioUnit` **depuis le thread WS** (≠ main)
tout en fermant l'éditeur en `dispatch_async` (= plus tard). Les gros
plugins enregistrent un timer périodique sur le main runloop **dès le
load** (pas seulement à l'ouverture de l'éditeur) : disposer l'instance
pendant que ce timer accède encore aux objets internes = use-after-free.

Fix (`au_host.mm`) : tout le teardown (close editor → uninitialize →
dispose) s'exécute désormais **synchroniquement sur le main thread** via
le helper `jmo_run_on_main_sync` (= celui déjà utilisé par le load).
Le main runloop sérialise : le teardown s'exécute ENTRE deux itérations,
jamais pendant un timer callback du plugin. Le helper gère aussi le cas
test/CLI (`NSApp == nil` → exécution inline, pas de deadlock `cargo
test`) et garde un filet de sécurité timeout 10 s.

#### Plugin bypassé à tort (« son guitare comme si BYPASS » + silence MIDI)

Cause (diagnostic via `agent.log`) : la détection d'overload S5
(`v0.4.7`) déclenchait le bypass auto sur le **seul** critère
`plugin p99 > 4 ms`. Or sur la session 28/05, AmpliTube 5 et BFD Player
tournaient à p99 4-6 ms **avec `drops_per_sec = 0`** (= le ringbuf S3
absorbait, audio nickel). Ils étaient donc bypassés alors qu'ils
fonctionnaient parfaitement → l'utilisateur entendait le signal DRY
+ silence total en mode MIDI (BFD bypassé = pas de son).

Le seuil p99 absolu est **hardware-dépendant** : 5 ms par bloc est viable
sur une machine, pas sur une autre. Le seul signal fiable de « le plugin
sature VRAIMENT » = `capture_drops > 0` (= le callback CPAL ne peut plus
pousser ses samples car l'encoder est durablement bloqué).

Fix (`ws_server.rs`) : le bypass auto ne se déclenche désormais que si
les **trois** conditions cumulatives sont réunies :
1. `capture_drops_window > 20/s` (= vraies coupures soutenues, pas 1-2
   blocs isolés) — **hardware-agnostic** ;
2. plugin actif et `p99 > 3 ms` sur ≥ 50 blocs (= il consomme un temps
   significatif → candidat coupable, vs drops venus d'ailleurs) ;
3. pas déjà bypassé (anti-spam, flag reset au load/unload/toggle user).
On bypasse quand ça coupe RÉELLEMENT, pas quand un plugin lourd mais
viable prend 5 ms par bloc. Plugin-agnostic : aucun nom en dur.

Quand un bypass plugin part sur une fenêtre de drops, le toast générique
`AgentPipelineOverload` est supprimé pour le même tick (= une seule cause
remontée à l'UI, pas deux toasts pour le même évènement).

### Notes

- cargo test --workspace : 28 verts (dont `load_unload_aumatrixreverb`).
  Le helper `jmo_run_on_main_sync` côté unload corrige aussi un **deadlock
  `cargo test`** latent : un `dispatch_sync(main)` brut depuis un thread
  worker de test pendait indéfiniment (NSApp nil → main queue non pompée).
- Aucun impact latence : hot path inchangé (process lock-free), les deux
  fix sont sur le cold path (unload + perfstats_task 1 Hz).

## [0.4.10] — 2026-05-27

### Added — Sprint S6 (partiel) : détection peer instable + re-switch HD

**Dernier sprint avant BETA** du chantier stabilité agent
(PLAN-EXECUTION-AGENT-STABILITE.md). Ferme le scope visible utilisateur
sur les sessions multi-peers.

S6.3 (anti-flap WS persistante) **différé en post-BETA** : v0.4.3
(slot single-client auto-recycle) a déjà résolu le bug fonctionnel
sous-jacent ; le cycle close/reconnect cosmétique reste sans impact UX.

#### S6.1 — Détection peer instable (drift drain bursts)

Côté agent :
- `AudioMixer::stream_unstable_events(window, threshold)` : nouvelle
  méthode publique qui purge la fenêtre glissante (`VecDeque<Instant>`
  par stream, alimentée à chaque drift drain dans `report_drift_drops`)
  et retourne les peers REMOTE au-dessus du seuil. Self-monitor exclu
  (= ses drains reflètent overload local, pas un peer distant).
- `ws_server perfstats_task` : à chaque tick 1 Hz, lit les peers
  instables et émet `AgentMessage::PeerUnstable { producer_id,
  drift_drains_window, drift_drains_total, drift_ppm }`.
  Anti-spam : 1× toutes les 30 s par `producer_id` (= si le peer reste
  instable, l'agent renvoie périodiquement pour signaler la situation
  continue ; sinon le badge UI disparaît après 60 s sans message).
- Seuil retenu : **> 16 drift drains sur fenêtre 30 s** (= ~1 par 2 s,
  cohérent avec le pattern Yannick observé en baseline 22/05).
- Log warn `jamodio::mixer` (= visible dans agent.log).

Côté browser :
- Handler `case 'peer-unstable'` dans `groupe.js handleAgentMessage` :
  recherche le peer via `p.agentMusicProducerId === msg.producerId`,
  set `p.unstable = true` + métadata, appelle `applyPeerUnstableBadge(p)`.
- Badge ⚠ flottant haut-droite de la tranche peer concernée
  (`.gr-ch-unstable-badge` + classe `.unstable` sur `.gr-channel`).
- Tooltip i18n FR + EN avec détail : "X envoie par à-coups (Y drains/
  30 s · Z ppm) — encoder saturé chez X ou wifi instable".
- Auto-clear : interval 5 s qui retire le badge si pas de nouveau
  message depuis 60 s (= peer s'est stabilisé).

#### S6.2 — Re-switch HD au retour de l'Audio Engine en session

Côté browser uniquement. Listener sur `agentStatus.on('change')` :
- Quand l'agent passe en CONNECTED ALORS QUE on est en session active
  (`currentRoomId !== null`) ET en mode fallback WebRTC
  (`!agentConnected`) ET on était précédemment en HD
  (`cm.wasInAgentMode === true`) ET pas déjà proposé sur cette session :
  - Affiche un toast non-intrusif "🎚 Audio Engine reconnecté — tu peux
    repasser en mode HD" avec bouton **"Repasser HD"** qui déclenche
    `tryFullRejoin('user-accepted-hd-reswitch')` (= existant) → cycle
    leave + rejoin + start-capture → audio HD restauré.
- Pas de migration automatique : confirmation user obligatoire (= éviter
  de couper l'audio pendant une jam si le retour agent est instable).
- Flag `_agentBackPrompted` empêche le spam (= 1 toast par session).

i18n FR + EN : `jam.agentBackReSwitch` + `jam.agentBackReSwitchAction`.

### Notes

- cargo test --workspace : 29 verts. cargo build --release Mac OK.
- Pas de régression latence attendue (= S6 est cold path WS, hors hot
  audio).
- Compat browser v0.4.1+ : message `peer-unstable` ignoré silencieusement
  par un browser ancien.
- Build matrix CI inchangée (Mac ARM + Windows x64 via tag `v*`).

### Backlog post-BETA (déféré)

- **S6.3 anti-flap WS persistante** : refactor `agentWs` en singleton
  `main.js` pour éviter le cycle close/reconnect à chaque navigation.
  3-4 h de dev, risque modéré, pas critique depuis v0.4.3.
- **Optimisation LoadInstrumentPlugin non-blocking** : background load
  + swap atomic pour éliminer le spike 100-3000 ms observé v0.4.9.
- **Test multi-thread `rt_priority` au CI** : éviter une régression
  type v0.4.5 (1 thread RT au lieu de 3).
- **Bench mock plugin sleep(20 ms)** : test artificiel S5 overload.

## [0.4.9] — 2026-05-27

### Added — Détection saturation pipeline (BFD-like) + watchdog 5s

Couvre le **blind spot** du plugin overload S5 identifié sur l'incident
BFD Player du 27/05 14:37 : encoder thread bloqué brutalement pendant
3-4 s pour cause de **sample-load** d'un plugin sampler (BFD, Kontakt,
etc.), sans que `plugin_latency` ne reflète le problème (les blocs CPAL
sont DROPPÉS avant d'atteindre `process_stereo`, donc pas mesurés).

#### Nouveau signal : `capture_drops > 100/s`

Dans `ws_server perfstats_task` (1 Hz), si le compteur `capture_drops`
flushé sur la fenêtre 1 s dépasse 100 :
- Émet `AgentMessage::AgentPipelineOverload { drops_per_sec,
  pipeline_p99_ms, plugin_name }`
- Distinct de `InstrumentPluginOverload` (S5) : **PAS de bypass auto**
  du plugin (qui peut être innocent — c'est un sample-load I/O ou un
  process tiers CPU)
- Anti-spam : 1× toutes les 10 s max (= évite le flood quand la
  saturation dure)
- Tracing warn `jamodio::ws` (= visible dans agent.log)

#### UX browser

Nouveau handler `case 'agent-pipeline-overload'` dans `groupe.js` :
- Toast warn 6 s (pas persistant, pas d'action) avec icône ⚠️
- Template i18n FR + EN : `"Agent saturé ({drops} drops/s) — ferme
  d'autres apps gourmandes ou choisis un plugin moins lourd"`
- Pas de bouton "Réactiver" (= rien à réactiver, juste informer)

### Changed — Browser watchdog 3 s → 5 s

`AGENT_WATCHDOG_TIMEOUT_MS: 3000 → 5000` dans `groupe.js`.

Justification : sur l'incident 27/05, le watchdog 3 s a tué la session
agent alors que l'incident BFD était transitoire (~3-4 s). 5 s tolère
ces hoquets sans rompre la session ; le toast `AgentPipelineOverload`
informe quand-même l'utilisateur. Le compromis :
- 3 s = trop sensible aux spikes plugin (faux positifs)
- 5 s = bonne marge pour sample-load I/O sans tuer le studio
- > 10 s = trop tolérant, l'utilisateur attend trop si vrai crash agent

### Notes

- cargo test --workspace : 29 verts.
- Pas de changement protocole côté `InstrumentPluginOverload` (S5).
- Compat browser v0.4.1+ : un browser ancien ignore simplement le
  nouveau type de message `agent-pipeline-overload`.

## [0.4.8] — 2026-05-27

### Added — Mesures perfstats par stage (capture/process/encode)

Suite à la session test v0.4.7 (Grand Piano + BFD Player) qui a montré
un `pipeline_max_ms = 113 ms` avec `plugin_max = 4 ms` (= plugin propre,
spike ailleurs), on a découvert que la métrique `pipeline_latency`
incluait le **temps passé en file** dans les ringbufs S3 entre stages.
Devenue trompeuse pour le diagnostic : pas moyen de discriminer
"vraie surcharge plugin" vs "stall en queue d'un autre stage".

#### Fix

3 nouveaux `Histogram` dans `PerfHandles` :

- `capture_latency` : temps de traitement PUR du `capture_stage_loop`
  (remap canal + resample éventuel), mesuré depuis `recv_timeout` (= pop
  `sample_rx`) jusqu'à juste avant `out_tx.send` (= AVANT entrée en
  file du ringbuf process). N'inclut **pas** le temps d'attente bloc.
- `process_latency` : temps de traitement PUR du `process_stage_loop`
  (input_cut, MIDI source, plugin INSERT, RMS, push self-monitor).
  Mesuré depuis pop `cap_to_proc_rx` jusqu'à juste avant
  `proc_to_enc_tx.send`. Inclut `plugin_latency` comme sous-ensemble
  (par sous-bloc dans `process_stereo`).
- `encode_latency` : temps de traitement PUR du `encode_stage_loop`
  (Opus encode + RTP build + try_send). Mesuré depuis pop
  `proc_to_enc_rx` jusqu'au dernier `try_send` RTP.

Chacun observé `.observe(elapsed_ms)` à chaque tour, flushé 1 Hz dans
`ws_server perfstats_task`.

#### Sémantique pour le diagnostic

```
pipeline_latency        = capture_in → encode_send (= ce qu'on entend)
sum(stages_max)         = capture_max + process_max + encode_max
queue_time_max          = pipeline_max − sum(stages_max)
```

- `queue_time_max ≈ 0` : stages bien découplés, pas de stall en queue.
- `queue_time_max ≫ 0` : un stage a stallé, accumulation dans ringbuf.
  Cas v0.4.7 BFD : `pipeline_max=113ms` − `process_max≈5ms` ≈ 108 ms en
  queue → un autre stage (probable mixer.lock) a bloqué brièvement.

#### Tracing log étendu

`tracing::info!(target: "jamodio::perfstats")` inclut maintenant 6
nouveaux champs : `capture_p99_ms`, `capture_max_ms`, `process_p99_ms`,
`process_max_ms`, `encode_p99_ms`, `encode_max_ms`. Visibles dans le
bug-report via `GetLogsArchive` existant.

#### Pas de changement protocole WS

Aucune modification de `AgentMessage::PerfStats` : le browser continue
à recevoir le même payload (= compat). Les nouvelles métriques sont
uniquement loggées dans `agent.log`, lues par
`scripts/agent-latency-baseline.js --compare` lors de l'analyse
post-session côté support.

#### Script baseline étendu

`scripts/agent-latency-baseline.js` parse les 6 nouveaux champs. Le
mode `--compare` affiche désormais une section "par stage" avec
`queue_time_max` calculé. Les baselines historiques (v0.4.1, v0.4.6)
n'ont pas ces champs → `stages: undefined` → la section est skipée
automatiquement (compat rétrograde).

### Notes

- cargo test --workspace : 29 verts.
- Coût mesure : 4 × `Instant::now()` supplémentaires par bloc dans le
  hot path (= ~120 ns sur Apple Silicon). Négligeable vs budget RT 2,7 ms.
- Pas d'impact sur la latence ressentie utilisateur.
- Future session test : on attend de voir si `process_max` reste sous
  5 ms (= plugin propre) tandis que `pipeline_max` pourra être plus
  élevé en cas de stall en queue (mixer.lock, etc.). Diagnostic fin
  réactivé.

## [0.4.7] — 2026-05-27

### Added — Sprint S5 : plugin overload guard + UX bypass auto

Protection automatique contre les plugins INSERT qui saturent le CPU.
Aucun impact sur la latence en charge normale (les chiffres v0.4.6
sont conservés). Critère métier : **plugin-agnostique** — fonctionne
pour AmpliTube, TONEX, neural amps, reverbs denses, etc. **sans
liste blanche/noire** ni hardcoding de nom.

#### Détection (agent)

Dans `ws_server perfstats_task` (= la tâche tokio 1 Hz qui flush déjà
les histogrammes), après chaque flush du `plugin_latency` :

- Si `p99_ms > 4.0` ET `count >= 100` ET pas déjà en bypass auto :
  - Set `instrument_plugin_bypass = true` (= signal dry sort,
    cohérent UX avec le bypass A/B manuel existant)
  - Set `plugin_auto_bypass_active = true` (= guard pour ne pas
    re-émettre tant que l'user n'a pas acté)
  - Log warn `jamodio::plugin` (visible dans `agent.log`)
  - Émet `AgentMessage::InstrumentPluginOverload { name, p99_ms,
    max_ms, count }` (= **après** le PerfStats pour que le browser
    voie d'abord les chiffres qui ont déclenché)

**Seuils choisis** :
- `p99 > 4 ms` (= 150 % du budget RT 2,7 ms à 48 kHz/128) : marge
  qui couvre les spikes acceptables (warm-up plugin, sample-load
  isolé) sans tolérer un plugin constamment hors budget.
- `count >= 100` : exclut le warm-up plugin (~10-20 premiers blocs
  souvent lents le temps que le plugin se "préchauffe"). Statistique
  fiable après ≈ 250 ms d'activité.

#### Reset (agent)

Le flag `plugin_auto_bypass_active` est reset à `false` (= un nouveau
message d'overload pourra être émis) sur :
- `LoadInstrumentPlugin` (= nouveau plugin = fresh start). Flush
  aussi l'histogramme `plugin_latency` pour ne pas mélanger les
  mesures du plugin précédent avec celles du nouveau.
- `UnloadInstrumentPlugin`
- `SetInstrumentPluginBypass` (= toggle manuel par l'user dans les
  deux sens : si l'user re-active manuellement et que le plugin
  re-spike, on l'avertit à nouveau).

#### UX browser

Nouveau handler `case 'instrument-plugin-overload'` dans
`groupe.js handleAgentMessage` :

- Toast persistant (`duration: 0`) non-dismissible auto, avec icône
  ⚠️ et action "Réactiver" qui send `SetInstrumentPluginBypass { false }`
- Texte template : `"⚠ {name} surcharge le CPU ({p99} ms) — bypass
  auto activé. Choisis un preset plus léger."` — i18n FR + EN.
  Le `{name}` est injecté depuis la payload agent, jamais hardcodé.
- Re-render du slot FX `self` pour afficher le badge bypass.
- Log warn structuré (`log.warn('plugin', 'overload détecté')`) pour
  que la trace remonte au bug-report.

#### Nouveau message protocole

```rust
AgentMessage::InstrumentPluginOverload {
    name: String,
    p99_ms: f32,
    max_ms: f32,
    count: usize,
}
```

(camelCase sur le wire : `name`, `p99Ms`, `maxMs`, `count`).

Pas de nouveau `BrowserMessage` — on réutilise `SetInstrumentPluginBypass`
existant pour le bouton "Réactiver".

### Notes

- cargo test --workspace : 29 verts.
- cargo build --release Mac OK. Smoke test toast UI vérifié en preview.
- Pas de bench mock dans cette version : le déclenchement réel
  nécessite un plugin lourd (TONEX, AmpliTube high-gain) qui n'est
  pas installé en CI. Le path overload sera validé en session BETA
  avec utilisateurs ayant des plugins variés. Test artificiel via
  un mock `PluginHost` qui sleep(5 ms) à ajouter en backlog post-S6.
- Compat browser v0.4.1+ (le browser ancien ignore simplement les
  messages WS de type inconnu).

## [0.4.6] — 2026-05-27

### Fixed — CRITIQUE : régression latence v0.4.5 (rt_priority guard global)

Régression introduite avec S3 (v0.4.5) sur la mesure 27/05 après-midi :
- `pipeline_p50_ms` : 0,120 (baseline v0.4.1) → **0,201** (+68 %)
- `pipeline_p99_ms` : 2,16 → **19,66** (+810 %, **× 9 pire**)
- `pipeline_max_ms` : 25,25 → 27,02 (+7 %)
- `drops_total = 0` (heureusement)

#### Cause racine

Le guard anti-double-promotion dans `audio::rt_priority::promote_thread_for_audio`
était implémenté comme un **`static AtomicBool` global** :

```rust
static PROMOTION_ACTIVE: AtomicBool = AtomicBool::new(false);
// ...
if PROMOTION_ACTIVE.swap(true, Ordering::SeqCst) { return no-op; }
```

Conçu en S2 pour signaler le bug d'usage "même thread appelle promote
deux fois sans drop", il **bloquait aussi les autres threads**. Avec
S3 split en 3 stages (capture/process/encode) appelant tous
`promote_thread_for_audio` en parallèle au boot, seul le 1er stage
(audio-capture) joignait le workgroup CoreAudio. Les 2 autres
(`audio-process` et `audio-encode`) tournaient en SCHED_OTHER → se
faisaient préempter par Chrome/Spotlight/etc. → spikes.

Confirmé dans le bug report 27/05 13:13 : `grep "thread promoted"`
retourne **1 seule ligne** au lieu de 3 attendues. Le `audio-process`
qui contient le plugin INSERT (= le hot path le plus sensible)
n'était pas RT.

#### Fix

Migration du guard `static AtomicBool` → **`thread_local!` Cell** :

```rust
thread_local! {
    static PROMOTION_ACTIVE: Cell<bool> = const { Cell::new(false) };
}
```

Le guard ne bloque QUE la double-promotion sur le MÊME thread (= cas
d'usage original). Chaque thread RT (capture/process/encode) peut
maintenant joindre le workgroup CoreAudio indépendamment.

#### Validation attendue post-v0.4.6

- 3 lignes "thread promoted to CoreAudio workgroup" dans `agent.log` au
  boot (une par stage)
- Retour aux chiffres v0.4.3 ou meilleurs (`p50 ≤ 0,07 ms`, `p99 ≤ 0,8 ms`)
- `drops_total = 0` conservé

#### Notes

- cargo test --workspace : 29 verts. Le test
  `rt_priority::tests::double_promotion_without_drop_yields_none`
  reste valide (= protection sur LE MÊME thread préservée).
- Pas de changement protocole. Compat browser v0.4.1+.
- Apologies pour le ship hâtif v0.4.5 sans détection de cette
  régression — le test sur le même thread ne pouvait pas la révéler,
  il aurait fallu un test multi-thread spawn 3 threads + verify chacun
  a son `RtPriorityHandle != None`. Ajouté en backlog post-S6.

## [0.4.5] — 2026-05-27

### Changed — Sprint S3 stabilité : split encoder pipeline en 3 stages

Refactor architectural important. **Aucun changement comportemental
visible utilisateur** (latence, audio, UI identiques). C'est une
**fondation** pour S5 (plugin guard + bypass auto) qui résoudra
définitivement les spikes plugin résiduels mesurés en v0.4.3 baseline.

#### Avant — encoder_thread monolithique

Un seul thread RT (`encoder_thread`) faisait séquentiellement sur chaque
bloc CPAL :
1. Receive depuis `sample_rx` (= CPAL callback)
2. Remap canal → stéréo
3. Resample (si Windows 44.1 → 48k, no-op sur Mac)
4. `input_cut` (silence toggle UI)
5. MIDI source override (zéro samples si mode MIDI)
6. **Plugin `process_stereo`** (AU mac / VST3 win, par sous-blocs 128)
7. RMS pour VU-mètre
8. Push self-monitor dans le mixer
9. Accumulate → Opus encode → RTP build → `try_send` UDP

**Conséquence** : si le plugin spike (sampler I/O, neural amp lourd…),
**toute la chaîne en aval est bloquée** : capture suivante en queue,
Opus en pause, RTP en silence. Sur la baseline v0.4.3, un spike plugin
de 22 ms se traduisait par 22 ms d'audio "silence" côté peer.

#### Après — 3 stages indépendants

```text
CPAL ─sample_rx─►  capture_stage  ─►ringbuf 32─►  process_stage  ─►ringbuf 32─►  encode_stage  ─►rtp_tx
                  (remap+resample)              (input_cut+midi+plugin+RMS+self)   (Opus+RTP)
```

Chaque stage tourne dans **son propre thread RT** (`audio-capture`,
`audio-process`, `audio-encode`). Chacun appelle
`rt_priority::promote_thread_for_audio` au boot → joint le workgroup
CoreAudio macOS / MMCSS Pro Audio Windows / SCHED_FIFO Linux.

**Ringbufs entre stages** : `crossbeam_channel::bounded::<TimedBlock>(32)`.
Capacité 32 × ~5,3 ms = **~170 ms de marge**. Absorbe un spike plugin
22 ms sans saturer (drops=0 garanti même en cas de cascade de spikes).

**Mesure `pipeline_latency` préservée** : le timestamp `Instant::now()`
est apposé par `capture_stage` en début de pipeline, transporté via
`TimedBlock = (Instant, Vec<f32>)` à travers les 3 stages, observé
final par `encode_stage` juste après `rtp_tx.try_send()`. La sémantique
de `pipeline_latency_ms` est donc **identique** à celle de v0.4.4 →
**la baseline v0.4.1 reste comparable**.

#### Stop propre

`Arc<AtomicBool>` partagé entre les 3 stages. Sur `stop_capture` :
1. Signal `stop_flag = true`
2. Chaque stage break en début de prochaine iteration
3. Join cascade amont→aval — quand `capture_stage` return, son
   `Sender` est drop → `process_stage` voit `Disconnected` sur son
   `recv`, drain ses samples en queue, return → idem pour `encode_stage`
4. Coût pire-cas du stop : ~170 ms (drainage des 2 ringbufs)

#### Ce que S3 résout

- **drops_total garanti à 0 même sous cascade de spikes** : avant, un
  spike plugin consécutif sur 30+ blocs aurait saturé le `bounded(64)`
  CPAL→encoder et causé un drop. Maintenant, le ringbuf entre stages
  protège chaque interface (capture, process, encode) indépendamment.
- **Architecture prête pour S5** : le `process_stage` isole le plugin.
  S5 ajoutera un timeout par bloc + bypass auto si plugin spike — sans
  toucher au reste du pipeline.

#### Ce que S3 ne résout pas (= S5)

**Le clic ponctuel sur un spike plugin isolé reste audible.** Quand le
`process_stage` est bloqué 22 ms par le plugin, l'`encode_stage` n'a
rien à manger → 22 ms de silence côté peer. C'est S5 qui le résoudra
via `bypass auto` du plugin si `p99 > 4 ms sur 1 s`.

### Notes

- `cargo test --workspace` : 29 verts (inchangé).
- `cargo build --release` Mac OK.
- Pas de nouvelle dépendance, pas de changement protocol WS.
- Compat binaire avec browser v0.4.1+.

## [0.4.4] — 2026-05-27

### Fixed — Log spam : promotion sur Close frame (cosmétique)

Détecté sur la session test v0.4.3 du 27/05 : 74 "external client promoted"
+ 73 "displacing previous external client" sur 15 min, alors que seulement
**2 vraies sessions** existaient (88 s et 120 s). Conséquence : logs
illisibles, drift apparent du slot. Pas d'impact fonctionnel (drops=0,
les vraies sessions tenaient), mais inutilisable pour analyser les
sessions BETA.

**Cause** : la condition de promotion `if !slot_taken && !is_internal`
se déclenchait sur **n'importe quel** `Message` reçu, y compris les
`Close` frames que les probes `agent-status.js` envoient en fermant
leur WS. Ces probes pourtant n'ont rien à voir avec une vraie session.

**Fix** :
- Promotion **uniquement sur Message::Text** qui se parse en
  `BrowserMessage` valide. Les Close/Ping/Pong/Binary sont ignorés
  pour la promotion (mais toujours traités par `handle_one_message`).
- Log "displacing previous external client" **uniquement si**
  `prev.send()` réussit (= un client était bien actif). Si le sender
  était stale (= receiver déjà drop, client précédent déjà cleanup),
  no-op silencieux : pas de log misleading, pas de pause 50 ms inutile.

### Notes

- Aucune régression fonctionnelle attendue : la sémantique de promotion
  est exactement la même qu'en v0.4.3, juste filtrée correctement.
- cargo test --workspace : 29 verts.
- Préreq pour la prochaine session test : confirmer en bundle que les
  logs `displacing` ne se produisent QUE sur les vrais cas de reconnect
  rapide (et non sur chaque probe agent-status.js).

## [0.4.3] — 2026-05-27

### Fixed — Slot single-client : kick automatique du précédent + watchdog

Résolution du bug "agent déjà utilisé" remonté en prod 27/05 (cf. mémoire
`agent_slot_libre_todo.md`). Initialement prévu pour S6 du chantier
stabilité, sorti en patch d'urgence pour débloquer les tests v0.4.2.

#### Symptôme

À chaque connexion au studio (alors que l'Audio Engine est ONLINE), le
browser reçoit `Rejected: another client is already connected`. Seule
solution : Quit + relance manuelle de l'agent depuis "Lancer" — encore
échouait à 1 essai sur 2. Cycle de frustration : impossible de tester
la version v0.4.2 sans cette répétition mécanique.

#### Cause racine

L'`encoder_thread` côté Rust gardait `client_active = true` après une
fermeture de tab brutale du browser (reload rapide, kill du Chrome,
fermeture sans `beforeunload`). La WS restait half-open côté agent
(TCP keepalive macOS = plusieurs minutes), le slot single-client était
verrouillé jusqu'au quit/restart du process.

#### Fix

Deux mécanismes ajoutés au `ws_server::handle_connection`, complémentaires :

1. **Promotion à la réception du 1er BrowserMessage** : la connexion WS
   ne prend PAS immédiatement le slot single-client. Elle entre en phase
   "pré-promotion" qui sert juste à recevoir le `Hello` agent → fermer
   (comportement standard des probes `agent-status.js`). Quand un **vrai**
   BrowserMessage arrive (typiquement le `HelloAck` envoyé par
   `groupe.js detectAgent()`), la connexion est promue : elle prend le
   slot et **kick le client précédent** s'il y en a un.

   Bénéfice : les probes `agent-status.js` ne kickent plus la session
   active (régression évitée du design initial qui n'avait pas cette
   distinction).

2. **Watchdog heartbeat 5 s post-promotion** : une fois le slot pris,
   la connexion attend des messages browser. Le heartbeat `get-stats`
   du browser arrive normalement toutes les 1.5 s
   (`groupe.js startAgentHeartbeat`). 5 s sans message = WS half-open =
   slot libéré automatiquement (pas d'attente du TCP keepalive système).

#### Mécanisme technique

- Nouveau champ `WsServerHandle.active_client_killer:
  Arc<parking_lot::Mutex<Option<oneshot::Sender<&'static str>>>>`. À la
  promotion d'un nouveau client, `replace()` du sender → si Some(prev),
  `prev.send("displaced-by-new-client")` → le client précédent voit son
  killer_rx déclencher dans son `tokio::select!` et break la receive
  loop → cleanup standard (stop_all + slot libre).
- Pause de 50 ms après le kick pour laisser le cleanup ancien finir
  (notamment `stop_all()` qui prend quelques ms).
- Helper `handle_one_message()` extrait pour partager le code entre la
  branche pre-promotion et post-promotion.
- Le `Rejected` message reste défini dans le protocole (compat browsers
  legacy) mais n'est plus jamais envoyé.

#### Log structuré

Le tag `jamodio::ws` log à `info` :
- `"client connected" is_internal=...` au open
- `"external client promoted (first BrowserMessage received)"` à la promotion
- `"displacing previous external client (stale slot)"` au kick d'un précédent
- `"client disconnected — cleanup" reason=<displaced|watchdog-timeout|
  ws-closed-normally|pre-promotion-idle|ws-error>` à la sortie

→ Le bug-report contient la trace exacte du comportement (qui kick qui,
qui timeout). Plus de "pourquoi le slot était bloqué".

### Notes

- Tests : `cargo test --workspace` : 29 verts (inchangé).
- Compatible avec le browser v0.4.1+ sans modif — la sémantique probe vs
  session est entièrement dérivée du comportement existant côté browser.
- Build matrix CI inchangée. Pas de nouvelle dépendance.

## [0.4.2] — 2026-05-27

### Changed — Sprint S2 stabilité : priorité RT effective (workgroup + MMCSS)

Cause racine R1 du chantier stabilité (cf. PLAN-EXECUTION-AGENT-STABILITE.md).
Avant ce sprint, `encoder_thread` appelait `thread_priority::Crossplatform(95)`
qui, sur macOS, se traduit en `pthread_setschedparam` avec une nice value que
Darwin ignore → SCHED_OTHER en pratique → préemptible par tout autre process
(Chrome, Spotlight, etc.). Baseline v0.4.1 du 27/05 a confirmé : spikes
`pipeline_max_ms` jusqu'à 25 ms alors que le budget RT est 2,7 ms/bloc.

S2 remplace ce mécanisme par les APIs natives OS dédiées au scheduling audio.

**Aucune dépendance d'API publique browser/WS** — le changement est purement
système. La nouvelle version est binaire-compatible avec un browser v0.4.1.

#### macOS — `os_workgroup_join` (HAL CoreAudio)

- Nouveau fichier `jamodio-au-host/cpp/audio_workgroup.mm` : binding ObjC++
  vers `os_workgroup_join` / `os_workgroup_leave`. Récupère le workgroup
  HAL via `kAudioDevicePropertyIOThreadOSWorkgroup` du device output choisi
  par l'utilisateur (match nominal case-insensitive, fallback default OS).
  Filtre les devices virtuels sans stream output (BlackHole input-only, etc.).
- ARC géré proprement via `__strong` implicite ObjC++ + `new`/`delete` C++
  pour la struct handle (cf. doc Apple sur ARC + structs C).
- Nouveau module Rust `jamodio_au_host::workgroup` : wrapper RAII
  `AudioWorkgroup::join_default()` / `join_by_name(name)`. `Send`, `!Sync`
  (token de leave thread-local). Drop = `os_workgroup_leave` auto.
- 3 tests unitaires (disponibilité, join default, join nom inexistant).

#### macOS — fallback QoS + THREAD_TIME_CONSTRAINT_POLICY

Si `os_workgroup_join` indisponible (macOS < 11, device virtuel sans
workgroup, thread déjà dans un workgroup) :
- `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)` : hint
  scheduler "travail user-facing prioritaire".
- `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)` : annonce explicite
  d'un budget déterministe au scheduler Mach. Paramètres alignés sur la
  frame Opus 2,5 ms : period 2,5 ms, computation 1,2 ms, constraint 2 ms,
  preemptible true (sécurité anti-deadlock système). Conversion ns → ticks
  via `mach_timebase_info` (mémoïsé via `OnceLock`).
- Bindings Mach raw via `mach2` + `libc` (déjà transitifs, ajoutés
  explicitement à la dep liste pour clarté).

#### Windows — MMCSS "Pro Audio"

- `AvSetMmThreadCharacteristicsW(L"Pro Audio")` au démarrage du thread
  encoder + `AvRevertMmThreadCharacteristics` au Drop. C'est l'API
  officielle MMCSS pour signaler un thread audio temps-réel (utilisée
  par tous les DAW pro depuis Vista).
- UTF-16 NUL-terminé statique pour "Pro Audio" (évite dépendance au
  macro `w!` du crate `windows`).
- Nouvelle dépendance `windows-sys = "0.59"` (features `Foundation`,
  `System_Threading`) sous `[target.'cfg(windows)']`.

#### Module chapeau `audio::rt_priority`

- `promote_thread_for_audio(output_device_name: Option<&str>) -> RtPriorityHandle`
  cache la sélection par OS. Tracing `info` (cible `jamodio::rt_priority`)
  loggue la méthode retenue (`macos-workgroup` / `macos-time-constraint`
  / `windows-mmcss` / `generic` / `none`). Le bug-report contient donc
  une preuve directe du chemin emprunté.
- Anti-double-promotion via AtomicBool global : si un thread re-appelle
  promote sans drop, log warn et retourne handle no-op (= bug d'usage,
  ne plante pas la prod). Reset auto au Drop.
- 3 tests unitaires : promote/drop safe, re-promotion après drop OK,
  double-promote sans drop = handle None.

#### Intégration `encoder_thread`

- Suppression du `thread_priority::set_current_thread_priority(Crossplatform(95))`.
- Ajout du nom du device output extrait du format `"{idx}:{name}"`
  (cf. mémoire `strict_device_id`) et passé à `rt_priority`.
- Le handle RT vit pour toute la durée du thread (drop en fin de boucle
  = leave/revert auto).

#### Cible chiffrée (à valider sur session test post-v0.4.2)

- `pipeline_p50_ms ≤ 0.120` (médiane stricte, baseline v0.4.1)
- `pipeline_p99_ms ≤ 2.661` (= baseline 2.161 + tolérance 0.5 ms)
- **`pipeline_max_ms < 5 ms` attendu** (vs 25.249 ms baseline) — c'est la
  métrique signature de la réussite de S2.
- `drops_total = 0` (déjà acquis en v0.4.1).

### Notes

- `cargo test --workspace` : 29 tests verts sur Mac (3 workgroup, 9 agent
  dont 3 rt_priority, 16 audio-core, 1 ignored).
- Build matrix CI inchangée — windows-sys et mach2 sont gated par
  `[target.'cfg(...)']` donc le Mac ne link pas Windows et inversement.
- Pas de changement de protocole WS : 100 % compatible browser v0.4.1.
- Préreq mesure post-merge : `node scripts/agent-latency-baseline.js
  --compare internal-docs/baselines/agent-v0.4.1-baseline.json <session-v0.4.2.txt>`
  doit exit 0.

## [0.4.1] — 2026-05-23

### Added — Sprint S1 stabilité : instrumentation profonde

Premier sprint du chantier "Fondations stabilité agent" (cf.
`internal-docs/PLAN-EXECUTION-AGENT-STABILITE.md`). Pas de changement
comportemental visible utilisateur — purement instrumentation pour
diagnostiquer les futures sessions et établir une baseline chiffrée
avant les refactors S2 (thread priority) et S3 (split encoder).

- **Nouveau module `jamodio-audio-core::perfstats`** : primitive
  `Histogram` glissante zero-alloc dans le hot path. Capacity fixe,
  ring buffer + tri scratch préalloué au flush. Tests unitaires couvrant
  ring overflow, percentiles, reset, drops independent. ~250 LoC + 7 tests.
- **`AgentMessage::PerfStats`** ajouté au protocole WS — snapshot 1 Hz
  des métriques pipeline (capture→send p50/p99/max, drops/s), plugin
  INSERT (process_stereo p50/p99/max si actif), peers (drift_ppm
  cumulatif, jitter buffer target_ms, underruns, drift_drops).
  Tag tracing dédié `jamodio::perfstats` → finit automatiquement dans
  `agent.log` et donc dans les bug-reports via `GetLogsArchive`.
- **Instrumentation encoder_thread** : wall-clock par tour de la boucle
  (capture→RTP send) + wall-clock par appel `host.process_stereo`
  (mesuré par sous-bloc PLUGIN_BLOCK). Coût `Instant::now()` ≈ 30 ns
  sur Apple Silicon, négligeable comparé au budget RT 2.7 ms/bloc.
- **Compteur `capture_drops` partagé** entre CPAL capture callback et
  `ws_server` (Arc<AtomicU64>, lu+swap 1 Hz). Inclus dans
  `PipelineLatency.dropsPerSec` — indicateur direct de saturation.
- **Drift ppm publié par peer** : `recv_decode_task` push la dernière
  valeur dans un `HashMap<producer_id, f64>` partagé quand elle bouge
  de plus de 1 ppm. Cleanup au shutdown du task. Lecture côté ws_server
  au flush 1 Hz.
- **`mixer::stream_perf_stats()`** : nouvelle méthode publique exposant
  (producer_id, underruns, drift_drops, target_ms) par stream remote
  (SELF_MONITOR exclu). Compteurs monotones — le browser fait la
  différence entre 2 snapshots s'il veut une cadence par seconde.

### Added — Browser side

- **Logger IndexedDB double-buffer** (`app/js/lib/logger.js`) : ring
  mémoire 5000 entries (= session courante) **+** persistence IndexedDB
  rotation 7 jours. Write batched async (flush 5 s + sur visibility=hidden +
  beforeunload), zéro blocage du thread principal. Prune horaire des
  entries > 7 jours. Fallback transparent au ring seul si IndexedDB
  indisponible (mode privé Safari ancien, quota dépassé).
- **API logger étendue** : `log.historySnapshot({ maxDays })` async lit
  IndexedDB ; `log.dumpWithHistory()` merge ring + history + dédup +
  tri chronologique. `log.snapshot()` et `log.dump()` inchangés
  (compat callers existants).
- **`bug-report.js`** utilise désormais `dumpWithHistory()` → les bundles
  envoyés au support contiennent les 7 derniers jours de browser logs,
  pas seulement le tab courant. Résout le pattern observé 22/05 (bundle
  généré le lendemain ne contenait que 2 lignes browser).
- **Handler `case 'perf-stats'`** dans `groupe.js` handleAgentMessage :
  log debug du snapshot reçu. Pas d'UI utilisateur en S1 (UI dashboard
  arrivera en S5 avec le toast plugin overload).

### Added — Tooling

- **Nouveau script `scripts/agent-latency-baseline.js`** (Node stdlib
  pur, aucune dépendance npm). Modes :
  - `--save baseline.json bug-report.txt` : extrait les snapshots
    perfstats du bug-report, agrège mean/p99/max session, sauve la
    référence.
  - `--compare baseline.json bug-report.txt` : compare et exit 1 si
    régression (`p99 > baseline + 0.5 ms` OU `p50 > baseline` OU
    `drops_total > 0`). Devient le **gate quantitatif** des PR S2-S6.

### Notes

- Build matrix CI inchangée (Mac ARM + Windows x64 via `agent-v*` tag).
- Zero impact mesuré sur la latence côté code : instrumentation ajoute
  ~60 ns par tour de l'encoder (`Instant::now()` × 2). Validation
  chiffrée à venir lors de la 1re session BETA après S2.
- 7 tests unitaires `perfstats` passent ; `cargo test --workspace` vert
  (16 tests audio-core, dont les 7 nouveaux). Build release Mac OK.
- Préreq pour PR-2 (S3+S4) : exécuter `agent-latency-baseline.js --save`
  sur une session test 10 min Mac+Win, committer le résultat dans
  `internal-docs/baselines/agent-v0.4.1-baseline.json`.

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
