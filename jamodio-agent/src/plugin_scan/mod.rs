//! Scan plugins out-of-process (0.5.9-2, PLAN-PLUGIN-SCAN-OOP-2026-07).
//!
//! # Pourquoi
//!
//! Le scan in-process instanciait chaque plugin tiers DANS le process agent :
//! une access violation native dans une DLL (cas Groove Agent SE, dossier
//! rapport support 23/07) tuait l'agent — sans panic Rust, donc sans
//! filet possible (`catch_unwind` n'attrape pas les crashs natifs, cf.
//! main_thread.rs). Standard DAW (auval, Cubase, JUCE) : le plugin s'exécute
//! dans un process jetable ; s'il meurt, on sait lequel, on le blockliste,
//! l'hôte ne bronche pas.
//!
//! # Architecture
//!
//! ```text
//! agent (coordinateur, coordinator.rs — Lot B)
//!   ├─ découverte in-process (données seules, aucun code plugin) :
//!   │    Windows : discovery des .vst3 (jamodio-vst3-host)
//!   │    macOS   : énumération du registre AudioComponent (jamodio-au-host)
//!   ├─ spawn `jamodio-agent --plugin-scan-worker` (MÊME binaire, un seul
//!   │    artefact signé — pas de sidecar)
//!   ├─ items sur stdin (1 par ligne), events NDJSON sur stdout
//!   └─ crash worker : dernier `begin` sans `end` = coupable → blocklist
//!      → respawn sur le reste de la liste
//!
//! worker (worker.rs — process enfant jetable)
//!   └─ pour chaque item : begin → probe RÉELLE du plugin (instanciation,
//!      y compris les tiers — c'est LUI qui crashe, pas l'agent) → plugin* → end
//! ```
//!
//! Le protocole (protocol.rs) n'est PAS versionné : worker et coordinateur
//! sont le même binaire, toujours spawnés ensemble — aucun skew possible.

pub mod cache;
pub mod coordinator;
pub mod discovery;
pub mod protocol;
pub mod session;
pub mod worker;

#[cfg(any(target_os = "windows", target_os = "macos"))]
use jamodio_audio_core::plugin_host::PluginInfo;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use session::BlockedItem;

#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::process::Command;

/// Construit une commande fraîche vers un worker de scan : le MÊME binaire,
/// mode `--plugin-scan-worker` (court-circuit dans main). Sur Windows, pas de
/// console (le worker est invisible).
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn worker_command() -> Command {
    let exe = std::env::current_exe().expect("current_exe pour worker de scan");
    let mut cmd = Command::new(exe);
    cmd.arg("--plugin-scan-worker");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Résultat d'un scan complet, prêt à publier au browser.
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug, Default)]
pub struct FullScan {
    pub plugins: Vec<PluginInfo>,
    pub blocked: Vec<BlockedItem>,
    /// Nombre d'items réellement passés au worker (0 = tout servi par le cache).
    pub scanned: usize,
}

/// Scan complet out-of-process avec cache persisté (PLAN §3.3-3.4).
/// Bloquant (thread `plugin-scan`).
///
/// Régime établi : la découverte matche le cache → `scanned == 0`, retour en
/// quelques ms. Sinon seuls les items nouveaux/modifiés partent au worker ;
/// les crashs sont blocklistés et le cache réécrit.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run_full_scan() -> FullScan {
    run_full_scan_impl(false)
}

/// Rescan FORCÉ (bouton « Rescanner les plugins ») : ignore le cache disque
/// (entries réutilisées + blocklist) → TOUT est re-scanné, y compris les AU
/// blocklistés à tort. C'est la voie de récupération pilotée par l'utilisateur :
/// un AU sans empreinte fichier (retenu à vie par la réconciliation normale)
/// retente ainsi sa chance sans qu'on ait à re-figer le scan à chaque lancement.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run_full_scan_forced() -> FullScan {
    run_full_scan_impl(true)
}

/// Cœur du scan. `force` = ignorer le cache disque comme prior (rescan total).
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn run_full_scan_impl(force: bool) -> FullScan {
    use std::collections::HashMap;

    let items = discovery::discover_items();
    let discovered: Vec<(String, Option<cache::FileFingerprint>)> = items
        .iter()
        .map(|i| (i.clone(), cache::fingerprint(i)))
        .collect();
    let fp_by_item: HashMap<String, Option<cache::FileFingerprint>> =
        discovered.iter().cloned().collect();

    // Rescan forcé : prior vide (scanner_abi = 0 ≠ SCANNER_ABI) → la
    // réconciliation bascule sur « tout rescanner, blocklist ignorée ».
    let prior = if force {
        cache::CacheFile::default()
    } else {
        cache::load()
    };
    let plan = cache::reconcile(&discovered, &prior);
    tracing::info!(
        target: "jamodio::plugin",
        discovered = items.len(),
        to_scan = plan.to_scan.len(),
        reused = plan.reused.len(),
        blocked_retained = plan.retained_blocked.len(),
        "scan: réconciliation cache terminée"
    );

    let scanned = plan.to_scan.len();
    let fresh = if plan.to_scan.is_empty() {
        coordinator::ScanOutcome::default()
    } else {
        coordinator::scan_items(plan.to_scan.clone(), &worker_command)
    };

    // Persiste le cache fusionné (réutilisés + frais, blocklist retenue + neuve).
    let cache_file = cache::build_cache_file(&fresh.plugins, &fresh.blocked, &plan, &fp_by_item);
    cache::save(&cache_file);

    let mut plugins = plan.reused;
    plugins.extend(fresh.plugins);
    let mut blocked = plan.retained_blocked;
    blocked.extend(fresh.blocked);

    FullScan { plugins, blocked, scanned }
}
