# Design — Single-owner ASIO duplex host (`AsioDuplexHost`)

Compte-rendu de l'étude concurrents (Jamulus / JUCE) + design du host, 2026-07-04.
Branche `feat/asio-duplex`. Windows/ASIO uniquement (`#[cfg(windows)]`) ; macOS/CoreAudio
et `jamodio-audio-core` restent **byte-identiques**.

---

## 0. TL;DR

Le wedge « entrée railée au cold-start » a **deux causes indépendantes**, toutes deux
liées à cpal, et toutes deux éliminées par un host single-owner :

1. **On force une taille de buffer que le driver n'accepte pas légalement.** `Fixed(64)`
   passe par `asio-sys::create_buffers`, qui ne valide QUE `size <= max` — **pas de clamp
   au `min`, pas de snap à la granularité.** Une taille illégale (64) est transmise telle
   quelle à `ASIOCreateBuffers`, qui l'accepte nominalement mais dont le moteur DMA ne
   délivre jamais de frames valides → buffer figé quasi-rail, re-servi à chaque callback.
   **Ni Jamulus ni JUCE ne forcent jamais une taille hors `[min,max]`** : ils retombent sur
   la taille préférée du driver. C'est LA raison pour laquelle ils « marchent partout ».

2. **cpal fait un churn create→start→create au cold-open.** `build_input_stream_raw` appelle
   `ASIOStart` immédiatement (stream.rs:231) → le DMA d'entrée tourne SEUL @64 → puis
   `build_output_stream_raw` voit `Running`, fait `ASIOStop`/`ASIODisposeBuffers`/
   `ASIOCreateBuffers(in+out)`/`ASIOStart`. Ce start-puis-teardown d'une entrée cold à 64
   est le suspect direct du figeage. Le host single-owner fait `create(in)` → `create(in+out)`
   → **un seul `start()`**, sans aucun `ASIOStart` intermédiaire.

Preuve croisée au banc (déjà acquise) : @256 l'entrée n'est plus railée (abs_max 0.48/0.0003),
@64 elle rail bit-exact 0.9404732584953308. Cohérent avec « le symptôme n'existait pas avant
0.5.4-17 à 128 ».

---

## 1. Ce que font Jamulus et JUCE (invariants communs)

| Point | Jamulus (`src/sound/asio/sound.cpp`) | JUCE (`juce_ASIO_windows.cpp`) |
|---|---|---|
| **Driver chargé** | `loadAsioDriver`+`ASIOInit` **1×** par device-select, gardé ouvert | `openDevice()` (load+init) séparé de `open()` (stream), driver gardé |
| **Start/Stop** | ne togglent QUE `ASIOStart`/`ASIOStop` | idem ; `close()` ne relâche PAS le driver |
| **Teardown** | `ASIODisposeBuffers`+`ASIOExit`+`removeCurrentDriver` **seulement** au changement de device / shutdown | idem ; `Release` seulement sur reset/shutdown |
| **createBuffers** | **1 seul** `ASIOCreateBuffers`, tableau plat : inputs `[0..nIn)` puis outputs `[nIn..]` | idem, exactement le même layout |
| **Taille buffer** | query `ASIOGetBufferSize`, **snap** à la taille légale la plus proche (échelle min/max/granularité, cas spéciaux `-1`=puissances de 2, `0`/`<-1`=EMU) ; plus petite demande = **128**, jamais 64 | accepte la taille demandée **seulement si ∈ [min,max]** (et multiple de granularité) sinon **preferredSize**, sinon 1024 ; retry `createBuffers` en preferred si échec |
| **Frame réseau ≠ frame ASIO** | **découplé** : buffer de conversion/resampling maison (`bSndCrdConversionBufferRequired`) → net frame 64 indépendant de la taille ASIO | callback bosse à la taille ASIO ; le resampling éventuel est hors backend |
| **callback** | 1 `bufferSwitch` : lit input → `ProcessCallback` → écrit output → `ASIOOutputReady` si supporté | 1 `callback`→`processBuffer` : `convertToFloat` → user cb → `convertFromFloat` → outputReady |
| **Format** | `switch` sur type ASIO, normalise en `int16` ; réel = Int16/24/32 LSB, Float32 LSB | `ASIOSampleFormat{bitDepth,byteStride,LE,isFloat}` ; **respecte `byteStride`** (Int24 = stride 3 !) |
| **Reset** | `asioMessages` **n'agit pas** dans le callback : émet un signal → thread hors-audio → stop/(reload)/recreate/start sous `MutexDriverReinit` | `resetRequest()` arme un **timer 500 ms** → close → `needToReset` → reload driver → restart, sous garde control-panel modal |
| **kAsioEngineVersion** | retourne **2** (host ASIO 2.0) | idem ; `SupportsTimeInfo/TimeCode` → 0 (opt-out du time-info) |
| **Barrière Stop** | `ASIOStop` puis `ASIOMutex.tryLock(5000)` : garantit qu'aucun callback n'est en vol avant de disposer | flag `isStarted=false` + `callbackLock` |

### Patterns défensifs JUCE en plus (potentiellement décisifs pour NOTRE cold-start)

- **Dummy-buffer priming.** Avant l'ouverture réelle : `createDummyBuffers(preferredSize)` →
  `start()` → `sleep(80ms)` → `stop()`. Commentaire JUCE : *« cubase does this… some devices
  fail if we don't »*. À la **taille préférée**, hold 80 ms démarré. **≠ notre wake-pass raté**
  (qui faisait open→close IMMÉDIAT @64 forcé → churn-wedge). À tester au banc à la taille préférée.
- **Start-timeout.** Boucle 300×10 ms attendant le 1er `bufferSwitch` (`calledback`) ; sinon
  `stop()` + erreur propre. Convertit un driver qui accepte `start()` mais ne clocke jamais
  (cf. bug `out-first` @64 : « ASIOStart Ok mais 0 callback ») en échec géré au lieu d'un hang.
- **SEH** autour de `CoCreateInstance`/`Release`/`init` (pas portable en Rust : isoler l'appel,
  traiter un crash comme « driver parti → reload »).
- **Quirks nommés** : `Digidesign`→toujours preferred ; `denon dj asio`→`init` ment, lire
  `getLastDriverError` ; blacklist `ASIO DirectX Full Duplex` / `ASIO Multimedia Driver`.
- **Aucune manip de priorité thread / MMCSS** : le driver possède et priorise le thread de
  callback. Ne rien booster ; garder le callback allocation-free (buffers pré-alloués à `open()`).

---

## 2. EXPÉRIENCE DÉCISIVE À FAIRE EN PREMIER (avant tout code du host)

Une seule donnée tranche tout et coûte ~30 s au banc :

> **Dumper `ASIOGetBufferSize(min, max, pref, granularity)` de la Focusrite à froid.**

- Si `min > 64` **ou** 64 n'est pas un multiple de granularité valide → **64 est illégal** sur
  cette interface. C'est le smoking gun : cpal/asio-sys passent un 64 illégal (validé `<=max`
  seulement), JUCE/Jamulus l'auraient refusé → preferred. Fix confirmé = snapper à la taille légale.
- Si `min <= 64 <= max` et 64 légal, et que ça rail quand même → ce n'est PAS la valeur de taille
  mais le **churn** cpal (cause #2) → le host single-owner reste justifié par la structure.
- Résout aussi l'ambiguïté du handoff : sur la Solo `BufferSize::Default` == 64 (pref = 64 ?) —
  le dump dit si `pref` est réellement 64 ou si le panneau était juste réglé là.

`asio-sys` expose déjà `Driver::buffersize_range() -> (min, max)`. Il manque `pref` et
`granularity` : soit on lit `asio_get_buffer_sizes()` (privé — à exposer via un petit patch ou
un appel FFI direct dans le lab), soit on ajoute un mode `bufinfo` au lab qui appelle
`ASIOGetBufferSize` directement. **Action lab #1** ci-dessous.

---

## 3. Design du `AsioDuplexHost`

### 3.1 Cycle de vie à deux couches (JUCE)

```
open_device(driver_name)         // 1×, gardé ouvert
  ├─ Asio::load_driver(name)     // asio-sys ; ré-appel même nom = même Driver (mono-client safe)
  ├─ set_sample_rate(48k) + can_sample_rate check
  ├─ channels() → (n_in, n_out)
  ├─ query ASIOGetBufferSize → BufferRange{min,max,pref,gran}
  ├─ input_data_type()/output_data_type() → format par direction
  └─ (option JUCE) prime: create dummy @pref → start → sleep(80) → stop

configure_stream(sel_in_ch, sel_out_ch, desired_size)
  ├─ size = snap(desired, range)          // §3.3 — JAMAIS de taille illégale
  ├─ prepare_input_stream(None, n_in, Some(size))          // create(in)
  ├─ prepare_output_stream(Some(input), n_out, Some(size)) // create(in+out) — 0 ASIOStart entre
  ├─ add_callback(duplex_cb)              // §3.4
  ├─ add_message_callback(reset_cb)       // §3.5
  └─ start()                              // UN SEUL ASIOStart
     └─ start-timeout : attendre 1er callback ≤ ~2-3 s sinon stop()+erreur

close()            // stop() + dispose_buffers() ; driver reste chargé
teardown()         // drop du dernier Arc<Driver> → ASIOExit + removeCurrentDriver
```

Le driver est chargé **une fois** et gardé (mappe le `WarmAudio` keep-warm existant).
`close()` ne fait que `ASIOStop`+`ASIODisposeBuffers`. `ASIOExit` seulement au changement de
device / shutdown / reset complet.

### 3.2 Un seul `ASIOCreateBuffers(in+out)`

- Via l'API **publique** asio-sys : `prepare_input_stream(None,…)` puis
  `prepare_output_stream(Some(input),…)` → la 2ᵉ concatène in+out en **une** slice → **un**
  `ASIOCreateBuffers`. Mais c'est **2 `ASIOCreateBuffers`** (in seul, puis in+out) car
  `create_buffers`/`create_streams` sont privés. **Crucial : 0 `ASIOStart` entre les deux**
  (≠ cpal). Le mode `baseline` du lab tourne déjà ainsi 90 s sans faille.
- Option « vrai single create » : patcher/forker asio-sys pour exposer un
  `prepare_duplex_stream(n_in, n_out, size)` faisant **1** `create_buffers(in+out)`.
  Plus propre (= Jamulus/JUCE littéral), coût = vendoring d'une dép. À décider après le banc
  (si les 2-creates suffisent à tuer le rail, on garde l'API publique).

### 3.3 Politique de taille (le cœur du fix) — fonction `snap`

Combinaison JUCE (rejet→preferred) + Jamulus (snap granularité) :

```
snap(desired, {min,max,pref,gran}):
  if desired < min || desired > max            -> return pref     // JUCE : hors range = preferred
  if gran == 0 || gran < -1                    -> return desired  // EMU : toute taille légale
  if gran == -1 (puissances de 2):
        snap desired au plus proche 2^k ∈ [min,max]
  else:
        snap desired au multiple de gran le plus proche dans [min,max]
  clamp final à [min,max]
```

- **Jamais** passer une taille non-snappée à `create_buffers`.
- `desired` par défaut : suivre Jamulus → **≥ 128** (jamais 64) ; ou `pref`.
- **Latence** : si le driver impose 128/256, latence 2.67/5.33 ms/dir. Pour garder un net frame
  64 tout en ouvrant ASIO en 128/256 : **buffer de conversion maison** (Jamulus) — l'agent
  encode/pipeline en 64, le host ASIO tourne à la taille légale, on (dé)groupe entre les deux.
  À décider : accepter 128 (simple, 2.67 ms) vs resample vers 64 (complexe, 1.33 ms).

### 3.4 Callback duplex unique

```
duplex_cb(info):
  idx = info.buffer_index            // 0/1 double-buffer
  // INPUT : pour chaque canal in sélectionné, buffers[idx] (i32/i24/f32) -> f32
  //   respecter byteStride (Int24 = 3 octets !), endianness, float-passthrough
  //   -> sample_tx.send(Vec<f32>)   (LE SEAM existant, inchangé)
  // OUTPUT : mixer.lock().mix_into(&mut scratch_f32)   (LE SEAM existant)
  //   f32 -> type natif out, écrire dans buffers[idx]
  // if post_output { ASIOOutputReady }   // géré déjà par asio-sys ; dead-end connu
```

- Formats réels à couvrir : `Int32LSB`, `Int24LSB`, `Int16LSB`, `Float32LSB` (couvre ~tout le
  parc Windows, Focusrite inclus). Query 1× à l'open.
- Allocation-free : scratch pré-alloués à `configure_stream`.

### 3.5 Reset hors thread audio

- `add_message_callback` : sur `kAsioResetRequest`/`kAsioResyncRequest`/`kAsioBufferSizeChange`
  → **juste** incrémenter un `AtomicU64` + `Notify` (le `ResetSignal` existant). Retourner 1.
- Le superviseur (thread hors-audio) fait : `close()` → (option reload complet ASIOExit→Init si
  reset dur) → `configure_stream()` → `start()`, sous un reinit-mutex, gardé contre la ré-entrée.
- Réutilise `asio_reset.rs` + `audio_liveness_supervisor` + `power_events.rs` déjà en place.

### 3.6 Intégration agent (remplacement chirurgical)

- Le host remplace **les deux `cpal::Stream`** dans `open_duplex_on_com`, **uniquement** sur
  `host_is_asio` (Windows). Le reste de `PipelineState` est inchangé.
- Réutilise le seam : `sample_tx: Sender<Vec<f32>>` (input) et `mixer.lock().mix_into` (output).
- Garde **Level-1** (cache d'énumération `ASIO_STREAM_ACTIVE`, commit 9ea22d8) : indispensable,
  empêche toujours le reload concurrent du driver mono-client.
- Garde keep-warm/rejoin, backoff buffer (mais backoff = snap vers taille légale supérieure).
- macOS : `host_is_asio=false` → chemin cpal/CoreAudio actuel, **intact**.

---

## 4. Plan de validation au banc (`jamodio-asio-lab`) — AVANT tout code agent

Règle mission : pas de code agent sans repro+fix au banc. Le lab a déjà COM init, load, duplex
create, callback, message callback, start/stop/destroy, et le mode `coldstart` + `ColdStartProbe`.

1. **Action #1 — mode `bufinfo`** : dumper `ASIOGetBufferSize(min,max,pref,gran)` + `channels()`
   + `input_data_type()` de la Focusrite. Coût ~30 s. Tranche §2.
2. **Action #2 — `coldstart` avec snap** : ajouter `snap()` au lab et un `JAMLAB_SNAP=1`.
   Ouvrir à froid (Scarlett en veille → studio) à la taille snappée/préférée. La sonde dit-elle
   « entrée vivante niveau sain » (vs railée) ? Si oui → cause #1 confirmée, fix = snap.
3. **Action #3 — churn @ preferred** : reproduire la structure cpal (create+start+create) mais
   à la taille préférée, et la structure single-owner (2-create, 1 start) — comparer. Isole
   cause #1 (taille) vs cause #2 (churn).
4. **Action #4 (option) — priming JUCE** : dummy @pref → start → 80 ms → stop → open réel.
   Le rail survit-il ? (≠ notre wake-pass @64 qui churn-wedgeait.)
5. **Action #5 — prototype `AsioDuplexHost` complet dans le lab** : lifecycle 2 couches + snap
   + callback duplex réel (pas fill silence) + reset. Passer `baseline`/`coldstart`/`churn`.
6. **Seulement ensuite** : porter dans l'agent derrière `host_is_asio`, clippy strict + tests,
   valider en session réelle, compte-rendu, merge.

### Rappel : le lab ne reproduit PAS le wedge (l'agent gagne la course au boot ASIO)

Le rail cold-start ne se voit QUE dans l'agent (1er client ASIO au boot). Donc :
- Actions #1 (bufinfo) et #3/#4 (structure) : **validables au lab** (données objectives, pas
  besoin du rail).
- Action #2 (le rail disparaît-il vraiment à froid ?) : **nécessite l'agent** avec le host, en
  cold-start réel (Scarlett en veille). Chaque test coûte un cycle de veille lent.

---

## 5. Risques / questions ouvertes

- **Taille préférée réelle de la Solo** : le dump #1 tranche. Si `pref`/`min` = 64 ET ça rail,
  c'est le churn (cause #2), pas la valeur — le host reste justifié.
- **2-creates asio-sys churn-t-il à froid ?** Le `baseline` du lab est chaud (agent a déjà
  warmé). Action #3 doit le tester dans l'agent à froid, ou via un fork single-create.
- **Latence vs sécurité** : ouvrir en preferred (128/256) augmente la latence sauf si on ajoute
  le buffer de conversion Jamulus (net frame 64 découplé). Décision produit de Ben.
- **`out-first` disqualifié** (driver mort au réveil) : le host crée input-first (déjà le bon
  ordre). Le start-order (le host fait UN seul start, donc non-sujet) et le create-order sont distincts.
