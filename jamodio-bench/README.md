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
cargo run --release -p jamodio-bench -- devices
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

- **« Retard max du banc »** : si le faux serveur envoie lui-même en retard,
  c'est lui qui fait la gigue. À regarder en premier.
- **Talkback envoyé** (`--send-voice`) : l'agent n'envoie de la voix que s'il
  entend quelqu'un. Parler, ou faire jouer un son dans le micro, pendant le banc ;
  sinon le critère 3 reste « sans objet ».
- **Mode réseau local** (faux serveur sur une autre machine) : pas encore. Le
  WebSocket de l'agent n'écoute que la machine locale ; il faudra un relais sur la
  seconde machine (étape suivante du plan).
- Test automatique associé, sans matériel :
  `cargo test -p jamodio-agent scale_tests` (garde-fou) et
  `cargo test -p jamodio-agent constat_hoquet -- --ignored --nocapture` (constat).
