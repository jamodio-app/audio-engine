//! Promotion de l'icône tray sur Windows 11 — `IsPromoted = 1`.
//!
//! # Problème (constaté par Ben le 11/06/2026 sur Windows 11)
//!
//! Windows 11 masque par défaut TOUTE nouvelle icône de zone de
//! notification dans l'overflow « ^ ». L'agent n'a pas de fenêtre dans la
//! barre des tâches (`skipTaskbar`) → sans icône tray visible, l'utilisateur
//! ne peut plus ni rouvrir la fenêtre ni quitter (hors Gestionnaire des
//! tâches). Il n'existe PAS d'API publique pour ça, mais depuis Win11 22H2
//! chaque icône a une entrée registre :
//!
//! `HKCU\Control Panel\NotifyIconSettings\<id>` avec :
//! - `ExecutablePath` (REG_SZ) — chemin de l'exe propriétaire
//! - `IsPromoted` (REG_DWORD) — 1 = visible dans la barre, 0 = overflow
//!
//! # Politique (respect du choix utilisateur) — v2
//!
//! Constat test Ben 11/06/2026 : Explorer crée l'entrée NotifyIconSettings
//! avec `IsPromoted = 0` D'OFFICE → lire la valeur ne permet PAS de
//! distinguer « défaut Explorer » d'un « choix utilisateur ». La v1 (qui ne
//! promouvait que si la valeur était absente) ne promouvait donc jamais.
//!
//! v2 : un marqueur à nous (`HKCU\Software\Jamodio\AudioEngine\
//! TrayPromotedOnce`) fait foi :
//! - marqueur ABSENT  = première promotion jamais faite → on force
//!   `IsPromoted = 1` (même si Explorer l'a mis à 0) puis on pose le
//!   marqueur. C'est le seul et unique moment où on touche la visibilité.
//! - marqueur PRÉSENT = on ne touche PLUS JAMAIS l'icône. Si l'utilisateur
//!   la masque ensuite via les Paramètres Windows, son choix est définitif
//!   (re-forcer une icône masquée volontairement = comportement de malware).
//!
//! L'entrée est créée par Explorer de façon asynchrone après la création
//! du tray → on poll avec quelques retries espacés, sur un thread détaché
//! (zéro impact démarrage). Win10 (pas de NotifyIconSettings) → no-op
//! loggé ; le filet reste le bouton « Quitter » de la fenêtre agent.

#![cfg(target_os = "windows")]

use std::time::Duration;

/// Tente la promotion en arrière-plan (fire-and-forget).
pub fn spawn_promotion() {
    std::thread::Builder::new()
        .name("tray-promote".into())
        .spawn(promote_with_retries)
        .ok();
}

fn promote_with_retries() {
    const RETRY_DELAYS: &[Duration] = &[
        Duration::from_secs(2),
        Duration::from_secs(3),
        Duration::from_secs(5),
    ];
    for delay in RETRY_DELAYS {
        std::thread::sleep(*delay);
        match try_promote() {
            Ok(Outcome::Promoted) => {
                tracing::info!(target: "jamodio::tray", "icône tray promue (IsPromoted=1)");
                return;
            }
            Ok(Outcome::AlreadyDecided) => {
                tracing::debug!(
                    target: "jamodio::tray",
                    "IsPromoted déjà défini (choix antérieur, utilisateur ou nous) — non modifié"
                );
                return;
            }
            Ok(Outcome::EntryNotFoundYet) => {
                // Explorer n'a pas encore créé l'entrée — retry.
            }
            Err(e) => {
                tracing::info!(
                    target: "jamodio::tray",
                    error = %e,
                    "NotifyIconSettings inaccessible (Win10 ?) — pas de promotion, le bouton Quitter de la fenêtre agent reste le filet"
                );
                return;
            }
        }
    }
    tracing::info!(
        target: "jamodio::tray",
        "entrée NotifyIconSettings introuvable pour notre exe après retries — pas de promotion"
    );
}

enum Outcome {
    Promoted,
    AlreadyDecided,
    EntryNotFoundYet,
}

/// Marqueur "promotion déjà faite" — survit aux updates ET aux
/// désinstall/réinstall (HKCU n'est pas nettoyé par le MSI) : on ne
/// re-force jamais une icône chez un utilisateur qui a déjà eu sa
/// promotion une fois.
const MARKER_KEY: &str = r"Software\Jamodio\AudioEngine";
const MARKER_VALUE: &str = "TrayPromotedOnce";

fn try_promote() -> Result<Outcome, std::io::Error> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let exe = std::env::current_exe()?;
    let exe_lower = exe.to_string_lossy().to_lowercase();
    let exe_file = exe
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // DIAGNOSTIC (v0.4.32) : les 2 tentatives registre précédentes ont échoué
    // pour une cause invisible côté Mac. On énumère et LOGGE systématiquement
    // tout ce qu'Explorer a réellement écrit, pour diagnostiquer sur faits
    // (mismatch de chemin ? entrée jamais créée ? valeur qui ne prend pas ?).
    let root = hkcu.open_subkey_with_flags(r"Control Panel\NotifyIconSettings", KEY_READ)?;

    let mut exact_match: Option<(String, Option<u32>)> = None;
    let mut file_match: Option<(String, Option<u32>)> = None;
    let mut seen = 0usize;
    for name in root.enum_keys().flatten() {
        let Ok(sub) = root.open_subkey_with_flags(&name, KEY_READ | KEY_SET_VALUE) else {
            continue;
        };
        let Ok(path) = sub.get_value::<String, _>("ExecutablePath") else {
            continue;
        };
        seen += 1;
        let promoted: Option<u32> = sub.get_value("IsPromoted").ok();
        let p_lower = path.to_lowercase();
        // On NE logge PAS chaque entrée (le file layer tourne en debug → un
        // bug report partagé listerait tous les logiciels tray de l'user). Le
        // bilan ci-dessous + le log de résultat suffisent au diagnostic.
        if p_lower == exe_lower {
            exact_match = Some((name.clone(), promoted));
        } else if !exe_file.is_empty() && p_lower.ends_with(&exe_file) {
            // Même exe, chemin stocké différemment (8.3, \\?\, casse d'un
            // composant…) — c'est exactement le genre de mismatch qui ferait
            // échouer un match strict. On le retient comme fallback.
            file_match = Some((name.clone(), promoted));
        }
    }
    tracing::info!(
        target: "jamodio::tray",
        our_exe = %exe.to_string_lossy(),
        entries_seen = seen,
        exact = exact_match.is_some(),
        by_filename = file_match.is_some(),
        "tray promote: bilan énumération"
    );

    let Some((key_name, previous)) = exact_match.or(file_match) else {
        return Ok(Outcome::EntryNotFoundYet);
    };

    // Marqueur : on respecte un choix utilisateur DÉJÀ exprimé — mais
    // seulement si la promotion a réellement pris une fois (IsPromoted lu à 1
    // au moment où on a gravé le marqueur). Si une tentative précédente a posé
    // le marqueur sans que l'icône soit devenue visible (le bug qu'on
    // diagnostique), on NE reste PAS bloqué : on re-tente tant que la valeur
    // effective n'est pas 1.
    let (marker, _) = hkcu.create_subkey(MARKER_KEY)?;
    let marker_set = marker.get_raw_value(MARKER_VALUE).is_ok();
    if marker_set && previous == Some(1) {
        return Ok(Outcome::AlreadyDecided);
    }

    let sub = root.open_subkey_with_flags(&key_name, KEY_READ | KEY_SET_VALUE)?;
    sub.set_value("IsPromoted", &1u32)?;
    let readback: Option<u32> = sub.get_value("IsPromoted").ok();
    if readback == Some(1) {
        marker.set_value(MARKER_VALUE, &1u32)?;
        tracing::info!(
            target: "jamodio::tray",
            key = %key_name, previous = ?previous,
            "IsPromoted=1 écrit ET relu OK — icône promue"
        );
        Ok(Outcome::Promoted)
    } else {
        // Écriture refusée / écrasée immédiatement : on ne grave PAS le
        // marqueur → on re-tentera au prochain lancement.
        tracing::warn!(
            target: "jamodio::tray",
            key = %key_name, readback = ?readback,
            "IsPromoted écrit mais relu ≠ 1 — la valeur ne prend pas (à diagnostiquer)"
        );
        Ok(Outcome::EntryNotFoundYet)
    }
}
