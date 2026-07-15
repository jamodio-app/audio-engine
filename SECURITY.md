# Politique de sécurité — Jamodio Audio Engine

## Signaler une vulnérabilité

Si tu découvres une vulnérabilité de sécurité dans Jamodio Audio Engine
(agent desktop) ou dans n'importe quel composant publié sur ce repo,
**merci de la signaler en privé** plutôt que via une issue publique.

### Canal préféré — GitHub Private Vulnerability Reporting

Ouvre un rapport via l'onglet **Security** de ce repo :
👉 <https://github.com/jamodio-app/audio-engine/security/advisories/new>

Ce canal est privé entre toi et les mainteneurs. C'est la méthode
recommandée par GitHub pour la divulgation responsable.

### Canal email

Si tu préfères l'email :
**support@jamodio.com**

Inclure si possible :
- Une description du problème et de son impact potentiel
- Les étapes pour reproduire (PoC minimal apprécié)
- La version concernée (visible dans la fenêtre de l'agent ou via le
  banner du navigateur)
- Ton OS (macOS ou Windows) et le device audio utilisé si pertinent

## Engagements côté Jamodio

- **Accusé de réception sous 72 h ouvrées**.
- **Diagnostic et plan de correction sous 7 jours** pour les
  vulnérabilités exploitables à distance ou impliquant les flux
  audio/réseau.
- **Pas de poursuite légale** envers les chercheurs qui respectent
  cette politique (pas d'accès à des données utilisateur tiers, pas
  d'exfiltration, pas de dégradation de service en production).
- **Crédit** dans le CHANGELOG si tu le souhaites (anonyme par
  défaut).

## Hors scope

- Vulnérabilités sur des dépendances tierces déjà publiées (à signaler
  upstream — ex. : `tauri`, `cpal`, `audiopus`, `webrtc-rs`).
- Manque de hardening sur l'environnement local de l'utilisateur
  (ex. : "l'agent fonctionne si l'user a désactivé Gatekeeper").
- Rapports automatisés sans PoC (scanners SCA non vérifiés, etc.).
- Bugs fonctionnels non sécurité-critiques → ouvre une issue publique
  classique.

## Surface concernée

Ce repo héberge **uniquement l'agent desktop** (Rust + Tauri + AU
host) qui tourne en local sur la machine de l'user et expose :

- Un WebSocket local `127.0.0.1:9876` (commande/contrôle browser
  ↔ agent).
- Des sockets UDP RTP/SRTP vers le SFU Jamodio (audio temps réel).
- Le pont CoreAudio (capture micro/instrument, playback).
- Le hosting AudioUnit (chargement de plugins natifs macOS dans la
  mixette).

L'infrastructure serveur Jamodio (SFU, web app, base de données,
auth) **n'est pas hébergée dans ce repo** et ne fait pas partie du
scope de cette politique. Pour ces composants, le même contact
`support@jamodio.com` reste valide.

## Signature et chaîne d'update

Tous les binaires publiés sur les releases de ce repo sont signés
avec une clé minisign (`tauri-plugin-updater`). La clé publique est
embarquée dans l'agent (`tauri.conf.json > updater.pubkey`) et la
clé privée est protégée côté secrets GitHub Actions.

Si tu détectes un binaire ou un `latest.json` dont la signature ne
matche pas la clé publique, **ne l'installe pas** et signale-le
immédiatement via le canal Security.
