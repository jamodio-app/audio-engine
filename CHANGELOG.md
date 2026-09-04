# Changelog

Toutes les versions notables de **Jamodio Audio Engine**.
Format : [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/) ·
Versioning : [Semantic Versioning](https://semver.org/lang/fr/).


## [0.5.13-3] — 2026-09-04 (pré-release)

### Ajouté — Micro talkback SÉPARÉ de l'interface instrument

Le talkback pouvait seulement être un CANAL du flux instrument : impossible de parler avec une
interface à une seule entrée (basse branchée = aucun canal libre), et choisir un autre micro
faisait sortir la voix de l'agent — donc sans Filtre antibruit et avec une latence non maîtrisée.

L'agent sait désormais ouvrir un **flux d'entrée dédié** sur le micro de son choix (micro-casque,
micro interne, seconde carte). Dans les Paramètres, la liste des micros talkback devient **unique** :
on choisit un micro, l'agent décide du mécanisme (tap sur le flux instrument si c'est la même
interface, flux dédié sinon). L'interface instrument n'y figure qu'une fois.

- **Rééchantillonnage du canal voix** quand le micro tourne à 44,1 ou 16 kHz — cas courant des
  micros-casques. Exception ASSUMÉE et limitée au talkback : le chemin instrument garde R2
  (48 kHz natif, aucun resampler). Coût mesuré < 4 ms, annoncé dans les Paramètres.
- Ids de micro voix **préfixés par leur host** (`wasapi:2:Casque USB`) : sans ça, l'index d'une
  énumération ASIO et celui d'une énumération WASAPI désigneraient deux matériels différents sous
  le même id. Les ids instrument ne changent pas — aucun réglage instrument n'est perdu.
- Le flux voix est tenu par un thread propriétaire : lâcher la poignée arrête le flux et **relâche
  le périphérique**.
- Rétro-compatible : `start-voice-capture` sans `voiceDeviceId` = comportement historique, prouvé
  par un test sur le message legacy.

⚠️ **Windows** : le canal voix passera par WASAPI pendant que l'instrument reste en ASIO (un
pilote ASIO est exclusif). À valider sur machine Windows avant diffusion.

⚠️ **Bêta** : les testeurs qui avaient choisi un micro « navigateur » devront le re-sélectionner
une fois (l'ancien identifiant ne désigne plus rien).

## [0.5.13-2] — 2026-09-03 (pré-release)

**Qualité de l'isolation de voix talkback.** La 0.5.13-1 rendait le talkback muet, puis,
une fois le VAD réparé, une voix hachée (« ça coupe quand il y a la voix »). Trois causes,
toutes corrigées à la racine et mesurées hors-ligne sur des prises réelles (voix + guitare,
un seul micro) avec `cargo run --release --example iso_offline`.

### Fixed — VAD Silero muet (talkback silencieux)

Silero v5 exige un **contexte de 64 échantillons** préfixé à chaque trame de 512 (entrée
réelle = 576). Sans lui, la probabilité de parole restait ~0 sur TOUTE parole → gate
toujours fermé → talkback muet. Test de non-régression sur parole réelle ajouté.

### Fixed — Denoise : on n'embarquait pas les réglages validés

DeepFilterNet commute des **étages entiers** selon le SNR local estimé, trame par trame :
sous `min_snr_db` il applique un masque de zéros (trame muette), au-dessus de
`max_erb_snr_db` il ne traite pas du tout. On utilisait les `RuntimeParams::default()` de
la bibliothèque (**−10 / 30 / 20**) au lieu de ceux du binaire officiel `deep-filter`
(**−15 / 35 / 35**), seuls validés à l'oreille : sur une captation où l'instrument repisse,
le modèle basculait sans arrêt d'un régime à l'autre (voix qui « respire »). Mesure sur
prise réelle : **67 % du niveau conservé contre 92 %**. Les seuils sont désormais explicites
(`DenoiseParams`), documentés, et verrouillés par un test.

### Fixed — Gate : le début des mots était rogné (lookahead)

Le VAD ne décide qu'à la **fin** de sa trame de 32 ms, et il lui faut parfois deux trames
sur une attaque douce. Sans retard, cette décision s'appliquait à des échantillons déjà
partis. Mesure : **48 attaques de mots sur 150 perdaient plus de 30 ms** (jusqu'à 205 ms).
La voix nettoyée passe maintenant par une **ligne à retard de 96 ms avant le gate** → 1
attaque sur 150 encore concernée. S'y ajoute une **hystérésis** (ouverture 0,50 / maintien
0,35) et une ballistique revue (attaque 5 ms, relâche 150 ms, maintien 400 ms).

⚠️ **Coût explicite : le talkback porte ~96 ms de latence de plus** (canal comm uniquement —
le monitoring instrument ne traverse JAMAIS cette chaîne). Le seuil d'ouverture reste à 0,50 :
plus bas, la repisse d'instrument suffit à ouvrir le gate et la règle « je joue, rien ne
sort » tombe (mesuré). La latence ajoutée est désormais **tracée au démarrage**.

### Fixed — L'entrée talkback ne sature plus l'encodeur (limiteur de crête)

Les logs terrain montraient `Possible clipping detected (2.619)` — **409 fois sur une journée,
dont 193 au-dessus de 2,0**. Vérification faite dans la source de DeepFilterNet : cette valeur
est le pic du signal **d'ENTRÉE**, pas de sortie. Autrement dit la capture livrait des crêtes à
**+8,4 dB au-dessus du plein échelle**, avec un simple micro-casque. C'est normal et attendu :
CoreAudio (comme WASAPI/ASIO en flottant) livre des `f32` qui ne sont pas bornés à ±1.0, et un
micro-casque ou interne — sans aucun réglage de gain matériel — peut être amplifié par le pilote
ou l'OS. Ces échantillons partaient tels quels dans Opus, qui les tronquait.

Mesuré au passage sur prise réelle : le denoise, lui, n'ajoute que **0,1 à 0,3 dB** — il n'est
pour rien dans le dépassement.

Un **limiteur de crête à lookahead** (3 ms, plafond −1 dBFS) est désormais placé juste avant
l'encodeur, **y compris en repli voix brute** puisque c'est l'entrée qui déborde. Sous le
plafond, le gain vaut exactement 1.0 : le signal n'est pas touché (ce n'est pas un compresseur).
La réduction appliquée est tracée — l'UI l'affichera.

### Fixed — Le début du talkback n'est plus perdu à l'activation

Le tap voix était greffé **avant** que les modèles d'isolation soient chargés (~260 ms) : la
capture poussait déjà des blocs, la file débordait, et la **première demi-seconde de talkback
partait en silence** (constaté dans les logs terrain grâce à la trace ci-dessous). Le thread
voix signale désormais qu'il est **prêt à consommer** — modèles chargés et rodés — et le tap
n'est greffé qu'à ce moment. Si le thread meurt avant d'être prêt, l'activation échoue avec une
erreur explicite au lieu d'un talkback muet.

### Changed
- Le tap voix ne jette plus de blocs **silencieusement** quand le thread voix est en retard :
  saturation tracée (échantillonnée). Ce thread fait tourner deux réseaux depuis la 0.5.13-1.
- `VoiceIsolator::new` fait un **tour de rodage** : `tract` alloue ses tampons à la première
  inférence, ce coût ne doit pas tomber sur le premier bloc de voix réel.

## [0.5.13-1] — 2026-09-03 (pré-release)

**Isolation de voix talkback (BÊTA, opt-in)** : sur le canal talkback, la voix est
isolée et la repisse d'instrument enlevée ; hors parole, le talkback est **coupé**
(silence total). 100 % **on-device**, pur Rust, **gratuit** (DeepFilterNet + Silero
VAD, exécutés via `tract`). N'affecte **jamais** le monitoring instrument temps réel.

- Cette pré-release est là pour **tester la fonction** en conditions réelles (idéalement
  2 machines : l'une joue/parle, l'autre écoute le talkback). L'isolation est **active en
  permanence** (le bouton on/off et le voyant arrivent ensuite) et ajoute ~30 ms de latence
  **au talkback uniquement** (canal comm).
- **Repli sûr** : si les modèles ne chargent pas, le talkback continue en **voix brute**
  (jamais coupé).

### Ajouté
- Sous-système `voice_isolation` (`jamodio-audio-core`) : denoise (DeepFilterNet) + VAD
  (Silero) + gate + resampler, pur Rust via `tract`, embarqué (aucune dépendance native).
- Champs `stream-levels` : `voiceOnAir` (voyant « à l'antenne ») et `isolationActive`
  (isolation active / repli), pour l'UI web à venir.
- `THIRD-PARTY-LICENSES.md` : crédits DeepFilterNet (MIT/Apache-2.0) + Silero VAD (MIT).

## [0.5.12] — 2026-09-02

### Fixed — Jitter buffer de réception : RÉCUPÉRATION (le buffer redescend enfin)

Sur lien jittery (WiFi), le jitter buffer de réception d'un pair **gonflait et
restait coincé haut** : mesuré à **17–40 ms** alors que la gigue réseau réelle
(`jitter_ms` RFC ~1,3 ms, queue tail 3–6 ms) n'en justifiait que **6–9 ms** — soit
**9 à 34 ms de latence évitable** sur ce que tu entends de chaque pair (étude
`internal-docs/studies/ETUDE-LATENCE-MONITOR-EMISSION-2026-08.md`, plan
`PLAN-JITTER-RECOVERY-C1`).

**Cause racine** : le filet réactif (`adapt_up`, +5 ms/underrun) protège bien, mais
la récupération (`adapt_down`) était (a) trop lente (−2,5 ms/5 s) et (b) **ré-armée
par chaque underrun** (`last_adapt = now`) → sur une cadence d'underruns WiFi, elle
ne se déclenchait quasiment jamais et le buffer restait collé, jusqu'au plafond 40 ms.

**Correctif (streams réseau uniquement, ADDITIF + BACKSTOP)** : la croissance du
filet est **inchangée** (protection identique). Seule la DESCENTE est revue : une
**pression d'underrun en fuite** (leaky bucket, sans horloge murale → déterministe)
pilote la récupération. Tant que la pression dépasse le seuil, le filet est tenu ;
sous le seuil (calme), il draine vers 0 à vitesse bornée → **retour au plancher
tail-aware (6–8 ms) en quelques secondes** au lieu de rester coincé. Un underrun
ISOLÉ ne bloque plus la récupération ; une cadence soutenue la maintient. Pire cas =
comportement d'avant (on ne peut pas régresser la protection). **Le self-monitor
local (`local_mode`) garde son adaptation bornée historique — intouché.** Couvert
par 5 tests unitaires (récupération au calme, tenue post-underrun, borne par pull,
plafond de pression, non-régression du chemin local).

**Calibration P1 (mesure réelle)** : la première pré-release récupérait
correctement (buffer redescendu de ~17 à ~10 ms de médiane) mais **trop vite sur
WiFi** — il retombait au plancher puis se faisait cueillir par un pic WiFi → hausse
des underruns. Récupération rendue plus CONSERVATRICE : fuite de pression ralentie
(mémoire d'underrun ~2 s au lieu de ~90 ms), drainage plus doux (~4 ms/s), et
**plafond de pression** (récupération au plus tard ~5,5 s après le dernier underrun).
Objectif : latence basse sur lien propre SANS regonfler les underruns sur WiFi.

### Added — Sécurité : coupe-circuit d'emballement de sortie (protection écoute)

Un plugin instrument peut s'**auto-osciller** (ampli-sim à fort gain nourri par un
glitch/denormal) et produire un niveau de sortie anormal — vécu en session : « gros
son » soudain dans le casque avec AmpliTube, persistant, obligeant à tuer l'agent.
Le soft-clip bornait déjà la sortie à ~0 dBFS (pas de sur-niveau dangereux) mais le
résultat restait un bruit plein-échelle pénible, et **rien ne coupait la source**.

Ajout d'un **coupe-circuit** : si le peak de sortie pré-clip dépasse **+12 dBFS (×4)**
sur **plusieurs fenêtres consécutives** (anti faux-positif sur un transitoire fort
légitime), l'agent **coupe (bypass) le plugin automatiquement** — en réutilisant le
mécanisme d'auto-bypass existant — et émet un event `instrument-plugin-runaway` →
toast UI « sortie anormale — plugin coupé par sécurité » avec bouton « Réactiver ».
Détection dans la boucle perfstats (**hors du chemin audio temps-réel** → zéro risque
sur le hot-path), pilotée par une fonction PURE testable (6 tests unitaires). Distinct
de l'overload CPU (`instrument-plugin-overload`) : ici la cause est le **niveau**, pas
la charge CPU.

### Changed — Écoute locale (A-lite) : plancher self-monitor 5 → 3 ms

Le retour casque (self-monitor) passait par un pré-fill de **5 ms** — le plancher
partagé avec le réseau (`MIN_TARGET_MS`). Mais le signal local n'a **pas de gigue
réseau** (seulement la petite gigue d'ordonnancement des threads). Ajout d'un plancher
LOCAL dédié `LOCAL_MIN_TARGET_MS = 3 ms` : le self-monitor descend à **3 ms au calme**
(**~2 ms gagnés** sur ce que tu entends), tandis que le **plancher réseau reste 5 ms
(intact)**. L'adaptation bornée (jusqu'à 15 ms sur spike plugin, retour au plancher,
concealment inaudible) est conservée. Ne touche NI le plugin, NI son thread, NI le
callback RT, NI le réseau. CONSTANTE DE CALIBRATION (remonter à 4 ms si trop de
concealments au chant). Aussi : l'affichage « Ton monitoring » côté studio est corrigé
pour être conforme (vrai monitoring local = buffers in+monitor+out, sans Opus ni jitter
réseau) — paire web.

### Added — Plancher jitter buffer piloté par le TAUX DE GLITCH (P0)

Dernier lever pour ZÉRO glitch. Sur lien PROPRE (Ethernet), le plancher tail-aware
tombe à MIN (5 ms) et underrunne ~2/min sur des micro-à-coups LOCAUX (recv_path 2 ms,
ordonnancement) que le plancher basé sur la gigue RÉSEAU ne voit pas (mesuré sur lien Ethernet propre).

Ajout d'un `glitch_floor` PERSISTANT (streams réseau) piloté par les underruns réels :
grow-fast (+1 ms/underrun, borné 20 ms), shrink-slow (décroissance très lente, ~5 ms
récupérés en ~5 min de calme TOTAL) → converge vers le plancher MINIMAL qui tient zéro
glitch, propre à chaque lien. Additif + backstop : glitch-free ⇒ inerte (identique à
avant). Self-monitor local intouché. Complète C1/P1 (récupération « buffer trop haut ») ;
P0 = le symétrique « buffer trop bas ». Cf. `internal-docs/plans/PLAN-P0-GLITCH-FLOOR-2026-09.md`.
3 tests unitaires ; 106 tests audio-core + clippy verts.

### Changed — Mixer sans verrou global : verrouillage FIN par flux (C2.1)

Après le durcissement priorité, un stall résiduel `recv_path` (~3–4 ms) subsistait : le callback
de sortie tenait UN SEUL mutex sur TOUS les flux pendant tout `mix_into` → s'il était préempté en
le tenant, le thread de décodage (`push_samples`) attendait → underruns/craquements. Priorité
inversée : un thread prioritaire qui attend un lock tenu reste bloqué.

Refonte du verrouillage du mixer (SANS lock-free — verrous standards, robuste, review facile) :
suppression du `Mutex<AudioMixer>` externe (méthodes `&self`, `Arc<AudioMixer>` partagé). La map
des flux passe sous `RwLock` (écrite seulement à l'add/remove) ; chaque flux a son `JitterBuffer`
sous un **Mutex court dédié** ; volume/pan/mix_armed + VU (rms/peak) en **atomiques**. `mix_into`
clone les `Arc` des flux sous un RwLock LECTURE bref, le relâche, puis pull chaque flux sous le
verrou COURT de SA cellule — le callback ne tient plus JAMAIS un verrou couvrant tous les flux.
`push_samples`/`push_self_samples` idem. **Fenêtre de contention décode↔callback : ms → µs.**

Math du mix **inchangée** (VU par tranche, MIX REC armés, DIM, talkback/voix, métronome/backing,
master, self-monitor, record, hot-swap — identiques). 101 tests mixer existants passent sans
modif d'assertion + 2 tests de concurrence ajoutés (push + churn add/remove pendant 20 000
`mix_into`). Cf. `internal-docs/plans/PLAN-C2-SPSC-MIXER-2026-09.md`.

### Changed — Durcissement de la PRIORITÉ des threads audio (Mac + Windows)

Le thread de **décodage de réception** était moins prioritaire que capture/process/encode
et se faisait **préempter sous forte charge machine** (rendu vidéo navigateur, WindowServer,
apps de fond) → pics `recv_path` 5–10 ms → underruns → craquements sur l'audio reçu des
peers. Racine mesurée (sessions debug 09/2026), **indépendante du réseau** (gigue réelle
~0,8 ms). Standard PRO : l'audio ne doit JAMAIS être dégradé par une charge tierce.

Durcissement des DEUX plateformes, à égale rigueur (cf.
`internal-docs/plans/PLAN-AUDIO-PRIORITY-HARDENING-2026-09.md`) :
- **macOS** : le décodage passe de « QoS `USER_INTERACTIVE` seul » à un
  `THREAD_TIME_CONSTRAINT_POLICY` **léger** (computation 0,3 ms, période 2,5 ms) — le fait
  passer dans la **bande temps-réel** du scheduler (au-dessus des threads UI/vidéo) sans
  sur-réserver de CPU. Fallback QoS-seul si le time-constraint échoue. On NE rejoint PAS le
  workgroup CoreAudio de sortie (sur-population + régression historique évitées).
- **Windows** : ajout de `AvSetMmThreadPriority(AVRT_PRIORITY_CRITICAL)` sur TOUS les threads
  audio RT (capture/process/encode **et décodage**) — les faisait monter du niveau NORMAL au
  **sommet** de la tâche MMCSS « Pro Audio ».

Effet visé : `recv_path` borné et underruns d'origine CPU → ~0, même sous charge lourde, sur
les deux plateformes. Aucune latence ajoutée (priorité, pas buffer).

## [0.5.11] — 2026-08-07

Release majeure consolidant tout le cycle 0.5.11 : robustesse audio (48 kHz natif
/ ASIO-only, crash 32 canaux, faux rate CoreAudio), détection de plugins (AU
licenciés, fabricants UVI), VU peak-mètre DAW, respect de l'autostart, MIX REC des
pistes armées, et **mises à jour obligatoires mais annoncées** (fin de l'install
silencieuse au boot). Détail des correctifs dans l'historique git de la 0.5.11.

### Ajouté
- **Audio 48 kHz natif / ASIO-only (R1–R5).** 48 kHz natif obligatoire (entrée ET
  sortie Mac) ; tout écart → refus explicite + sortie du studio, **jamais de repli
  silencieux**. Zéro resampler (il coûtait ~29 ms ≈ tout le budget latence). ASIO
  obligatoire sous Windows. Docs : `internal-docs/decisions/AUDIO-48K-ASIO-ONLY-2026-08.md`.
- **WebView2 machine-wide (`embedBootstrapper`).** Corrige « Could not find the
  WebView2 Runtime » sur les Win10 sans runtime préinstallé, sans embarquer
  l'installeur offline (~127 Mo).
- **VU peak-mètre DAW.** Pic échantillon (dBFS) par stream + master + mix, calculé
  dans les passes RMS existantes (**zéro impact latence**, hors thread audio RT).
  La barre devient un vrai peak-mètre (attack instantané, descente à taux fixe) et
  alimente une détection de CLIP fiable en mode agent.
- **Bouton « Rescanner » les plugins** + **diagnostic par nom** : les plugins
  écartés s'affichent par leur **nom réel** (icône + forme + texte, daltonien-safe ;
  plus d'emoji dans l'UI).
- **Autostart au choix.** Toggle « Démarrer avec l'ordinateur » dans la fenêtre
  agent (commandes `get_autostart` / `set_autostart`), **respecté** ; défaut ON au
  1er lancement (marqueur `app_config_dir`) puis l'état OS fait foi.
- **MIX REC = pistes armées uniquement (mode agent).** Le bus d'enregistrement
  (fichier mix + VU MIX REC) ne somme que les sources **armées** (`mix_buf` dédié
  dans `mix_into`) ; le monitoring/MASTER reste le mix complet. Zéro latence
  ajoutée. Protocole `set-record-arm`.
- **Mises à jour obligatoires mais annoncées.** Fin de l'installation silencieuse
  au démarrage (risque de conflit avec le chargement des drivers ASIO + fenêtre
  surprise). Quand l'agent est en retard sur la dernière stable, le web **bloque
  l'entrée en studio** par une modale « Mise à jour requise » (barre de progression
  + phases), **jamais pendant une session active**. Protocole `update-progress`.
- **Hook panic Rust → `agent.log`** — un panic partait sur stderr (invisible sur
  une app GUI Windows) ; il est désormais dans le bundle support.

### Corrigé
- **Crash ASIO 32 canaux** (corruption de tas / `STATUS_HEAP_CORRUPTION` sur
  Behringer WING 32 in/out) : l'agent n'ouvre plus que **`paire+2`** canaux de
  sortie (régime éprouvé 0.5.9) au lieu de tous les canaux.
- **Fuite de callbacks fantômes CoreAudio** (faux rate 96k/192k qui piégeaient le
  détecteur de dérive) : un `cpal::Stream` droppé sans `pause()` continuait
  d'émettre ~750 callbacks/s → compteur pollué. Fix : enveloppe `SendStream` dès la
  création → tout `Err` en aval passe par `pause()`.
- **AU tiers licenciés à nouveau détectés sur Mac** (BFD, AmpliTube, Kontakt,
  plugins iLok…) : le worker de scan pompe désormais sa run loop Cocoa comme le
  chargement live → plus de hang → plus de blocklist à tort (`SCANNER_ABI` 1→2 =
  rescan complet au 1er lancement).
- **Plugins d'un fabricant à code fourcc < 4 caractères** (UVI/Falcon, Sparkverb,
  Thorus…) à nouveau détectés : le worker ne tronque plus l'espace de complément du
  code (« UVI » ≠ « UVI »).
- **Sélecteur de plugins bloqué sur « Scan… »** : conséquence des rescans en boucle
  ci-dessus (jusqu'à 256 sessions) — corrigé (plus besoin de recharger la page) ;
  scan plus rapide et plus léger au démarrage.
- **Autostart Mac respecté** (`[NSApp disableRelaunchOnLogin]`) : macOS ne relance
  plus l'agent via la restauration de session au login → seul le LaunchAgent (= le
  toggle utilisateur) contrôle le démarrage.

## [0.5.10] — 2026-07-28

Chantier **sortie audio en mode agent** : en studio, tout l'audio IN **et OUT**
passe par l'agent, avec choix du périphérique et des canaux de sortie. Zéro ms
ajouté à la pipeline (garde-fou latence n°1 respecté). Durci par une revue de code
max-effort (web + agent) avant release.

### Ajouté
- **Sélection de la sortie audio (mode agent).** Picker de périphérique de sortie
  (CoreAudio) + sélection de la **paire de canaux de sortie** sur interface
  multicanal — **ASIO (Windows) ET CoreAudio (Mac)**. L'agent ouvre tous les canaux
  du device et n'écrit le mix stéréo que dans la paire choisie (`set-output-pair`,
  store atomique) : **swap LIVE, aucune réouverture driver**, inerte sur un device
  ≤ 2 sorties. Sortie stéréo standard = comportement **identique** à l'historique
  (fast-path zéro-copie).
- **Voix / talkback des pairs via l'agent (Lot C).** La voix des autres musiciens
  peut sortir sur le device d'écoute choisi (plus seulement la sortie système). Étage
  voix **dédié** : sommé après le tap RECORD et après le DIM → **jamais enregistré,
  jamais ducké** (parité exacte avec le navigateur). Latence instrument/self-monitor
  **intacte** (étage distinct) ; voix ≈ 7–13 ms, imperceptible pour la parole.
- **Aperçus de fichiers Library via l'agent, en studio (Lot D).** Slot `preview`
  dédié (2e instance de backing, buffer séparé) sortant sur le bon device ; jamais
  enregistré ni ducké, n'évince pas le backing chargé.
- **Énumération de sortie tolérante (macOS).** Récupère les sorties réellement
  ouvrables que CPAL masquait (port jack intégré inactif, HP sous un enregistreur
  virtuel…). Windows inchangé (probe interdit sur driver ASIO mono-client).
- **« Défaut système » suit l'OS en live.** Changer la sortie par défaut de l'OS en
  session réouvre le playback dessus (instrument aligné sur backing/métronome). Hors
  thread audio → zéro latence. Une sortie explicitement choisie n'est jamais écrasée.

### Corrigé
- **Une préférence de sortie périmée ne bloque plus l'entrée en session.** La
  résolution de sortie est **non-fatale** : repli **visible** sur la sortie par
  défaut système + signal `outputFallback` (le sélecteur revient sur « Défaut
  système » + toast), jamais silencieux.
- **Talkback des pairs coupé après ~8 s de silence.** L'idle-timeout est désactivé
  pour les flux voix (légitimement silencieux au mute) ; les instruments gardent le
  timeout.
- **Claquement (clic) à l'arrêt du backing track.** Fondu de declic ~5 ms au
  play/pause/stop (profite aussi aux aperçus Library).

### Robustesse (revue max-effort avant release)
- **Sortie stéréo (self-monitor) : fast-path zéro-copie restauré** — plus d'étage
  CPU ajouté sur le chemin audio le plus sensible.
- **Superviseur de liveness** : ne reconstruit plus une capture SAINE quand la sortie
  est absente par design (plus de churn de la capture).
- **`set-output-pair`** appliqué de façon fiable (plus de perte silencieuse sous
  contention de lock).
- Défauts sûrs : le tap RECORD n'enregistre jamais un flux inconnu ; l'index de
  paire est forcé pair ; l'enveloppe de declic est réinitialisée au (re)chargement.
- Nettoyage ciblé (module partagé `audio::output_pair`, commentaires, code mort) ;
  clippy 100 % propre sur **macOS ET Windows**.

## [0.5.9] — 2026-07-23

Release **publique**. Refonte du **scan des plugins** (désormais hors-process,
standard DAW) et correctif capture macOS. Consolide les pré-releases
`0.5.9-1` → `0.5.9-4`.

### Changé
- **Scan des plugins hors-process.** L'agent instanciait chaque plugin tiers
  dans son propre process au démarrage : un seul plugin qui plante à
  l'instanciation (crash natif, ex. Groove Agent SE) faisait tomber l'agent
  entier, sans possibilité de le rattraper. Le scan tourne désormais dans un
  **worker jetable** : un plugin qui le fait crasher ou figer ne tue que le
  worker, le plugin fautif est **exclu** (blocklisté) et le scan reprend au
  suivant. L'agent ne tombe plus jamais au scan.

### Ajouté
- **Cache de scan persisté** : le scan complet n'a lieu qu'une fois (seuls les
  plugins nouveaux ou mis à jour sont re-scannés) → démarrages suivants quasi
  instantanés (quelques ms au lieu de 15–25 s).
- **Blocklist auto-réversible** : un plugin exclu retente automatiquement sa
  chance dès qu'il est mis à jour ; les plugins exclus sont signalés au
  navigateur (fenêtre FX) pour expliquer leur absence.
- Isolation dure du worker : Job Object `KILL_ON_JOB_CLOSE` sur Windows — il ne
  peut jamais survivre à l'agent.

### Corrigé
- **Perte de la tranche des autres musiciens au changement d'entrée (macOS),
  puis « recherche de l'Audio Engine » en boucle.** À chaque changement de
  canal/device en session, l'ancien flux de capture CoreAudio continuait de
  tourner après sa destruction (quirk cpal : dropper le stream d'entrée n'arrête
  pas son AudioUnit) → callbacks fantômes qui saturaient le lock pipeline →
  agent muet, watchdog « agent lost », streams pairs tués et boucle de
  reconnexion. Correctif : `pause()` explicite avant chaque destruction de
  stream cpal (`Drop for SendStream`) — couvre hot-swap, stop, reset driver et
  chemins d'erreur. Couvert par 3 tests device réel.

### Interne
- Suppression du scan de plugins in-process, devenu inutile (code mort) ;
  couverture de tests repointée sur les primitives hors-process. Aucun
  changement de comportement.

## [0.5.8] — 2026-07-21

Release **publique** consolidant les pré-releases `0.5.8-1` → `0.5.8-5` :
éditeur VST3 net en haute densité (Windows), réception des pairs préservée au
changement d'entrée, **keepalive Ping/Pong WS** (fin des déconnexions
intermittentes qui perdaient mute/ENTRÉE/volumes en onglet arrière-plan),
**métronome enrichi** — plus le correctif ENTRÉE.

### Ajouté
- **Métronome enrichi (source « référence » Option B).** La synthèse du clic
  supporte le **chiffrage** (`pulseRatio` pour la croche en ×/8 + `beatsPerBar`
  + `accentPattern` fort/médium/normal par temps), les **subdivisions**
  (`8`/`8t`/`16` en plus de `q`) et une **banque de sons** synthétisés
  déterministes (`click`/`blip`/`digital`/`cowbell`/`woodblock`), miroir exact
  du navigateur. Le navigateur reste maître de la grille (timing/sync inchangés).

### Corrigé
- **Fenêtre d'éditeur VST3 rognée sur écran à haute densité (DPI > 100 %)**
  (Neural DSP Archetype, Polyverse Wider…). L'hôte appelle désormais
  `IPlugViewContentScaleSupport::setContentScaleFactor` avant `getSize` → la vue
  renvoie sa taille physique correcte et la fenêtre l'épouse.
- **Changer son entrée en cours de session coupait la réception de TOUS les
  pairs** (VU figé, aucun son, jusqu'à un rejoin complet — bug asymétrique
  Mac↔PC). Le navigateur signale via `sessionContinues` qu'un `start-capture`
  continue une session active (hot-swap) : l'agent ne reconstruit alors que le
  chemin capture/self et **préserve la réception des pairs**.
- **Déconnexions WS agent↔browser en boucle (~16 s) → commandes perdues**
  (mute/ENTRÉE/volumes par intermittence). Cause : le watchdog s'appuyait sur un
  heartbeat JS **throttlé par Chrome en onglet arrière-plan**. Correctif :
  **keepalive Ping/Pong WebSocket** (au niveau réseau, hors JS throttlé) — un
  socket réellement mort ne renvoie plus de Pong → coupure légitime préservée.
- **Changement de paramètre métronome pas appliqué en temps réel** (obligeait à
  PAUSE/PLAY) : passer à une grille plus lente supprimait tous les onsets
  jusqu'au rattrapage → silence. `set_config` réinitialise désormais le garde-fou
  anti-double-clic.
- **Instrument muet au join après avoir quitté une session en ENTRÉE OFF.** Le
  drapeau `input_cut` (toggle ENTRÉE), porté par le pipeline unique et à vie de
  l'agent, survivait d'une session à l'autre → instrument coupé à la source alors
  que l'UI se réaffiche ENTRÉE ON. Une nouvelle session
  (`session_continues=false`) repart désormais ENTRÉE ON ; un OFF volontaire
  reste préservé au hot-swap (`session_continues=true`).

## [0.5.7] — 2026-07-18

Première release **publique** du **Lot 2** : talkback sur un canal indépendant
via l'agent, VU-mètres fidèles au mix (stéréo réel, pan, talkback pair) et
robustesse ASIO. Consolide les pré-releases `0.5.7-1` → `0.5.7-8`.

### Ajouté
- **Talkback sur un canal indépendant via l'agent (Lot 2)** : protocole
  `StartVoiceCapture`/`StopVoiceCapture`/`SetVoiceGain`, **4ᵉ thread
  `voice_encode`** + **tap voix lock-free** extrayant un canal mono arbitraire du
  buffer d'entrée (nécessaire sur Windows/ASIO exclusif où Chrome ne voit que les
  canaux 1-2). Fondu de gain par-sample anti-clic. **Coût nul quand inactif**,
  instrument byte-identique.
- **VU MASTER / MIX REC en vrai stéréo** : RMS L/R du mix **réel** (fini le proxy
  mono L=R) → le pan et les faders deviennent visibles sur ces VU.
- **VU talkback en mode agent** : le thread `voice_encode` mesure le RMS
  **post-gain** du talkback, diffusé comme niveau `voice` dans `StreamLevels`.

### Corrigé
- **VU ne reflétait pas le pan** : les RMS L/R sont désormais calculés **post-pan**
  (loi de balance partagée avec `mix_into`) — couvre self + peers en un point.
- **VU talkback figé quand on parle seul** (sans jouer) : les niveaux sont poussés
  dès que `voice_rms > 0`, même sans peer ni instrument actif.
- **Plugins INSERT — budget de latence intrinsèque relevé 64 → 128 samples.** Les
  amp-sims Neural DSP (Darkglass ≈ 84 samples) étaient rejetés à tort ; 128
  (2,67 ms @ 48 kHz) est purement additif et sûr. Règle de compatibilité
  centralisée et partagée par les hôtes AU et VST3.
- **Réouverture ASIO à froid muette après une pause prolongée hors studio**
  (Focusrite USB). Après relâche du driver par la grâce, la réouverture revenait
  parfois « callbacks vivants mais muette ». Correctif : recyclage de l'apartment
  COM-STA (`CoUninitialize`/`CoInitialize` frais) avant ce cold-reopen — ciblé,
  sûr, sans effet sur macOS/WASAPI.

### Nettoyage
- Retrait du diagnostic temporaire `talkback-vu-diag` : la cause du VU talkback
  plat au switch device navigateur→agent était **côté navigateur** (snapshot
  instrument incomplet), corrigée dans la web app.

## [0.5.6] — 2026-07-15

> **Première version BETA publique.** Consolide les itérations de pré-release
> 0.5.6-1 → 0.5.6-6 : robustesse sous charge (3+ peers sur ASIO/Focusrite),
> durcissement sécurité pré-BETA, et corrections MIDI / éditeur plugin / logs.

### Sécurité
- **WS de contrôle local durci** : en build release, refus des connexions sans
  en-tête `Origin` (un navigateur en envoie toujours un — seul un client natif
  l'omettait pour se faire admettre) et whitelist des previews Vercel épinglée au
  scope de l'équipe. Ferme le drive-by web distant (exfiltration micro).
- **StartCapture** : validation de l'IP de destination SFU (rejet des IP
  invalides / loopback en release).

### Robustesse
- **Setup critique sous charge** : `StartCapture`, `AddStream`, `RemoveStream`,
  `SelectDevices`, `SetInputSource` (bascule MIDI/audio), `Load/UnloadInstrumentPlugin`,
  `ListPlugins`, `Start/StopRecording`, `Stop` **attendent** désormais le lock
  pipeline au lieu de répondre `overloaded` au bout de 200 ms. Corrige, à 3+ peers
  sur ASIO/Focusrite : instrument muet au join d'un 3ᵉ peer, bascule MIDI/audio
  cassée obligeant un relaunch, liste de plugins bloquée en « Scan… ». Le hot-path
  idempotent (volume/pan/dim, stats, éditeur plugin) garde le skip 200 ms.
- **Erreurs agent corrélées** : `Error` porte une clé optionnelle (`producerId`
  pour `add-stream`) → le navigateur ne rejette que la requête concernée. Un
  handler lent n'empoisonne plus les requêtes de setup en vol.
- **Liste de plugins** : sur contention, `ListPlugins` renvoie `scanning:true` et
  l'UI repolle de façon bornée (bouton « Réessayer » en dernier recours) — elle
  ne reste plus bloquée indéfiniment.
- **Thread COM-STA (Windows/ASIO)** : isolation de panic (`catch_unwind`) — un
  panic dans une closure driver ne détruit plus l'audio pour la vie du process.
- **Clamps anti-DoS** : bornes défensives sur la pipeline et le mixer de référence.

### Corrigé
- **MIDI — source poussée au (re)connexion** : l'agent envoie sa source d'entrée
  courante (audio / MIDI) à chaque connexion browser. Sans ça, après un rejoin
  (bascule mode agent, 2ᵉ onglet Jamodio, reload de page), le clavier MIDI
  (physique ou virtuel) restait indisponible jusqu'à un re-toggle manuel.
- **Éditeur plugin VST3 (Windows)** : re-cliquer sur un plugin déjà chargé ramène
  sa fenêtre au premier plan (restore si minimisée + bring-to-front fiable), même
  cachée derrière le navigateur — mise à parité avec macOS (AU).
- **Rétention des logs** : `rolling::daily` ne purgeait jamais les anciens
  `agent.log.*` (~150 Mo / 60 fichiers constatés sur macOS) ; l'agent conserve
  désormais les 14 fichiers les plus récents (purge best-effort au démarrage, ne
  touche jamais le fichier du jour).

### Modifié
- **Retry-on-`overloaded` borné** (navigateur, 15 × 200 ms) étendu à
  `start-capture` (join initial + swap device en session), en plus d'`add-stream`.
- **Badge de version de l'agent plus lisible** (9 → 12 px, contraste renforcé) —
  facilite l'identification d'un build en cours.

### CI
- Actions GitHub épinglées à un SHA de commit (protège la clé privée de l'updater
  contre un tag/branche repointé).

## [0.5.5] — 2026-07-09

> **Option B — la référence (métronome + backing) est jouée par l'AGENT**, sur son
> chemin de sortie à latence CONNUE (ASIO/CoreAudio), au lieu du navigateur (qui
> sous-rapporte sa latence de sortie sur Windows/WASAPI). Résultat : synchro
> inter-peers juste sur toutes les machines, à 2 comme à 3 (late-join inclus).
> Validé au micro PC Focusrite ↔ Mac. Synthèse des pré-releases 0.5.5-1/-2. Cf.
> `internal-docs/plans/PLAN-OPTION-B-B0-DESIGN.md`.

### Added
- **Source « référence » dans le mixer** (`jamodio-audio-core/mixer/reference.rs`) :
  - **Métronome** synthétisé à l'échantillon près, à partir d'une grille exprimée
    en frames de sortie de l'agent (browser maître de la grille). Synthèse
    extensible (son / figure rythmique) ; un preset câblé.
  - **Backing** : le navigateur pousse le PCM stéréo 48k une fois
    (`reference-backing-begin/chunk/end`), l'agent le rejoue aligné sur la grille,
    servo varispeed anti-dérive inter-peers (snap sur seek, ±1 % sinon).
  - Mixée à un point DÉDIÉ : exclue du MIX enregistré, non duckée par le DIM, suit
    le master. Volume/pan via `producerId 'reference'` (métro) / `'backing'`.
- **Ancrage horloge échantillon↔mural** exposé au navigateur (`reference-clock-pong`)
  + **horloge monotone process-wide** (`sync/clock.rs`) — le navigateur mappe la
  grille serveur → l'échantillon de sortie exact (ping/pong min-RTT).
- **Protocole** : `reference-clock-ping/pong`, `reference-config/grid/stop`,
  `reference-backing-*`.

### Notes
- Bénéfice garanti sur ASIO/CoreAudio (latence de sortie connue). Sur WASAPI sans
  interface, le navigateur retombe sur la compensation locale (Option A).
- Le navigateur gate Option B sur agent ≥ 0.5.5 (les agents antérieurs → Option A).

## [0.5.4] — 2026-07-09

> **Host ASIO duplex « maison » + robustesse Windows + télémétrie de latence honnête.**
> Synthèse des pré-releases 0.5.4-1 → 0.5.4-19. Premier débogage sur un vrai PC
> Windows + Focusrite Scarlett : cause racine du gel ASIO trouvée (ré-énumération
> concurrente) et corrigée ; host ASIO single-owner promu par défaut.

### Added
- **Host ASIO duplex single-owner (maison)** — entrée + sortie sur un seul driver
  ASIO, propriétaire unique du driver mono-client, cold-start armé sur toute
  interface. Remplace le double-flux cpal comme défaut (build-probe v1→v4 validés
  sur vrai Scarlett).
- **Capture paire stéréo arbitraire** (`stereoStart`) — deux canaux physiques
  consécutifs en entrée.
- **VU self stéréo** — RMS L/R indépendants pour la tranche « moi ».

### Changed
- **Télémétrie de latence honnête** — latence rapportée = **taille de buffer RÉELLE
  mesurée** (plus d'estimation).
- **Buffer 64 échantillons par défaut** + backoff automatique à 128 sous charge ;
  déférence à la taille de buffer préférée du driver.

### Fixed
- **Gel ASIO (cause racine) : cache d'énumération** — plus de rechargement du driver
  ASIO mono-client pendant qu'un stream est actif (le `GetDevices` du browser en
  session déclenchait un `ASIOInit` concurrent → gel silencieux des callbacks).
  macOS/WASAPI strictement inchangés.
- **Reset coopératif ASIO** (`kAsioResetRequest`) + recovery robuste.
- **Keep-warm ASIO** — driver gardé chaud à travers les leave/rejoin (fin du churn
  `ASIOExit/ASIOInit`) ; fix du double-leave qui annulait le keep-warm.
- **Robustesse temps-réel Windows (P0+P1)** — queue de gigue robuste + recovery des
  callbacks ASIO ; plus de faux « agent saturé/drops » pendant le park ASIO.
- **Jitter buffer plancher tail-aware** (Chantier #1 Phase 1).
- **Sécurité** — probe rendu inerte après un BSOD.

## [0.5.3] — 2026-06-28

> **Release temps-réel Windows : réception + émission RT (Windows jouable) +
> auto-recovery des callbacks ASIO. Synthèse des pré-releases 0.5.3-1 → 0.5.3-5.**

### Added
- **Décodage de réception en temps-réel** (un thread RT partagé, MMCSS Windows /
  QoS `USER_INTERACTIVE` macOS) : corrige le « 60 ms injouable » Windows — le
  décodage ne se faisait plus préempter, le jitter buffer du pair ne se collait
  plus au plafond 40 ms. Validé PC↔Mac en internet réel.
- **Émission en temps-réel** : chiffrement SRTP + `send_to` fusionnés dans le
  thread d'encode RT (suppression de la tâche UDP tokio + du hop) → émission RT,
  zéro gigue d'égression. `WouldBlock` → drop (PLC) plutôt que staller le RT.
- **Auto-recovery de la mort des callbacks audio** (`audio_liveness_supervisor`
  + `restart_audio_streams`) : surveille en continu les callbacks CPAL ; s'ils
  se figent en cours de session (driver ASIO qui émet un `kAsioResetRequest` non
  honoré par CPAL → callbacks haltés en silence), recrée UNIQUEMENT les streams
  CPAL en gardant encodeur/SFU/réseau (pas de re-handshake, ~100-300 ms de trou).
  Borné, erreur claire au browser si épuisé. Générique (toute interface), no-op
  macOS. Recovery validée sur PC (1 trou de ~1 s auto-résorbé).
- **Instrumentation déterministe** : `emit_burst` (frames Opus/bloc), taille du
  1er callback, latence du chemin de réception `recv_path`, débit de callbacks
  CPAL/s (`capture_cb_per_sec`/`output_cb_per_sec`).

### Notes
- Émission ET réception sont désormais **toutes deux en temps-réel** (tous les
  étages audio — capture, traitement, encodage, décodage, envoi — sur threads RT).
- Le watchdog « cold-start » 700 ms (pré-release 0.5.3-4) a été **remplacé** par
  le superviseur de liveness continu : le diagnostic initial était faux (les
  callbacks ASIO démarrent bien puis meurent ~21 s plus tard, ce n'est pas un
  démarrage à froid). Cause racine = `kAsioResetRequest` non géré par CPAL 0.15.

## [0.5.2] — 2026-06-27

> **Release latence : Opus low-delay + jitter buffer adaptatif (gigue mesurée +
> compensation de drift continue). Synthèse des pré-releases 0.5.2-1 → 0.5.2-7.**

### Added
- **Encodeur Opus en `RESTRICTED_LOWDELAY`** : **−4 ms/sens** de latence
  algorithmique (lookahead 312→120 samples, mesuré), qualité CELT identique.
- **Jitter buffer adaptatif piloté par la gigue mesurée** (RFC 3550) : la cible
  suit `k·gigue + headroom` par peer au lieu d'un ratchet réactif, avec filet
  réactif conservé (jamais moins sûr que l'historique). Plafonne le pire cas
  (queue 40→17 ms) et s'adapte à chaque réseau.
- **Compensation de drift d'horloge en continu** (resampler asservi au
  remplissage, ratio borné ±0,5 %, inaudible) : remplace les drift-drains
  discrets, tient le buffer bas malgré la dérive sender↔récepteur.
- **Instrumentation déterministe** : gigue/drift par peer (`jamodio::netstats`) +
  latence d'émission (`send_path_latency`) loggées pour piloter l'optimisation
  sur des chiffres fiables (pas l'acoustique).

### Notes
- Un **pacing d'émission** (lissage des rafales Windows/ASIO) a été tenté
  (0.5.2-5/-6) puis **retiré** (0.5.2-7) : l'instrumentation a montré qu'il
  injectait ~160 ms (sommeil-par-paquet tokio → saturation de file). La rafale
  d'émission Windows reste à traiter proprement (cf.
  `internal-docs/plans/PLAN-CHANTIER-LATENCE-2026-06.md`).

## [0.5.1] — 2026-06-26

> **Confort & robustesse : éditeurs de plugins macOS, détection ASIO sans
> redémarrage manuel, identité visuelle de l'agent.**

### Added
- **Détection ASIO sans relancer l'agent à la main (Windows).** Quand l'agent
  démarre AVANT que l'interface soit branchée (cas fréquent avec le lancement au
  login), il reste figé en WASAPI (+10-20 ms) jusqu'à un redémarrage — le choix
  du host audio est décidé une fois au boot. Un bouton **« Redémarrer l'agent »**
  apparaît désormais sur le badge WASAPI des Réglages audio (Windows, hors
  session live) : un clic relance l'agent, qui re-sonde le host → ASIO détecté,
  bascule automatique, badge vert. Le badge ASIO/WASAPI et l'état du bouton se
  mettent à jour tout seuls au retour de l'agent. Nouveau message protocole
  `relaunch-now`. **Décision : pas de re-probe du host « à chaud »** (trop
  risqué : ids device, thread COM-STA ASIO) — un redémarrage propre, comme un DAW.

### Fixed
- **Éditeurs de plugins AU mal dimensionnés (macOS).** La fenêtre d'un éditeur
  AudioUnit pouvait s'ouvrir trop petite, trop grande (marges vides autour d'une
  UI à taille fixe), ou tronquée le temps du chargement (il fallait fermer/
  rouvrir). La fenêtre **suit désormais la taille réelle du plugin** et s'adapte
  à son layout asynchrone (observation de `NSViewFrameDidChangeNotification`,
  fenêtre verrouillée sur la taille de la vue). **Aucun impact sur le VST3
  Windows** (crate séparé).
- **Agent injoignable après « Redémarrer l'agent » (Windows).** Deux causes
  empilées, isolées via les logs agent : (1) `app.restart()` relançait la
  nouvelle instance pendant que l'ancienne tenait encore le verrou
  single-instance → elle était tuée comme « 2e instance » → plus aucun agent.
  Corrigé par un **relanceur détaché** (`--awaited-relaunch`) qui attend la mort
  de l'ancien process avant de démarrer. (2) Le port WS restait ~30 s en
  `TIME_WAIT` → bind refusé (`WSAEADDRINUSE`). Corrigé par **`SO_REUSEADDR`**
  (via `socket2`, que tokio ne pose pas sur Windows). Reconnexion désormais
  immédiate (~2-3 s).
- **Clignotement de l'icône dans la barre des tâches (Windows).** L'icône
  affichait brièvement le badge puis une autre tuile au démarrage (override
  runtime hérité du tray monochrome). Override supprimé → une seule icône.

### Changed
- **Identité visuelle de l'agent.** Icône du Dock / de l'app alignée sur la
  marque (logo complet avec les points extérieurs aux extrémités des ondes) ;
  icône de la barre de menus (macOS) / barre des tâches (Windows) = **badge
  jaune Jamodio**, lisible et cohérent sur les deux OS.
- **Icône du Dock cliquable (macOS).** Cliquer l'icône du Dock affiche désormais
  la fenêtre d'infos de l'agent (au lieu de ne rien faire). L'agent assume une
  présence Dock (app « Regular ») ; le tray reste un bonus.
- **Publication des releases (CI) fiabilisée** : upload des assets idempotent
  (delete-then-upload) + verrou de concurrence par tag (corrige un échec
  `422 already exists` quand deux runs se chevauchaient).

## [0.5.0] — 2026-06-23

> **Jalon — première release validée Windows + macOS, plugins opérationnels.**

Cette version marque la **parité fonctionnelle Windows ↔ macOS** : l'agent est
désormais testé et validé de bout en bout sur les deux plateformes, plugins
inclus. Pas de nouvelle fonctionnalité depuis `0.4.41` — c'est la promotion d'un
état stable en jalon mineur. Acquis consolidés depuis `0.4.x` :

- **Plugins INSERT opérationnels sur les deux OS** : hôte AudioUnit (macOS) et
  hôte VST3 (Windows). Scan, load, process, bypass, MIDI, éditeur natif.
- **Éditeur de plugin VST3 (Windows)** : ouverture/réouverture fiables et
  **fenêtre qui suit le plugin au redimensionnement** sans rogner le GUI
  (`canResize`/`onSize`/`checkSizeConstraint`/`resizeView`, cf. `0.4.38`),
  validé sur plugin scalable (ValhallaFutureVerb).
- **Carte audio ASIO (Windows)** : énumération, capture et playback fonctionnels
  via un thread COM-STA dédié, au format natif du driver (Int32/Int16/F32),
  latence validée sur interface réelle (cf. `0.4.39` → `0.4.41`).
- **macOS Apple Silicon + Windows x64** comme cibles de release officielles
  (macOS Intel abandonné).

### Fixed
- **VST3 (Windows) : instruments détectés comme tels.** Le scan lit désormais la
  sous-catégorie VST3 (`PClassInfo2::subCategories` via `IPluginFactory2`) et
  expose un champ autoritaire `isInstrument` au browser. Avant ce correctif, un
  instrument VST3 doté d'un bus d'entrée audio (sidechain — Surge XT, BFD,
  Kontakt…) était classé comme effet via la seule heuristique `has_input_bus`,
  et la tranche **basculait à tort en AUDIO** au lieu de MIDI. La classification
  est unifiée AU/VST3 (`is_instrument` = `aumu` côté AU, premier token
  `"Instrument"` côté VST3) ; fallback `!has_input_bus` pour les plugins sans
  `IPluginFactory2`. **Aucun changement de comportement sur macOS.**

## [0.4.41] — 2026-06-23

### Fixed
- **ASIO : « stream configuration not supported » / aucun son** (suite des
  Étapes 1-2). Une fois la carte trouvée et le stream ouvert sur le thread
  COM-STA, `build_input_stream` échouait : cpal **ne convertit pas** les formats
  ASIO et exige que le type demandé == type natif du driver
  (`host/asio/stream.rs`). On ouvrait un callback `f32` alors que les interfaces
  ASIO (Focusrite, Scarlett, MOTU…) sont en **Int32** → rejet (faux toast
  « carte utilisée par une autre app »). `capture.rs`/`playback.rs` ouvrent
  désormais le stream au **format natif** du driver (f32/i32/i16) et convertissent
  vers/depuis f32 nous-mêmes (entrée : i32/i16 → f32 normalisé ; sortie : mix f32
  → type natif via scratch, sans alloc par bloc). CoreAudio/WASAPI (f32 natif) :
  branche F32, comportement inchangé.

## [0.4.40] — 2026-06-23

### Fixed
- **Capture/playback ASIO impossibles sur Windows (« ENTRÉE AUDIO
  INTROUVABLE », aucun son)** — suite de l'Étape 1 (v0.4.39, énumération).
  L'ouverture du stream CPAL (`StartCapture` → `get_input_device` +
  `build_input_stream`) tournait sur un worker tokio sans COM → `load_driver`
  ASIO (CoCreateInstance) échouait → device « introuvable » à l'entrée studio.
  Nouveau module `audio/com_exec.rs` : **thread COM-STA persistant** qui exécute
  toutes les opérations CPAL/ASIO (énumération, résolution, ouverture ET
  fermeture des streams). Un objet driver ASIO étant lié à son apartment, il est
  désormais créé, utilisé et détruit sur le même thread STA. L'énumération
  (Étape 1) est rebranchée dessus (un seul apartment pour tout l'ASIO).
  - ASIO mono-client : en capture on ferme l'ancien stream avant d'ouvrir le
    nouveau ; résolution + ouverture atomiques sur le thread STA (le
    `cpal::Device`/`Stream` !Send ne traverse jamais les threads).
  - Ouverture de la **sortie** rendue non-fatale sur échec de *build* : un edge
    ASIO (duplex) ne rend plus l'utilisateur muet pour les autres — la capture
    prime, le playback est juste indisponible jusqu'à nouvelle sélection.
  - macOS (CoreAudio, pas de COM) : exécution inline, comportement inchangé.

## [0.4.39] — 2026-06-22

### Fixed
- **Carte audio (interface ASIO) non détectée par l'agent sur Windows**
  (`jamodio-agent/src/audio/device.rs`) : `asio-sys` charge les drivers ASIO
  via `CoCreateInstance` (ASIO SDK `loadAsioDriver`) sans initialiser COM ; sur
  un thread sans COM (les workers tokio qui traitent `GetDevices`),
  `load_driver` échoue et cpal **saute silencieusement** le device → liste
  d'entrées **vide** renvoyée au browser (« Aucune entrée audio détectée »),
  alors que l'énumération au boot (thread principal Tauri/WebView2, COM STA
  déjà initialisé) voyait bien la carte. `list_inputs`/`list_outputs`
  s'exécutent désormais sur un thread frais avec `CoInitializeEx(STA)`.
  macOS (CoreAudio, pas de COM) : exécution inline inchangée → zéro impact.
  *(Étape 1/2 — énumération. La capture suivra : elle ouvre aussi le device
  sur un worker tokio et requiert le même contexte COM.)*

## [0.4.38] — 2026-06-22

### Fixed
- **Fenêtre d'éditeur de plugin VST3 redimensionnable rognait le GUI**
  (`jamodio-vst3-host/src/editor.rs`, Windows) : la fenêtre hôte était
  toujours créée en `WS_OVERLAPPEDWINDOW` (donc `WS_THICKFRAME` +
  `WS_MAXIMIZEBOX`), et aucun `WM_SIZE` ne propageait la nouvelle taille à la
  vue du plugin — redimensionner laissait du vide ou **coupait le plugin**
  (Valhalla, etc.). Correctif aligné sur les hôtes pro (Reaper/Cubase) :
  - La redimensionnabilité *utilisateur* suit `IPlugView::canResize()` : les
    plugins à UI fixe obtiennent une fenêtre non redimensionnable qui épouse
    exactement la vue ; seuls les plugins redimensionnables gardent le cadre.
  - `WM_SIZE` → `IPlugView::onSize()` : la vue suit désormais la zone client.
  - `WM_SIZING` → `IPlugView::checkSizeConstraint()` : le plugin contraint le
    drag (min/max/ratio), bord ancré respecté.
  - `IPlugFrame::resizeView` implémenté (était un no-op) : une demande de
    resize côté plugin redimensionne réellement la fenêtre hôte.
  - Dimensionnement exact via `AdjustWindowRectEx` (style/DPI/thème réels) au
    lieu des marges codées en dur `+16/+40`.

## [0.4.37] — 2026-06-12

### Added
- **Redémarrage à la demande depuis le navigateur** (`protocol.rs`,
  `ws_server.rs`, `main.rs`) : nouveau message browser→agent `Restart`,
  déclenché par le bouton « Relancer mon agent » du banner d'update. L'agent
  rejoue le flux d'auto-update (download + install de la version dispo),
  broadcaste `Shutdown` aux clients connectés, puis `app.restart()`. Avant,
  le bouton ne faisait que masquer le banner côté navigateur — il ne relançait
  rien. `WsServerHandle` porte désormais le `AppHandle` (injecté au `setup`)
  pour pouvoir déclencher `check_for_update` hors du boot.

## [0.4.36] — 2026-06-12

### Security — durcissement pré-BETA (audit adversarial)

Suite à 2 audits sécurité dédiés (surface réseau/IPC + robustesse
mémoire/concurrence) avant le lancement beta public :

- **RCE Windows bloqué** (`pipeline.rs`) : `LoadInstrumentPlugin` n'accepte
  plus QUE des plugins présents dans le cache de scan. Le `path` d'un
  `PluginRef::Vst3` venait du navigateur et arrivait jusqu'à `LoadLibrary`
  (exécution de code natif au chargement) — une page autorisée aurait pu
  faire charger une DLL arbitraire. Validation transparente (le navigateur
  ne propose que des plugins scannés).
- **Origin WS durci** (`ws_server.rs`) : webview interne reconnue par
  comparaison EXACTE (`tauri://localhost` / `http://tauri.localhost`) au
  lieu d'un préfixe (un client local pouvait forger `tauri://...evil`). Les
  origins de DEV (`http://localhost:*`, `127.0.0.1`) ne sont autorisées
  qu'en build debug → en release, seuls jamodio.com / previews Jamodio /
  webview interne / file:// passent.
- **Clés SRTP effacées de la mémoire** (`net/srtp_*.rs`) : les clés
  décodées sont en `Zeroizing` → zeroées au drop (les deux backends).
- **Checksum SDK ASIO épinglé en CI** (`release.yml`) : SHA256 vérifié
  avant compilation dans le binaire signé (supply-chain).

### Changed
- VST3 : `has_input_bus` est mis en cache à `setup_stereo` — plus d'appel
  COM `getBusCount` cross-DLL à chaque bloc audio temps-réel (risque de
  priority-inversion si le plugin verrouille en interne).
- Hygiène : README à jour (4 crates + hôtes AU/VST3), SECURITY.md (versions
  génériques mac/Windows). Workspace clippy 100 % propre (mac + Windows).

## [0.4.35] — 2026-06-12

### Fixed — LOT 3 review pré-beta : hot-path audio (qualité + RT-safety)

Deux bugs AUDIBLES du mixer corrigés (avec tests unitaires) :
- **Pan/balance continu** (`mixer.rs`) : l'ancienne loi constant-power non
  normalisée sautait de **−3 dB sur les deux canaux** dès que le fader pan
  quittait le centre exact. Remplacée par la loi de balance stéréo
  linéaire 0 dB au centre (standard DAW pour pistes stéréo) : identique au
  centre (unity) et aux extrêmes (côté plein = unity), continue partout.
  Tests : continuité au centre + extrêmes.
- **SetBuffer ne touche plus le self-monitor** (`mixer.rs`) : régler le
  buffer réseau (ex. 40 ms) multipliait par 8 la latence d'écoute de son
  propre instrument (5 ms → 40 ms) + un trou audible. Le self-monitor est
  local, il garde sa cible de 5 ms. Test : exclusion vérifiée.

Allocations supprimées du hot path (review 11/06) :
- encode stage : plus de `Vec` alloué par frame Opus (~400/s) — encode
  direct depuis l'accumulateur + drain sans collect.
- process stage : plus de clone de String par bloc audio en mode MIDI.
- `rtp::build_packet` : +24 octets de headroom → `SrtpContext::protect`
  (tag AEAD 16 o) ne réalloue plus chaque paquet (~400/s).
- `set_volume` : garde NaN alignée sur set_pan (NaN aurait silencé le
  stream définitivement).

### Changed
- VST3 : `restartComponent(kLatencyChanged)` est désormais loggé en WARN
  (latence de compensation figée au load — re-sync complet = backlog) au
  lieu d'être avalé silencieusement.

### Notes (différé, chantier mesuré post-beta)
Le pool de buffers cross-stage (to_vec du callback CPAL, remap, taps
record pendant REC) et le confinement thread des `cpal::Stream` sont
VOLONTAIREMENT différés : ils touchent la sémantique de shutdown des
3 stages pour un gain non mesuré (allocations amorties, zéro drop dans
les baselines). À faire avec mesures avant/après sur matériel réel.

## [0.4.34] — 2026-06-12

### Fixed — LOT 2 review pré-beta : MAJEURS robustesse

- **Enregistrement — plus de panic Ogg** (`record/opus_ogg.rs`,
  `record/ogg.rs`) : une page Ogg ne peut contenir que 255 segments de
  lacing ; sur un transitoire VBR à gros packets, l'ancien `assert!`
  crashait le thread record (fichiers perdus). On flushe désormais la page
  AVANT débordement (batching par segments) ; l'assert devient debug-only.
- **Enregistrement — plus de blocage au stop** (`record/mod.rs`) : si le
  finalize timeout (thread record figé), on ne `join()` plus le thread
  (laissé détaché) — sinon le thread de contrôle de l'agent se bloquait
  indéfiniment.
- **StopRecording ne gèle plus le pipeline** (`ws_server.rs`,
  `pipeline.rs`) : on extrait le handle d'enregistrement sous un lock COURT
  puis on finalise (jusqu'à 30s) HORS lock — avant, tous les autres
  handlers voyaient « overloaded » pendant tout le finalize.
- **PlayMidiNote ne bloque plus le runtime** (`ws_server.rs`) : `try_lock`
  du plugin_host au lieu de `lock` bloquant — une note de clavier HTML est
  abandonnée si un load/unload est en cours plutôt que de bloquer un worker.

### Fixed — VST3 cycle de vie (Windows)

- **`Instance::drop` appelle toujours `terminate()`** si `initialize` a
  réussi (`host.rs`) — avant, seulement si `setup_stereo` avait réussi →
  une instance qui échoue au setup (fréquent au scan) fuyait sans terminate
  (contrat IPluginBase violé).
- **Ordre de teardown au shutdown** (`lib.rs`) : champ `editor` déclaré
  avant `instance` + `Drop for Vst3Host` qui route la fermeture des
  éditeurs et les `terminate()` via vst3-main (règle single-main-thread).
- **`ExitDll` appelé avant `FreeLibrary`** (`loader.rs`) — contrat SDK
  VST3 ; release de la factory puis ExitDll puis dlclose, dans l'ordre.
- **Ouverture d'éditeur en échec** (`editor.rs`) : si `createView`/`attached`
  échoue, on déconnecte les IConnectionPoint et on terminate le controller
  séparé (sinon réouverture cassée + leak).

### Changed — clarté du dashboard fenêtre agent

Étiquettes et infobulles réécrites, lambda-friendly et HONNÊTES :
« Latence » → **« Latence locale »** (tooltip explicite : N'INCLUT PAS le
réseau ; la latence réelle avec un partenaire est dans le studio).
« Jitter » → « Gigue réseau », « CPAL » → « Buffer carte »,
« Streams » → « Musiciens reçus », « Underruns » → « Coupures ».
(L'ancien tooltip « Latence end-to-end » était trompeur.)

### Internal
- perfstats/levels : un seul flusher par agent (gate `!is_internal`). Le
  dashboard de la fenêtre agent (alimenté par GetStats, pull non-destructif)
  est inchangé.

## [0.4.33] — 2026-06-12

### Fixed — LOT 1 review pré-beta : 2 CRITIQUE teardown plugins

- **Use-after-free AU corrigé** (`au_host.mm`) : `openEditor` (v2 et v3)
  capturait un pointeur `Entry*` brut dans des blocks async à délai non
  borné (`requestViewController`) + un observer de fermeture jamais
  retiré ; un `unload` entre-temps libérait l'Entry → écriture sur mémoire
  libérée. Désormais : capture du `handle_id` + re-résolution sous lock au
  moment de l'exécution (skip si déchargé), et `removeObserver` au teardown.
- **Réentrance pump VST3 corrigée** (`main_thread.rs`) : si un plugin fait
  tourner une pompe de messages imbriquée pendant `attached()`, le drain
  de jobs ne s'exécute plus de façon réentrante (re-post si déjà en cours)
  → plus de `component.terminate()` pendant qu'`attached()` est sur la pile
  (use-after-terminate).

### Changed
- Tray : l'énumération `NotifyIconSettings` n'est plus loggée par entrée
  (un bug report ne liste plus les logiciels tray de l'utilisateur) ; seuls
  le bilan et le résultat de promotion restent loggés.

## [0.4.32] — 2026-06-11

### Security — P0 review pré-beta (3 quick-wins)

Suite à la code review senior du 11/06 :
- **Anti-replay SRTP activé sur Windows** (`net/srtp_webrtc.rs`) : le
  backend webrtc-srtp tournait SANS protection anti-replay (`Context::new`
  options à `None` → `srtp_no_replay_protection()`), alors que le backend
  macOS (libsrtp2) l'a par défaut. Fenêtre 128 posée sur le contexte
  entrant (srtp + srtcp) → parité de sécurité mac/Windows.
- **Plus de panic sur message WS non-UTF8-aligné** (`ws_server.rs`) : le
  log d'erreur tronquait par octets (`&text[..120]`) → panic possible au
  milieu d'un char multioctet, tuant la connexion avant son cleanup.
  Tronque désormais par chars.
- **Origin Vercel restreint** (`ws_server.rs`) : `ends_with(".vercel.app")`
  whitelistait TOUS les déploiements vercel.app (drive-by possible depuis
  `evil.vercel.app`). Exige maintenant le nom de projet Jamodio.

### Changed — tray Windows : version diagnostique + match de chemin robuste

Les promotions v1/v2 échouent pour une cause invisible côté dev (Mac).
v0.4.32 logge systématiquement (`target: jamodio::tray`) TOUTES les
entrées `NotifyIconSettings` vues, leur `ExecutablePath` et leur
`IsPromoted`, plus le bilan (exe courant, match exact/par nom de fichier).
Améliorations de robustesse au passage :
- **match par nom de fichier** en fallback du match par chemin exact
  (gère 8.3 / `\\?\` / casse d'un composant — causes classiques d'un
  match strict qui échoue) ;
- **read-back** après écriture : le marqueur `TrayPromotedOnce` n'est
  gravé que si `IsPromoted` est relu à 1 → si une écriture ne « prend »
  pas, on re-tente au prochain lancement au lieu de rester bloqué.

À retester : installer, lancer, ouvrir `%APPDATA%\Jamodio\logs` et
chercher `jamodio::tray` → le log dira exactement ce qui se passe.

## [0.4.31] — 2026-06-11

### Fixed — tray Windows : promotion qui fonctionne vraiment (v2)

Constat (test sur Win11) : Explorer crée l'entrée
`NotifyIconSettings` avec `IsPromoted=0` D'OFFICE → la v1, qui ne
promouvait que si la valeur était absente, ne promouvait jamais. v2 :
marqueur dédié `HKCU\Software\Jamodio\AudioEngine\TrayPromotedOnce` —
absent = première promotion → `IsPromoted=1` forcé (même sur le 0 par
défaut d'Explorer) puis marqueur gravé ; présent = on ne touche plus
jamais (un masquage utilisateur ultérieur est définitif). La valeur
précédente est loggée pour diagnostic.

## [0.4.30] — 2026-06-11

### Fixed — installeur MSI : texte enfin lisible (v2 des BMPs)

Le texte des dialogs MSI est rendu par Windows en GRIS FONCÉ, non
recolorable (captures ×2). La v1 (v0.4.29) avait dégagé les zones de
texte mais en les laissant sombres → texte foncé sur fond foncé. v2 :
- `dialog.bmp` : bande de marque sombre à gauche (glyphe + JAMODIO),
  zone de texte DROITE en clair (#f7f8f9) — la bande blanche du checkbox
  « Launch » de l'ExitDlg s'y fond naturellement.
- `banner.bmp` : fond clair, glyphe sur tuile sombre arrondie à droite
  (style icône app), liseré bas.

## [0.4.29] — 2026-06-11

### Added — ASIO par défaut sur Windows (chantier A, bloquant v0.5.0)

L'agent était compilé avec la feature cpal `asio` mais utilisait
`cpal::default_host()` partout = **WASAPI shared toujours** (+10-20 ms
évitables, silencieux).

- **`audio/host.rs` (nouveau)** : sélection du host UNE fois au boot —
  ASIO si un driver expose ≥ 1 device d'entrée, sinon WASAPI (macOS :
  CoreAudio, inchangé). Choix + raison loggés. Les 7 duplications de
  `default_host()` dans device.rs passent par ce point unique.
- **Wire** : `Devices.audioHost` (`"asio"|"wasapi"|"coreaudio"`, additif
  rétro-compatible) → le browser affiche badge vert ASIO / badge orange
  WASAPI + toast unique au join + lien support optimiser-latence.
- **Pas de fallback silencieux** : échec d'ouverture sur host ASIO
  (driver mono-client tenu par un DAW) → `CaptureError` dédiée
  `asio-open-failed` avec message UI explicite, PAS de bascule WASAPI
  cachée qui mentirait sur la latence.
- Buffers : `buffer_size.rs` gérait déjà les Range ASIO (16-4096) →
  Fixed(128) négocié comme avant.

### Added — Quit UX Windows (chantier C)

- **Tray auto-épinglé Windows 11** (`tray_promote.rs`) : écrit
  `IsPromoted=1` dans `HKCU\Control Panel\NotifyIconSettings\<id>`
  UNIQUEMENT si la valeur est absente (premier run) — un masquage
  volontaire par l'utilisateur n'est jamais écrasé. Win10 : no-op loggé.
- **Bouton « Quitter l'agent »** dans la fenêtre agent (filet quand
  l'icône tray est masquée). Sortie unifiée `graceful_quit` : broadcast
  `Shutdown{reason:"quit"}` aux browsers connectés puis exit — le menu
  tray Quitter passait par `app.exit(0)` sec sans prévenir personne,
  corrigé au passage.

## [0.4.28] — 2026-06-11

### Fixed — polish éditeur VST3 Windows (suite v0.4.27, qui OUVRE la fenêtre ✅)

- **Fenêtre éditeur derrière l'UI Jamodio** : l'agent est un process
  background → `SetForegroundWindow` est refusé par Windows (d'où le
  comportement non systématique). Fix : toggle `HWND_TOPMOST` →
  `HWND_NOTOPMOST` après `ShowWindow` (place la fenêtre devant sans voler
  le focus clavier) + `SetForegroundWindow` best-effort.
- **Réouverture impossible après fermeture** : à la fermeture, on ne
  déconnectait jamais component↔controller et on ne terminate() jamais le
  controller. Le component JUCE gardait le pointeur de l'ancien controller
  et **ignorait** le handshake du suivant (`if juceVST3EditController ==
  nullptr` dans son notify) → `createView` null à la 2e ouverture. Fix :
  teardown complet sur WM_DESTROY (ordre plugprovider SDK) : `removed()` →
  release view/frame/handler → `disconnect()` des deux IConnectionPoint →
  `terminate()` + release du controller (uniquement si instance séparée du
  component).

### Fixed — tray icon Windows invisible

`tray.png` est un glyphe "template" gris foncé : macOS le recolore selon le
thème (`iconAsTemplate`), Windows l'affiche tel quel → quasi invisible sur
la barre des tâches sombre par défaut (il fallait le Gestionnaire des
tâches pour quitter l'agent). Sur Windows, l'icône du tray est désormais
l'icône couleur de l'app (tuile noire + glyphe jaune) — visible sur thème
sombre et clair. Menu Afficher/Quitter inchangé.

### CI — NSIS plus buildé pour rien

`--bundles` explicite par plateforme dans release.yml (`app,dmg` macOS,
`msi` Windows) : le bundler NSIS tournait à chaque build Windows (~2 min
perdues) alors que seul le MSI est publié.

## [0.4.27] — 2026-06-11

### Fixed — VST3 editor Windows : fix racine (thread vst3-main unique)

Cause racine ENFIN identifiée en lisant les sources JUCE (Surge XT et
Valhalla sont des plugins JUCE) — elle explique les DEUX symptômes
précédents d'un coup :

1. **Hang `attached()` (v0.4.24)** : JUCE lie son MessageManager au thread
   qui construit la `JucePluginFactory` (= le thread qui appelle
   `GetPluginFactory` après le dlopen — chez nous le thread WS/scan, qui ne
   pompe jamais de messages Win32). `attached()` → `addToDesktop` →
   constructeur `HWNDComponentPeer` → `callFunctionIfNotLocked` : si le
   thread appelant n'est pas le message thread JUCE, la création de fenêtre
   est marshalée vers lui via `callFunctionOnMessageThread` et **bloque pour
   toujours** (fenêtre blanche "NOT RESPONDING").

2. **`createView` null (v0.4.25/26)** : avec un ConnectionProxy entre
   component et controller (pattern SDK), les plugins JUCE ne peuvent plus
   faire leur cast direct privé et basculent sur un handshake par message
   (`JuceVST3EditController::connect` → `sendIntMessage`). Ce fallback
   alloue le message via `hostContext->createInstance(IMessage)` — que notre
   `IHostApplication` ne fournissait pas (`kNotImplemented`). Handshake mort
   en silence → le controller n'a jamais son AudioProcessor → `createView`
   retourne null. Le filtre de threads du proxy n'y était pour rien.

Fix (refactor interne `jamodio-vst3-host`, API publique inchangée) :

- **Nouveau thread `vst3-main` persistant** (`main_thread.rs`) : STA + pump
  Win32 permanente. TOUT le non-RT y vit désormais : scan, dlopen/factory,
  createInstance, initialize, setup, controller, connect, createView,
  attached, fenêtres d'éditeur (plus de thread par fenêtre — registry
  HWND→état nettoyée sur WM_DESTROY). C'est la règle "single main thread"
  de la spec VST3, et ce que font tous les DAW. `process()` reste sur le
  thread audio RT (conforme spec).
- **`IHostApplication::createInstance` implémente `IMessage` +
  `IAttributeList`** (`host_app.rs`, équivalent `hostclasses.cpp` du SDK) →
  le handshake JUCE à travers le ConnectionProxy fonctionne.
- **Host context passé à `IComponent::initialize`** (était null).
- Bonus : `unload` ferme l'éditeur AVANT `terminate()` du component
  (use-after-terminate latent corrigé), et une fenêtre fermée par
  l'utilisateur (X) peut être réouverte.

## [0.4.26] — 2026-06-11

### Fixed — VST3 editor createView null

v0.4.25 a éliminé le hang `attached()` via ConnectionProxy (= pattern
Steinberg) mais déplacé le problème : `createView` retournait désormais
null. Cause : le ThreadChecker du SDK (strict "thread créateur uniquement")
est trop restrictif pour notre modèle multi-thread. Il droppait aussi les
notify() venant du thread WS load (= initialisation du controller par
le component) → controller jamais sync → `createView` null.

Fix : filtre asymétrique inversé. Au lieu de bloquer tout sauf le thread
éditeur STA, on **autorise tout SAUF le thread audio RT** (= seul cas où
le marshaling cross-thread vers editor STA pendant `attached()` cause un
deadlock). Le thread audio se marque explicitement via
`jamodio_vst3_host::register_audio_thread()` appelé au start du
`process_stage_loop`.

- Notify depuis WS load thread (= component → controller initial sync) :
  autorisé → controller reçoit son état → `createView` peut succeed.
- Notify depuis editor STA thread (= UI knob change → component) :
  autorisé → param changes UI fonctionnent.
- Notify depuis audio RT thread (= component send param during process) :
  drop → pas de deadlock pendant `attached()`.

Coût : on perd les param-change notifications générées DEPUIS le thread
audio (rare en pratique, l\'utilisateur tourne les knobs depuis l\'UI).

### Notes

Le pattern Steinberg SDK est correct pour un host single-threaded (= leur
`editorhost` sample). Pour Jamodio qui est multi-thread (WS tokio + audio
RT + editor STA), il fallait adapter le filtre — c\'est ce que fait
v0.4.26.

## [0.4.25] — 2026-06-11

### Fixed — VST3 editor hang (Windows)

`IPlugView::attached()` hangait systématiquement sur tous les plugins VST3
commerciaux testés (Valhalla FutureVerb, Surge XT) — fenêtre éditeur blanche,
statut "NOT RESPONDING", aucun log après `calling attached…`. Reproduit
sur VM Windows 11 ARM emul ET sur PC Windows x64 natif → bug dans notre
host, pas un quirk émulation comme initialement supposé.

**Cause** : on connectait directement les 2 `IConnectionPoint` du plugin
(component ↔ controller) sans wrapper. Cela permettait à un `notify()`
depuis l'audio thread RT de marshalize cross-thread vers notre éditeur STA
qui était assis dans `attached()` → **deadlock circulaire**.

**Fix** : nouveau module `conn_proxy.rs` qui implémente le pattern
`ConnectionProxy` du SDK Steinberg (`public.sdk/source/vst/hosting/connectionproxy.cpp`).
Le proxy stocke `GetCurrentThreadId()` à la création (= sur le thread
éditeur STA), et `notify()` retourne `kResultFalse` si l'appel vient
d'un autre thread. Asymétrique : seul le UI thread peut envoyer des
notify ; les notify depuis l'audio thread sont silencieusement droppés —
exactement comme dans le SDK officiel.

Branché dans `editor.rs::connect_component_to_controller_via_proxies()`
appelé sur le thread éditeur juste avant `setComponentHandler` et
`createView`. Les 2 ComWrappers de proxies sont keep-alive locaux au
thread (le plugin garde les pointeurs en cache).

### Notes

- Le code éditeur (HWND, msg pump, COM STA, IHostApplication,
  IComponentHandler, IPlugFrame, IBStream state sync, setFrame) était
  déjà conforme à la spec — il manquait juste ce wrapping crucial des
  `IConnectionPoint`. Pris du SDK officiel.
- Aucun changement comportemental côté audio path (process_stereo, MIDI
  dispatch, etc.).
- Aucun impact mac (le code éditeur est `#[cfg(target_os = "windows")]`).

## [0.4.24] — 2026-06-10

### Added — Sample rate natif exposé dans la liste des devices

Extension du Q3 Chantier A (garde-fou 48 kHz) : le sample rate natif
est désormais disponible **dès la liste des devices** retournée par
`GetDevices`, pas seulement après `CaptureStarted`. Permet d'afficher
le badge UI dans la modal Paramètres audio **hors session** (page
"Mes Studios" / Paramètres), sans devoir entrer dans un studio.

**Protocole** :
- `protocol::AudioDevice` reçoit un nouveau champ
  `nativeSampleRate: u32` (camelCase JSON via serde). 0 si la probe
  CPAL `default_input_config` / `default_output_config` échoue
  (device introuvable ou driver KO).
- `audio::device::list_inputs` et `list_outputs` : refactor pour
  appeler `default_input_config` / `default_output_config` une seule
  fois (récupère channels ET sample rate), puis remplit les 2 champs.
  Cohérence des deux infos garantie.

Rétrocompat browser : champ ajouté, pas retiré. Les browsers pré-Q3
ignorent le champ inconnu (comportement JSON standard).

Côté browser (= monorepo Jamodio, hors scope agent) : le helper
`resolveDeviceFormat` dans `studio-settings-modal.js` a maintenant 2
sources de vérité (priorité décroissante) :
1. `agent-input-status.js#getStatus()` (= `capture-started` reçu) —
   le plus fiable, c'est le format réellement utilisé par le pipeline.
2. Lookup dans la liste des devices agent du device courant sélectionné
   et lecture de son `nativeSampleRate` — utile dès la page Paramètres
   hors session.

Plus refresh live du badge quand l'utilisateur change le device dans
le select (`refreshAudioFormatBadge` greffé sur l'event `change`).

## [0.4.23] — 2026-06-10

### Added — Garde-fou 48 kHz natif (Q3 Chantier A)

Exposition côté agent du sample rate natif du device d'entrée pour
permettre au browser d'alerter l'utilisateur quand le resampler Rubato
est actif (= ~29 ms de latence cachée non visible dans le budget).

**Protocole** :
- `protocol::AgentMessage::CaptureStarted` reçoit un nouveau champ
  `nativeSampleRate: u32` (renommé en camelCase pour le JSON via serde).
  Populé depuis le `native_sr` retourné par CPAL au start_capture, déjà
  disponible — pas de probe supplémentaire.
- `pipeline::CaptureStartedInfo` étendu du même champ. Propagé dans
  `ws_server.rs` au moment de construire le message agent → browser.

**Cible** : surtout Windows (Realtek HD Audio onboard configuré en
44 100 Hz par défaut dans Sound Properties) + interfaces USB
grand public mal configurées. Sur Mac, la quasi-totalité des cartes
pro tournent déjà en 48 kHz natif (peu impacté).

Rétrocompat browser : champ ajouté optionnel. Les browsers pré-Q3
ignorent simplement le nouveau champ (comportement JSON standard).

Côté browser (= monorepo Jamodio, hors scope agent) : nouveau module
`lib/agent-input-status.js` qui stocke le dernier device confirmé,
badge UI dans la modal Paramètres audio (vert si 48 kHz, rouge avec
explication ~29 ms cachées + lien vers article support), toast au
join anti-spam via localStorage, article support enrichi avec
procédure complète Windows Sound Properties.

### Fixed — Playback Windows shared mode : fallback symétrique au capture

Avant : `audio::playback::start_playback` forçait `BufferSize::Fixed(128)`
sans vérifier que le device output supporte cette valeur. Sur une sortie
Windows WASAPI shared mode qui n'expose pas Fixed(128) dans son
`SupportedBufferSize::Range` (sortie jack onboard Realtek, HDMI),
`build_output_stream` échouait avec `StreamConfigNotSupported` →
studio bloqué côté agent.

Côté capture, la logique existait déjà depuis v0.3.2 (helper
`device_supports_fixed_buffer` + fallback `Default`). Le bug était une
asymétrie input/output pure — invisible sur Mac (CoreAudio output expose
toujours Fixed(128)) et sur Windows ASIO / WASAPI exclusive, mais
bloquait potentiellement les Windows WASAPI shared output (= cible
"sans carte audio externe").

Refactor au lieu de duplication :
- nouveau module `audio::buffer_size` avec helper générique
  `configs_support_fixed_buffer<I>` sur
  `impl Iterator<Item = SupportedStreamConfigRange>`.
- `capture.rs` : helper interne devient un fin wrapper sur le générique
  (3 lignes au lieu de 20). Import `SupportedBufferSize` retiré.
- `playback.rs` : helper symétrique `device_supports_fixed_buffer` côté
  output + fallback `Default` avec le même log info que capture
  (cohérence diagnostique).

Aucun impact comportemental Mac. Résout silencieusement le crash
potentiel Windows shared sans changer la sémantique du happy path.

### Changed — Télémétrie buffer CPAL séparée input/output (wire honnête)

Avant : `pipeline.buffer_samples: u32` était HARDCODÉ à 128 au démarrage
de capture, et le wire `Stats.bufferMs` en dérivait. Le commentaire du
protocole disait « Identique côté in/out car on utilise
`BufferSize::Fixed(128)` des deux côtés » — **mensonge** dès que capture
ou playback fallback sur `BufferSize::Default` (Windows shared, post fix
ci-dessus). Conséquence : `totalLatencyMs` calculé avec un double input
estimé à 2,67 ms même quand le driver réel imposait 10 ms → la latence
end-to-end annoncée sous-estimait la réalité d'environ 14 ms sur les
setups Windows shared.

Refactor en vraie source de vérité :

- `audio::capture::start_capture` retourne désormais
  `(Stream, channels, sr, Option<u32>)` : le 4ᵉ élément = `Some(N)` si
  `BufferSize::Fixed(N)` accepté par le driver, `None` si fallback
  `Default`.
- `audio::playback::start_playback` retourne `(Stream, Option<u32>)`,
  symétriquement.
- `pipeline.buffer_samples: u32` REMPLACÉ par deux champs séparés
  `input_buffer_samples: Option<u32>` et `output_buffer_samples: Option<u32>`
  qui reflètent exactement ce que les fonctions `start_*` ont appliqué.
  Propagation aux 4 sites d'appel (start_capture, deux start_playback,
  restart_playback). Reset propre dans `stop_capture` (input only —
  playback peut continuer en écoute peers) et `stop_all` (les deux).
- `protocol::Stats` : commentaire `bufferMs` corrigé (deprecated,
  sémantique historique = input). Ajout `inputBufferMs: Option<f32>` et
  `outputBufferMs: Option<f32>` avec `skip_serializing_if = "Option::is_none"` :
  les champs sont ABSENTS du JSON quand le driver est en `Default`
  (impossible à mesurer précisément côté agent sans instrumenter le
  callback CPAL).
- `ws_server.rs` : recalcule séparément `input_buf_ms_est` et
  `output_buf_ms_est`, avec fallback estimé 10 ms (= 480 samples / 48,
  valeur conservatrice typique du mode WASAPI shared) si
  `Option` est `None`. `totalLatencyMs` utilise désormais
  `input + opus_enc + opus_dec + jitter + output` au lieu du double-input
  hérité — corrige le calcul faux sur Windows shared.
- `bufferMs` conserve sa sémantique d'avant (= valeur input estimée)
  pour rétrocompat avec les browsers pré-Q3 qui le sommaient avec
  `opus_ms`.

Validé en local (MacBook Pro mic interne) — wire JSON observé :
`inputBufferMs: 2.67`, `outputBufferMs: 2.67`, `bufferMs: 2.67`
(identique), `captureLatencyMs: 5.17` (= 2.67 + 2.5),
`playbackLatencyMs: 2.67`, `totalLatencyMs: 10.33`
(= 2.67 + 2.5 + 2.5 + 0 + 2.67, streams = 0). Sur Windows shared
(post-lancement), les champs `Option<f32>` seront absents si fallback
`Default` côté capture, et `totalLatencyMs` reflétera enfin la réalité
~25 ms vs ~10 ms aujourd'hui mensonger.

Rétrocompat browser : aucun champ retiré, deux champs ajoutés
optionnels. Les browsers pré-Q3 ignorent simplement les nouveaux
champs (comportement standard JSON).

## [0.4.22] — 2026-06-09

### Added — Talkback auto-mute (signaux MIDI + audio RMS)

Le pipeline expose désormais 2 signaux pour piloter l'auto-mute du
talkback côté browser quand l'utilisateur joue de son instrument :

- **`midi_active`** (`AtomicBool`) : passé à `true` à chaque MIDI Note ON
  reçu par le `process_stage` (status `0x9N` + velocity > 0). Reset à
  `false` après 200 ms sans nouvel event (timeout vérifié à chaque bloc).
  Couvre les claviers USB-MIDI hardware ET le clavier MIDI virtuel HTML
  (qui transite par l'agent dans tous les cas).
- **`input_rms`** : déjà calculé post-plugin (sqrt(mean(samples²))),
  désormais exposé dans le payload WS `StreamLevels` (10 Hz).

Le message `StreamLevels` accueille deux nouveaux champs optionnels
`inputRms: Option<f32>` et `midiActive: Option<bool>` (back-compat : un
browser plus ancien ignorera ces champs sans erreur).

Côté browser, un détecteur d'activité avec hold time 250 ms consomme
ces signaux et fait passer le bouton TALK en jaune pendant que
l'instrument joue (mode AUTO sur la tranche voix), sans toucher au
chemin audio chaud (`voiceTrack.enabled` uniquement, zero-latence).

## [0.4.21] — 2026-06-08

### Reverted — Variante A (ticker silencieux mode MIDI)

Rapport BETA (v0.4.18 → v0.4.20) : craquement numérique
reproductible sur swaps successifs MIDI↔AUDIO. 3 tentatives de fix
(probe_input_format v0.4.19, drain 80 ms v0.4.20) n'ont pas résolu en
pratique. La Variante A introduisait un swap de source CPAL↔ticker au
moment de la bascule mode, et la frontière entre les samples du vieux
source (CPAL réel ≠ 0) et du nouveau (ticker silence = 0, ou
réciproquement) ne parvenait pas à être lissée de façon fiable malgré
le Chantier C conceal_fade.

#### Retour à la stratégie v0.4.17

CPAL **toujours ouvert** dans les deux modes. En mode MIDI, ses samples
sont **forcés à 0 en software** côté `process_stage` :

```rust
if matches!(*input_source.lock(), InputSource::Midi(_)) {
    stereo.fill(0.0);
}
```

Zéro swap de source = zéro risque de craquement à la frontière des
buffers audio. Le plugin instrument INSERT (BFD, Kontakt, AUSampler…)
génère son audio depuis les events MIDI, indépendamment du contenu du
buffer d'entrée (qui est silencieux).

**Coût** : 1 callback CPAL + 1 `stereo.fill(0)` par bloc audio
(2,67 ms) = ~0,01 % CPU. Négligeable.

**Compromis assumé** : un device de routing externe (Pro Tools Audio
Bridge, BlackHole…) reste actif côté driver pendant le mode MIDI, mais
ses samples ne polluent jamais le mix car ils sont écrasés à 0 dès
l'entrée du process_stage. Le risque de fuite signal est éliminé en
software, pas en hardware.

#### Conservé

- Chantier #1 — MIDI sample-accurate (`CapturedMidiEvent.captured_at`,
  frame_offset calculé sample-accurate, helpers `midi_frame_offset` +
  `dispatch_subblock_midi`, 11 tests `midi_dispatch_tests`).
- Chantier #3 — suppression latency-equalizer.
- Chantier C — conceal_fade local mode pour le self-monitor.
- Tous les fixes clippy strict (build sous `-D warnings`).

#### Supprimé

- `audio/midi_clock.rs` (entier — 405 lignes).
- `CaptureMode` enum + champs `capture_mode`, `capture_format`,
  `capture_sample_tx` de `PipelineState`.
- `probe_input_format` helper + ses 4 tests.
- `swap_capture_mode` method.
- Branche MIDI dans `start_capture`.
- `playback_independence_tests` (verrou architectural sans objet
  maintenant que le swap n'existe plus).
- Feature `Win32_Media` / `Win32_Media_Multimedia` de `windows-sys`
  (était pour timeBeginPeriod du ticker, plus utilisé).

Net : **−825 lignes**. Code simpler, plus sûr. 58 tests workspace OK
(30 jamodio-agent + 9 au_host + 19 audio_core). Clippy strict propre.

## [0.4.20] — 2026-06-08

### Fixed — Craquement numérique sur swaps successifs MIDI↔AUDIO

Rapport BETA v0.4.19 :
- Entrée studio en MIDI : OK
- Bascule AUDIO : OK (1er swap propre)
- Re-bascule MIDI : **craquement numérique**
- Re-bascule AUDIO : **craquement numérique**

#### Cause

Dans `swap_capture_mode`, le nouveau source démarrait **immédiatement**
après le drop de l'ancien, sans laisser au pipeline aval le temps de
drainer ni au mixer self-stream le temps de underrun. La frontière entre
les samples du vieux source (CPAL audio ≠ 0) et du nouveau (ticker
silence = 0, ou réciproquement) tombait au milieu du buffer audio sans
transition → step d'amplitude → click.

Le 1er swap MIDI→AUDIO sortait propre par hasard : amplitude micro
encore basse (= ambient au démarrage de session). Les swaps suivants
attrapaient le mic à niveau plein de jeu → click net audible.

#### Fix

`SWAP_DRAIN_MS = 80 ms` d'attente entre drop et install dans
`swap_capture_mode`. Pendant ce gap :

1. `sample_tx` channel se vide (encoder consomme les résidus)
2. `capture_stage` / `process_stage` / `encode_stage` time-out sur
   `recv_timeout` en cascade (~50 ms)
3. Mixer self-stream sous-alimenté → underrun → Chantier C
   `conceal_fade_out` (2 ms inaudible) au lieu d'un step
4. Nouveau source démarre frais → premier bloc fondu-in via
   Chantier C `conceal_fade_in_remaining`

80 ms = sous le seuil de latence perceptible pour une bascule initiée
par l'utilisateur (< 100 ms = "instantané" perçu).

#### Refactor symétrique

Résolution device AVANT drop sur la branche MIDI→AUDIO. Sinon un échec
de résolution device laissait le pipeline orphelin sans source
(encoder en silence permanent). Désormais : si la résolution device
fail → erreur retournée **avant** toute opération destructive, le mode
courant reste actif.

## [0.4.19] — 2026-06-08

### Fixed — Swap MIDI→AUDIO échouait sur devices > 2ch (régression v0.4.18)

Reproduction (rapport BETA) :
1. Entrer en studio en mode MIDI (clavier + plugin instrument INSERT chargé)
2. Commuter en mode AUDIO depuis l'UI (panneau Source d'entrée)
3. Toast bloquant :
   `format device 4ch/48000Hz incompatible avec encoder 2ch/48000Hz`
4. L'agent reste en mode MIDI orphelin — l'audio input ne fonctionne plus
   jusqu'à un stop/start complet de session.

#### Cause

Régression introduite dans le Chantier #2 Variante A (v0.4.18) : au
`start_capture` en mode MIDI, l'encoder était configuré avec un format
canonique HARDCODÉ `(2 ch, 48 kHz)`. La "limitation v1" documentée
supposait que la majorité des users ont une interface stéréo — c'était
FAUX : les interfaces pro grand public (Scarlett 2i2/4i4, Focusrite,
MOTU, Apollo…) exposent quasi toutes 4+ canaux côté CPAL.

#### Fix

Nouveau helper `probe_input_format(input_id) -> (channels, sample_rate)`
qui interroge `default_input_config` du device cible **sans l'ouvrir**.
Utilisé en mode MIDI au `start_capture` pour configurer l'encoder ET le
ticker silencieux au format que CPAL utilisera au swap MIDI→AUDIO.

Fallback `(2, 48_000)` si le device est introuvable ou la probe échoue
(edge case rare = device disparu mid-session).

Message d'erreur du swap reformulé pour décrire le scénario réel (= device
a changé de format en cours de session) plutôt que la technique interne.

Tests de non-régression ajoutés (4 dans `probe_input_format_tests`) :
- Fallback canonique sur device fantôme (= verrou du bug v0.4.18).
- Pas de panic sur input_id mal formé (chemins critiques).
- Pas de panic sur `None` (1er lancement).
- Tuple retourné toujours valide comme entrée de `MidiSilenceClock`.

Validation BETA attendue : reproduire la séquence Scarlett → swap doit
réussir silencieusement, audio input fonctionnel immédiatement.

## [0.4.18] — 2026-06-08

### Removed — Latency-equalizer (sous-système entier, Chantier #3)

L'égaliseur de latence côté serveur (broadcast `latency-align` toutes les
2 s, computé sur le RTT WebSocket) écrasait la cible adaptative du jitter
buffer (5-40 ms autonome) en la forçant à `10 + delay_ms` via `SetPeerDelay`.
En pratique, un seul peer pathologique (drift d'horloge > 200 ppm, Wi-Fi
instable…) tirait toutes les cibles de la room jusqu'à 100+ ms et déclenchait
un drift drain régulier visible dans les perfstats BETA.

Suppressions côté agent :
- `BrowserMessage::SetPeerDelay` (protocol).
- Handler `SetPeerDelay` (`ws_server`).
- `AudioMixer::set_peer_delay` + constantes `REMOTE_BASE_TARGET_MS` +
  `PEER_DELAY_HYSTERESIS_MS`.
- `MAX_ALIGN_TARGET_MS` (200 ms) du ring buffer — `set_target_ms` clampe
  désormais à `[MIN_TARGET_MS=5, MAX_TARGET_MS=40]`, cohérent avec
  l'adaptation auto.

L'ancrage temporel inter-peer (métronome + backing track) reste assuré par
la clock-sync NTP (`pong.serverTs` + `_clockOffset`), inchangée.

### Added — MIDI sample-accurate (Chantier #1)

Le path `clavier MIDI USB → midir → encoder → plugin instrument` quantifiait
chaque event au début du bloc audio (`frame_offset: 0`), soit ±1,33 ms RMS
de jitter aléatoire par frappe. Sur une batterie électronique (Alesis Nitro
Max → BFD Player), les pairs distants entendaient les drums avec un
flottement subtil mais reproductible (vs guitare directe sample-accurate).

Fix : nouvelle struct `CapturedMidiEvent { captured_at: Instant, data }` posée
dans le callback midir, transportée via channel crossbeam vers l'encoder
thread, et convertie en `MidiEvent { frame_offset, data }` au drain :

```
frame_offset = (captured_at − block_start) × 48 / 1000 µs
```

clampé dans `[0, n_pairs)` (events de queueing → 0, events tardifs → max).
Précision : ~20 µs (= sample 48 kHz lui-même), DAW-grade.

Helpers extraits + 11 tests unitaires (`midi_dispatch_tests`) :
- `midi_frame_offset(captured_at, block_start, max_offset)` — conversion µs
  → frame_offset avec snap au début et clamp à la fin.
- `dispatch_subblock_midi(events, sub_start, sub_end, out)` — route chaque
  event vers le sous-bloc qui contient son `frame_offset` absolu (l'ancien
  `first_subblock` qui collait tout au sous-bloc 0 disparaît).

Cas particulier conservé en `frame_offset=0` : le clavier HTML virtuel
(`PlayMidiNote` via WS) — son timing source n'est de toute façon pas
sample-accurate (mousedown/keydown + lag UI + transit WebSocket).

### Added — Ticker silencieux en mode MIDI (Variante A, Chantier #2)

Quand `input_source = InputSource::Midi(_)` et qu'un plugin instrument INSERT
est chargé, la capture audio CPAL n'a aucune utilité : le plugin génère
son audio depuis les events MIDI. Avant ce sprint, l'agent gardait CPAL
ouvert et forçait `samples = 0` côté process_stage = 1 callback CPAL / bloc
+ 1 lecture device + 1 fill(0) pour rien, et un device de routing externe
(Pro Tools Audio Bridge, BlackHole…) restait actif et risquait d'injecter un
signal parasite.

Architecture :

```
enum CaptureMode { Audio(SendStream), Midi(MidiSilenceClock) }
PipelineState {
    capture_mode: Option<CaptureMode>,
    capture_format: Option<(u16, u32)>,      // fixé au start_capture
    capture_sample_tx: Option<Sender<...>>,  // pour re-créer le mode au swap
}
```

L'encoder thread aval reste strictement identique : il consomme `sample_rx`
sans savoir si la source est CPAL ou ticker. Le swap CPAL↔ticker en amont
est transparent.

Nouveau module `audio/midi_clock.rs` :
- Thread dédié promu RT (workgroup macOS / MMCSS Windows / generic Linux),
  cohérent avec encoder thread.
- Deadline absolue (`start + n × block_duration`) + sleep coarse + busy-spin
  pour les derniers 300 µs → précision ±100 µs.
- Windows : `timeBeginPeriod(1)` activé via RAII pour amener la granularité
  sleep de 15 ms (défaut) à 1 ms.
- Drop : signal stop + join, arrêt borné à ~2,7 ms.
- 7 tests unitaires (rate approx, drop, format custom 4ch/44,1 kHz, channel
  full, channel disconnected, validation params).

Transitions gérées par `set_input_source` :
- Hors session (`capture_mode = None`) : update préférence seulement, le
  prochain `start_capture` instancie le bon mode.
- En session, bascule de catégorie AUDIO↔MIDI : swap atomique du
  `CaptureMode` au format fixé par `start_capture`. Gap de silence ≤ 1 bloc
  audio (~2,7 ms) côté pipeline aval.
- En session, même catégorie (Midi→Midi nouveau device) : ré-ouverture
  MIDI uniquement, pas de swap.

Limitation v1 explicite : la bascule MIDI→AUDIO en cours de session échoue
si le device audio courant ne supporte pas le format `(channels,
sample_rate)` fixé au `start_capture`. Diagnostic clair, recovery par
`stop_capture` + `start_capture`.

Code mort éliminé :
- `if matches!(src, InputSource::Midi(_)) { stereo.fill(0.0); }` dans
  process_stage — le ticker pousse déjà des zéros au format attendu.
- Paramètre `input_source: Arc<Mutex<InputSource>>` retiré de
  `encoder_thread` et `process_stage_loop`.

### Chore — 14 warnings clippy nettoyés

Build agent désormais propre sous `cargo clippy --workspace --all-targets
-- -D warnings`. Permet de gater la CI sur clippy strict pour les chantiers
futurs.

Tests : 65 OK (37 jamodio-agent dont 18 nouveaux midi_dispatch + midi_clock,
9 au_host, 19 audio_core).

## [0.4.17] — 2026-06-05

### Fixed — MIDI physique muet après bascule AUDIO→MIDI in-session

Reproduction (Mac AU + Grand Piano, clavier MIDI physique) :
1. Configurer le clavier MIDI dans Paramètres AVANT d'entrer dans le studio.
2. Entrer dans le studio en mode agent + plugin instrument → MIDI physique
   OK (le plugin reçoit les notes, le son sort).
3. Bascule la source d'entrée à AUDIO (pour brancher une guitare par exemple).
4. Re-bascule en MIDI + re-sélectionne le même clavier MIDI physique.
5. Les notes jouées sur le clavier physique **ne déclenchent plus aucun son**.
   Le clavier virtuel HTML (touches piano sur le browser) marche toujours.

#### Bug racine

`encoder_thread` reçoit `midi_event_rx: Option<Receiver<MidiEvent>>` **par
valeur** au `start_capture` (= clone du receiver crossbeam figé à ce moment).
Au passage AUDIO, `set_input_source` met `self.midi_event_rx = None`. Au
retour MIDI, un **nouveau** channel `(tx, rx)` est créé via `bounded::<…>(256)`
et `self.midi_event_rx = Some(rx)` — mais l'encoder garde son ancien clone
pointant sur l'ANCIEN channel orphelin. Les notes du clavier physique partent
dans le nouveau `tx` → personne ne les lit → silence.

Le clavier virtuel HTML, lui, contourne ce channel (`PlayMidiNote` côté WS
appelle directement `plugin_host.dispatch_midi_only(handle, &[event])`),
ce qui explique pourquoi il continuait à fonctionner.

#### Fix

`midi_event_rx` devient `Arc<Mutex<Option<Receiver<MidiEvent>>>>` (champ
`PipelineState` + paramètres `encoder_thread` + `process_stage_loop`).
`set_input_source` swappe désormais l'Option intérieure via `lock()`, et
l'encoder thread lookup l'Option via `lock()` à chaque bloc audio (= 375 Hz,
parking_lot non-contendu en régime établi → coût négligeable, set_input_source
est rare).

Conséquences :
- Bascule MIDI ↔ AUDIO ↔ MIDI sans restart de la capture, sans gap audio,
  sans churn SFU/SRTP.
- L'encoder voit immédiatement le nouveau receiver au prochain bloc.
- Plugin instrument + handle + bypass + workgroup CoreAudio : tous préservés
  (aucun thread RT recréé).

#### Notes

- `cargo build --release` : OK, zéro warning.
- `cargo test --workspace` : 47 verts (aucun nouveau test — le scénario est
  difficile à reproduire en unit sans mocker `midi::MidiInput::open`, qui
  ouvre un device physique. Test runtime côté beta tester valide le fix).
- Aucun changement de protocole WS, 100 % compat browser v0.4.1+.
- Build matrix CI inchangée.

## [0.4.16] — 2026-06-04

### Fixed — S5 fix #1 : retry+backoff sur ouverture CPAL post-restart agent

Quand l'agent vient de redémarrer (auto-update via tauri-plugin-updater,
kill manuel, install nouvelle release), la **1ʳᵉ ouverture du device input
peut échouer** parce que le driver audio USB n'a pas encore fini de se
libérer du process précédent. Côté CPAL/CoreAudio le symptôme est un
timeout :

```
ERROR CPAL input: A backend-specific error has occurred:
       timeout waiting for sample rate update for device
```

Conséquence : le browser tombe en **fallback WebRTC silencieux** (mode
musique en 25-30 ms au lieu du mode HD agent), et l'utilisateur devait
sortir + re-rentrer dans le studio pour que la 2ᵉ tentative passe.
Observé sur Scarlett Solo 4th Gen mais **générique** : tout driver USB
lent à libérer son device après un restart de process est concerné.

#### Fix

Nouveau helper pur `retry_with_backoff` (`capture.rs`) générique sur
`F: FnMut() -> Result<T, E>`, qui rejoue l'opération avec un slice de
`Duration` fourni. Total d'essais = `backoffs.len() + 1`. Découplé du
`sleep` (durée passée en paramètre) → testable sans I/O en utilisant
`&[Duration::ZERO; N]`.

`start_capture` enveloppe désormais `device.build_input_stream` dans
ce helper avec `BUILD_STREAM_BACKOFFS = [200ms, 500ms, 1000ms]` :
- **4 essais max**, pire-cas **~1,7 s** avant remontée de l'erreur
  (vs fallback WebRTC silencieux immédiat avant).
- `is_retryable` fail-fast sur `BuildStreamError::StreamConfigNotSupported`
  (= config invalide channels/SR/buffer → retry inutile). Toutes les
  autres variantes (timeout, `DeviceNotAvailable`, `BackendSpecific`)
  sont supposées transitoires.
- **Latence du hot path inchangée** : en régime normal, le 1ᵉʳ essai
  réussit immédiatement (zéro sleep, même chemin code qu'avant).
- Logs tracing structurés `jamodio::capture` : `warn` à chaque essai
  qui échoue (`attempt`, `error`), `info` final si succès après retry
  (`attempts`).

#### Pourquoi générique (pas device-spécifique)

L'option « détecter Scarlett et appliquer retry uniquement pour cette
famille » a été écartée : fragile (nouveau hardware similaire = bug
réintroduit), tandis qu'un retry court borné ne pénalise les autres
devices que de quelques µs en régime normal.

### Notes

- `cargo test --workspace` : 47 verts (43 + 4 nouveaux tests retry :
  succès direct, succès après 2 échecs, épuisement des essais,
  fail-fast non-retryable). Tests purs CPU, pas d'I/O — exécution < 1 ms.
- `cargo build --release` Mac OK, zéro warning.
- Aucun changement de protocole WS, 100 % compat browser v0.4.1+.
- Build matrix CI inchangée (Mac ARM + Windows x64 via tag `agent-v*`).
- Validation runtime : reproduit on-device au prochain auto-update agent
  qui touche le binaire (Scarlett ou autre device USB lent).
- Hors périmètre : modification du 4ᵉ argument `None` de
  `build_input_stream` (= timeout du callback CPAL pendant la run,
  pas de l'init — ne joue pas dans le bug).

## [0.4.15] — 2026-05-28

### Fixed — Voyant CLIP : faux positifs sur les transitoires + message générique

Retour test v0.4.14 (BFD + Piano) : le voyant CLIP **restait allumé en
permanence alors que le son était parfait**. Analyse des logs : la sortie pique
bien au-dessus de 0 dBFS (peak p90 = 1.29, max 9.56) — **mais sur des
transitoires** (attaques de batterie/piano), pas en continu. Ces pics sont
soft-clippés de façon **inaudible** (quelques samples par attaque). Le voyant
se basait sur le **pic instantané** (≥ 0.99) → il s'allumait sur chaque attaque.

Fix :
- Le voyant est désormais piloté par le **TAUX de saturation SOUTENUE**
  (`output_clip_pct` = % de samples qui dépassent réellement la pleine-échelle
  sur la seconde), pas le pic instantané. Un transitoire = taux ~0 % → pas de
  voyant. Un overdrive réel et soutenu (≥ 1 % des samples) → voyant. Fini les
  faux positifs sur batterie/piano.
- **Message générique** (l'utilisateur n'a pas forcément de plugin) : « baisse
  ton niveau (gain d'entrée de ta carte son, OU sortie de ton plugin) ».
- Soft-clip plus **transparent** : seuil 0,94 → **0,98** (-0,17 dBFS) → on ne
  shape QUE le tout haut du signal (vrais dépassements), plus le signal fort
  propre.

`output_peak` reste exposé (diagnostic). `output_clip_pct` ajouté à PerfStats +
agent.log. cargo test --workspace : 42 verts (nouveau test : transitoire vs
overdrive soutenu).

## [0.4.14] — 2026-05-28

### Fixed — Chantier C : anti-clip plugin-agnostic + buffer monitor adaptatif

Analyse de la session test v0.4.13 (Mac Mini M1, AmpliTube/Piano) **croisée
avec l'enregistrement audio** (STEM décodé + analysé) :
- **0 drop** de capture sur toute la session (Chantier A toujours bon).
- Le signal **enregistré est propre** (aucun clic) → les craquements entendus
  étaient dans le **monitoring uniquement**, pas dans l'audio livré.
- Mais **clipping généralisé** : pic +6 dBFS (peak 2.06), 112 s/322 clippées →
  AmpliTube sort trop chaud, écrêtage dur du DAC + de l'enregistrement.
- AmpliTube spike périodiquement à **8–22 ms** par bloc (intrinsèque : les 3
  threads sont bien promus au workgroup CoreAudio) → le buffer self-monitor de
  5 ms ne pouvait pas absorber → underruns → clics dans le casque.

#### Anti-clipping — soft-clip de sécurité (plugin-agnostic, ZÉRO latence)

`soft_clip_block` (process stage, post-plugin) : en dessous de −0,5 dBFS le
signal est **bit-identique** ; au-dessus, genou `tanh` qui plafonne en douceur
vers ±1,0 (au lieu d'un écrêtage dur qui craque). **Aucun lookahead → aucune
latence ajoutée.** Protège le DAC (monitoring), le réseau et l'enregistrement
quel que soit le plugin. Voyant **CLIP** sur la tranche self quand la sortie
dépasse 0 dBFS (via `PerfStats.output_peak`) → invite à baisser la sortie du
plugin (la vraie correction côté source).

#### Buffer self-monitor adaptatif (latence-first)

Mode « local » du `JitterBuffer` pour le self-monitor :
- **Concealment** : sur underrun (spike plugin), au lieu d'une coupure sèche
  (clic), fondu de sortie + fondu d'entrée à la reprise → le trou devient un
  bref creux lissé, **zéro clic**. Zéro latence ajoutée.
- **Adaptation bornée** : baseline **5 ms inchangée** (latence mini quand le
  plugin se comporte) ; sous spikes, la cible grandit mais est **plafonnée à
  15 ms** (latence de monitoring bornée) et **redescend vers 5 ms dès le
  calme**. Les streams réseau gardent leur comportement (cap 40 ms).
- Diagnostic : `PerfStats.monitorBufferMs` + `monitorUnderruns` (visible dans
  agent.log + bundle) → on voit la latence monitoring grimper/redescendre.

Priorité latence respectée : en régime normal, latence monitoring identique à
avant (~5 ms self + I/O). Elle ne monte (≤ 15 ms) que transitoirement quand un
plugin sature vraiment le CPU, et revient à la baseline.

### Notes

- cargo test --workspace : 41 verts (6 nouveaux : 3 soft-clip cross-platform,
  3 buffer local — concealment, fade-in reprise, cap d'adaptation).
- Côté navigateur (repo principal) : voyant CLIP sur la tranche self.
- Limite physique honnête : un plugin qui stalle le CPU 22 ms force soit ~22 ms
  de buffer monitor (latence), soit un bref creux. On plafonne à 15 ms +
  concealment → meilleur compromis sans clic. La vraie élimination passe par
  un plugin moins gourmand / un preset plus léger.

## [0.4.13] — 2026-05-28

### Fixed — Crossfade dry→wet à l'activation d'un plugin (fin du clic de swap)

Suite à la validation on-device de v0.4.12 (load/unload non-bloquant, **0
drop**), un petit **craquement** subsistait au moment précis où un plugin
s'active : le signal basculait instantanément du son SEC (dry) au son traité
(wet) à une frontière de bloc, sans fondu → discontinuité audible.

Fix (`pipeline.rs`, process stage) : à chaque bascule « pas de plugin » →
« plugin actif » (load terminé, un-bypass, reprise après un swap), on applique
un **fondu équal-power ~8 ms** (`sin`/`cos`, `g_dry² + g_wet² = 1` → loudness
perçue constante) entre le signal sec sauvegardé et la sortie wet. Helper pur
`apply_dry_wet_fade` (testé). Couvre aussi le clic du toggle bypass A/B.

- Coût **nul en régime établi** : un seul test booléen par bloc (`fade_remaining
  == 0`), aucune copie, aucune latence ajoutée. La copie du signal sec
  (pré-allouée, réutilisée) n'a lieu que pendant les ~3 blocs du fondu.
- Plugin-agnostic. cargo test --workspace : 35 verts (3 nouveaux tests fondu :
  dry→wet, invariant équal-power, fondu étalé sur plusieurs blocs).

> Côté navigateur (hors agent, repo principal) : l'affichage de latence sur la
> tranche est désormais lissé (médiane glissante des pings) pour ne plus sauter
> sur un pic de mesure isolé, + latence de monitoring dans le tooltip.

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
  cohérent avec le pattern observé en baseline 22/05).
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
  --compare <baseline.json> <session-v0.4.2.txt>` (outillage interne)
  doit exit 0.

## [0.4.1] — 2026-05-23

### Added — Sprint S1 stabilité : instrumentation profonde

Premier sprint du chantier "Fondations stabilité agent". Pas de changement
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
  la baseline interne v0.4.1.

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
au lieu d'être rejetés. Et certains plugins (FIN-NEO d'UJAM observé en beta) ont des destructeurs C++ buggés qui throw une exception
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

### Entitlements AU host — vraie cause du bug AU (v0.2.23 ne suffisait pas)

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
signé). Validation manuelle au déploiement v0.2.24.

## [0.2.23] — 2026-05-13

### Sprint robustesse plugin AU — fix bug plugin AU (BFD + AmpliTube `-1`)

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
browser. Invisible à 2 peers FR-FR fibre (test interne 2 musiciens 22 ms reste
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

Des testeurs rapportaient s'entendre « légèrement décalé » dans leur
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
