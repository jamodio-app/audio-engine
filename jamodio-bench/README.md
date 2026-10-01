# session-bench — banc « N musiciens »

Outil de test interne (non livré avec l'Audio Engine). Il remplace le studio et
le serveur auprès de l'Audio Engine **installé** sur la machine, et simule de 1 à
8 autres musiciens (ou plus) pour mesurer ce que le poste tient quand le groupe
grandit. Plan et critères : `internal-docs/plans/PLAN-BANC-N-MUSICIENS-2026-09.md`
(dépôt du site).

## Avant de lancer

1. Audio Engine lancé, en pré-version **≥ 0.6.6-1** (la cause des trous n'est
   mesurée qu'à partir de là ; plus ancien, les colonnes restent vides).
2. **Fermer le studio dans le navigateur** : l'Audio Engine n'a qu'un
   propriétaire, le banc prend sa place.
3. La carte son branchée, comme pour jouer.
4. **Fermer les programmes de fond qui prennent la main sur la machine** :
   prise en main à distance (TeamViewer…), application Claude, navigateur. Le
   01/10/2026 sur le NUC, avec TeamViewer et Claude ouverts (même inactifs), le
   banc envoyait jusqu'à 1,8 ms en retard (7 secondes sur 30) ; fermés,
   « SUFFISANTE ». Lequel des deux suffisait à dégrader : non établi.
5. Lancer `selftest` : « SUFFISANTE » avant toute campagne, sinon chercher ce
   qui tourne encore.

## Lancer

```text
cargo run --release -p jamodio-bench -- selftest          # d'abord : précision du banc seul
cargo run --release -p jamodio-bench -- devices
cargo run --release -p jamodio-bench -- plugins
cargo run --release -p jamodio-bench -- run --input "…" --output "…" --plugin "AmpliTube 5"
cargo run --release -p jamodio-bench -- run
cargo run --release -p jamodio-bench -- run --profile ethernet --to 6 --step-secs 120
cargo run --release -p jamodio-bench -- run --profiles regular,wifi --peer-voice bursts --send-voice 2
cargo run --release -p jamodio-bench -- scenario > mon-test.json
cargo run --release -p jamodio-bench -- run --scenario mon-test.json
cargo run --release -p jamodio-bench -- scenarios                     # la bibliothèque
cargo run --release -p jamodio-bench -- run --named 9-reseaux-mixtes
cargo run --release -p jamodio-bench -- scenario 9-reseaux-mixtes > a-modifier.json
```

Par défaut : de 2 à 9 musiciens, 5 min par palier (dont 30 s d'installation non
comptées), flux parfaitement réguliers, en local — **tout trou y est de cause
locale**. `Ctrl-C` arrête proprement et écrit ce qui a été mesuré.

## Résultats

Dans `bench-results/<scénario>-<date>/` :

- `resume.md` — tableau par nombre de musiciens, table par musicien simulé
  (lien, dérive simulée / lue par l'Audio Engine, trous par cause, cible),
  événements (cible de chaque flux avant → après), et état des 7 critères :
  « ✔ TENU » / « ✖ NON TENU » / « ○ sans objet », avec la valeur — une forme
  ET un mot, jamais une couleur seule ;
- `peers.csv` — chaque flux reçu, chaque seconde (trous par cause, cible du
  tampon et ses parts, remplissage, masquages…) ;
- `machine.csv` — la machine (CPU, callbacks manquants) et le faux serveur (son
  propre retard d'envoi, ce qu'il reçoit de l'agent : instrument et talkback) ;
- `scenario.json` — le scénario exact, pour rejouer la campagne.

Joindre aussi le **journal de l'Audio Engine** (lignes `TROU`, `perfstats`).

## Émetteur distant (lot R1-bis) — le mode recommandé sur PC

Le banc doit être plus précis que ce qu'il mesure. Sur la machine mesurée, sa
précision dépend de ce qui y tourne (NUC, 01/10/2026 : jusqu'à 2 ms de retard,
17 à 186 secondes sur 330, ancien banc compris). Avec l'émetteur distant, une
SECONDE machine fabrique et envoie les flux simulés et reçoit ce que l'Audio
Engine envoie ; la machine mesurée ne fait plus que piloter l'Audio Engine et
ne traite que les paquets d'un vrai musicien. Le vrai câble réseau est dans
la boucle, dans les deux sens.

```text
(seconde machine, en Ethernet)  session-bench remote
(machine mesurée)               cargo run --release -p jamodio-bench -- run --named regulier-9 --remote IP-DE-LA-SECONDE:51901 --input "…" --output "…"
```

- La précision du banc affichée est celle de l'émetteur (mesurée là-bas).
- Un fil d'envoi par musicien : la salve qu'un lien relâche ne retarde pas
  les autres (avec un seul fil, jusqu'à 2,6 ms, NUC du 01/10/2026).
- Les flux partent avec l'horloge de l'émetteur : la dérive que lit l'Audio
  Engine contient l'écart entre les deux horloges (quelques ppm, le même pour
  tous les flux). Le résumé l'estime et le retire avant de comparer.
- Un trou « arrivée » vient du trajet émetteur → machine mesurée ; une coupure
  de l'instrument envoyé, du trajet inverse.
- Le binaire macOS est autonome (bibliothèques système seulement) : il se
  copie sur un Mac sans rien y installer. Autoriser `session-bench` dans le
  pare-feu des deux machines.

## Réseaux réalistes (lot R1, `PLAN-BANC-REALISTE-2026-10.md`)

Chaque musicien simulé a SON lien et SON horloge. Dans un scénario JSON, à
côté de `jitter` et `loss_pct` (inchangés) :

```json
{
  "name": "adsl",
  "origin": "hypothèse — à calibrer en R2",
  "jitter": { "model": "pareto", "tail_ms": 6, "shape": 2.0, "max_ms": 60 },
  "loss_pct": 0.05,
  "burst_loss": { "rate_pct": 0.2, "mean_packets": 3 },
  "spikes": { "every_mean_s": 60, "hold_ms": 40 },
  "reorder": { "pct": 0.1, "max_depth": 1 },
  "base_delay_ms": 12,
  "drift_ppm": -35,
  "changes": [ { "at_s": 150, "link": { "name": "route-30ms", "jitter": { "model": "none" }, "base_delay_ms": 30 } } ],
  "absences": [ { "at_s": 300, "for_s": 20 } ],
  "voice": null
}
```

- `jitter` : `none`, `exponential` (`mean_ms`), ou `pareto` — queue LOURDE,
  réglée par sa queue p95 − p10 (`tail_ms`, la mesure de l'agent), sa forme et
  un plafond.
- `burst_loss` : pertes en rafales (deux états : taux total, longueur moyenne).
- `spikes` : le lien retient tout pendant `hold_ms`, en moyenne toutes les
  `every_mean_s` secondes, puis relâche d'un coup.
- `reorder` : une part des paquets arrive après les 1 à `max_depth` suivants
  (l'agent les écarte « en retard », `net/seq.rs`).
- `base_delay_ms` : seul, il ne change rien (le tampon ne voit que les
  variations) ; son SAUT par un `changes` est un changement de route.
- `drift_ppm` : l'horloge du musicien (cadence ET horodatage), par rapport à
  celle du banc. La carte son de la machine mesurée ajoute sa propre dérive,
  la même pour tous les flux.
- `changes` / `absences` : en secondes depuis l'arrivée du musicien. Une
  absence retire ses flux ; au retour, nouveaux flux (comme un vrai musicien).
- Tout nouveau champ est neutre par défaut : un ancien scénario envoie
  exactement les mêmes paquets (test d'empreinte). Une faute de frappe dans un
  profil est refusée.
- Instrument et talkback d'un même musicien vivent le même lien (mêmes pics).

Préréglages (`--profile`) : `regular`, `ethernet`, `wifi` (mesurés) ; `fibre`
(fibre + poste en Wi-Fi 5 GHz), `adsl`, `wifi-charge`, `4g` — **non calibrés**,
leur `origin` le dit et le résumé le recopie, jusqu'au lot R2.

Bibliothèque (`scenarios`) — chaque effet d'abord seul, puis mêlés :
`regulier-9`, `derive-100ppm` (30 min), `pics-seuls`, `desordre-seul`,
`rafales-seules`, `evenements`, `un-wifi-charge-parmi-8`, `9-reseaux-mixtes`.

Critères ajoutés (validés le 01/10/2026) : **5** aucun trou de cause locale
(réception, décodage, consommation) sous réseau simulé ; **6** quand un lien
change, la cible des AUTRES flux ne bouge pas de plus de 1 ms (médianes des
120 s avant / de 30 s après jusqu'à la suite) ; **7** dérive seule : aucun
trou, cible stable (≤ 1 ms entre les 5 premières et les 5 dernières minutes).
La cible publiée par l'agent est entière (tronquée) : ±1 ms d'arrondi.

## À savoir

- **Précision du banc** : ses fils tournent en priorité temps réel (macOS) ou
  MMCSS « Pro Audio » (Windows) — en priorité normale, le premier essai (28/09)
  envoyait jusqu'à 26 ms en retard et créait lui-même les trous qu'il mesurait.
  `selftest` la mesure sans l'Audio Engine ; le résumé de chaque campagne la
  rappelle (« SUFFISANTE » si aucun envoi n'a eu plus de 1 ms de retard).
- **« Retard max du banc »** : si le faux serveur envoie lui-même en retard,
  c'est lui qui fait la gigue. À regarder en premier.
- **Plugin** (`--plugin`) : chargé DANS l'Audio Engine sur l'instrument, comme
  en session — c'est la charge réelle du musicien. Ne pas lancer le plugin en
  standalone : il prendrait lui-même l'interface ASIO.
- **Talkback envoyé** (`--send-voice`) : l'agent n'envoie de la voix que s'il
  entend quelqu'un. Parler, ou faire jouer un son dans le micro, pendant le banc ;
  sinon le critère 3 reste « sans objet ».
- **Mode réseau** (`--relay`) : en local, les paquets ne passent ni par la
  carte réseau ni par son pilote. Pour les y faire passer, lancer le RELAIS sur
  une seconde machine du même réseau, puis le banc avec `--relay` :
  ```text
  (seconde machine)   cargo run --release -p jamodio-bench -- relay
  (machine mesurée)   cargo run --release -p jamodio-bench -- run --relay IP-DU-RELAIS:51900 …
  ```
  Le relais ne lit ni ne modifie rien (chiffrement de bout en bout banc ↔
  agent) ; il mesure son propre délai, que le résumé rappelle (« Relais »).
  Il mesure aussi les **coupures déjà présentes en arrivant chez lui** (colonne
  « Coupures à l'arrivée au relais », et à l'écran pendant le banc) : tout ce
  qui y arrive vient de la machine mesurée, donc présentes → elles naissent à
  l'ALLER ; absentes alors que l'agent a des trous « arrivée » → au RETOUR.
  Autoriser `session-bench` dans le pare-feu des deux machines.
- Test automatique associé, sans matériel :
  `cargo test -p jamodio-agent scale_tests` (garde-fou) et
  `cargo test -p jamodio-agent constat_hoquet -- --ignored --nocapture` (constat).
