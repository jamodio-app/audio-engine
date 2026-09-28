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
```

Par défaut : de 2 à 9 musiciens, 5 min par palier (dont 30 s d'installation non
comptées), flux parfaitement réguliers, en local — **tout trou y est de cause
locale**. `Ctrl-C` arrête proprement et écrit ce qui a été mesuré.

## Résultats

Dans `bench-results/<scénario>-<date>/` :

- `resume.md` — tableau par nombre de musiciens et état des 4 critères
  (« TENU » / « NON TENU » / « sans objet », avec la valeur) ;
- `peers.csv` — chaque flux reçu, chaque seconde (trous par cause, cible du
  tampon et ses parts, remplissage, masquages…) ;
- `machine.csv` — la machine (CPU, callbacks manquants) et le faux serveur (son
  propre retard d'envoi, ce qu'il reçoit de l'agent : instrument et talkback) ;
- `scenario.json` — le scénario exact, pour rejouer la campagne.

Joindre aussi le **journal de l'Audio Engine** (lignes `TROU`, `perfstats`).

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
