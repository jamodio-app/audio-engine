//! Cache de scan persisté + blocklist (PLAN-PLUGIN-SCAN-OOP §3.4).
//!
//! But : un boot en régime établi ne rescanne RIEN (publie `Ready` en < 100 ms
//! au lieu de 20-27 s) et n'oublie pas les plugins condamnés d'une session à
//! l'autre — sans jamais bannir définitivement un plugin qui pourrait être
//! corrigé par une mise à jour.
//!
//! Invalidation à trois niveaux :
//! - `scannerAbi` : bump global quand ce qu'on extrait change (efface tout) ;
//! - empreinte fichier (mtime+size) pour les items à chemin (VST3) : une màj
//!   du `.vst3` force son rescan (y compris s'il était blocklisté) ;
//! - appartenance à la découverte : un item désinstallé sort du cache.
//!
//! Items AU (`au:…`, pas de fichier) : pas d'empreinte → réutilisés/retenus
//! tant qu'ils restent énumérés ou jusqu'à un bump d'ABI. Le crash au scan
//! d'un AU est le cas rare (le rapport terrain à l'origine du chantier est
//! Windows/VST3).

use std::path::{Path, PathBuf};

use jamodio_audio_core::plugin_host::PluginInfo;
use serde::{Deserialize, Serialize};

use super::protocol::AuItemPrefix;
use super::session::{BlockReason, BlockedItem};

/// Version du format ET de ce qu'on extrait. Bump = invalidation totale
/// (rescan complet, blocklist du cache ignorée — cf. `reconcile`).
/// v1 : scan out-of-process, probe AU tous fabricants (0.5.9-2).
/// v2 : probe AU sur main thread pompé (0.5.11-4). Indispensable : sans ce
///      bump, les AU licenciés blocklistés à tort en v1 (empreinte AU = None →
///      retenus À VIE) ne seraient jamais re-scannés, donc le correctif run
///      loop ne réparerait AUCUN utilisateur déjà touché.
const SCANNER_ABI: u32 = 2;

const CACHE_FILENAME: &str = "plugin-scan-cache-v1.json";

/// Empreinte de changement d'un item à chemin (VST3). `None` pour les items
/// sans fichier (AU) — cf. module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// mtime en secondes depuis l'epoch (résolution suffisante pour un màj).
    pub mtime: i64,
    pub size: u64,
}

/// Entrée de plugins sains pour un item scanné avec succès.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    item: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<FileFingerprint>,
    plugins: Vec<PluginInfo>,
}

/// Entrée de blocklist persistée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockedRecord {
    item: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<FileFingerprint>,
    reason: BlockReason,
}

/// Contenu sérialisé du fichier cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheFile {
    scanner_abi: u32,
    #[serde(default)]
    entries: Vec<CacheEntry>,
    #[serde(default)]
    blocked: Vec<BlockedRecord>,
}

/// Empreinte d'un item : `Some` pour un chemin existant, `None` pour un item
/// AU ou un fichier illisible (traité comme « à (re)scanner »).
pub fn fingerprint(item: &str) -> Option<FileFingerprint> {
    if item.starts_with(AuItemPrefix::VALUE) {
        return None;
    }
    let meta = std::fs::metadata(Path::new(item)).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(FileFingerprint {
        mtime,
        size: meta.len(),
    })
}

/// Décision de réconciliation entre la découverte et le cache existant.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Items à envoyer au worker (nouveaux ou empreinte changée).
    pub to_scan: Vec<String>,
    /// Plugins réutilisés depuis le cache (items inchangés) — pas de worker.
    pub reused: Vec<PluginInfo>,
    /// Blocklist retenue (items toujours découverts, empreinte inchangée).
    pub retained_blocked: Vec<BlockedItem>,
}

/// Cœur pur & testable : décide quoi rescanner. `discovered` = (item,
/// empreinte courante) dans l'ordre de découverte.
///
/// Règles par item découvert :
/// - présent en cache entries avec empreinte identique → réutilisé ;
/// - présent en blocklist avec empreinte identique → reste bloqué, PAS scanné ;
/// - sinon (nouveau, ou empreinte changée = màj) → à scanner.
///
/// Les items du cache non redécouverts (désinstallés) sont naturellement
/// abandonnés (on n'itère que sur `discovered`).
pub fn reconcile(discovered: &[(String, Option<FileFingerprint>)], cache: &CacheFile) -> Plan {
    let mut plan = Plan::default();
    if cache.scanner_abi != SCANNER_ABI {
        // ABI périmé : tout rescanner, ignorer entries/blocked du cache.
        plan.to_scan = discovered.iter().map(|(i, _)| i.clone()).collect();
        return plan;
    }

    for (item, fp) in discovered {
        if let Some(entry) = cache.entries.iter().find(|e| e.item == *item) {
            if fingerprints_match(entry.fingerprint, *fp) {
                plan.reused.extend(entry.plugins.iter().cloned());
                continue;
            }
        }
        if let Some(rec) = cache.blocked.iter().find(|b| b.item == *item) {
            if fingerprints_match(rec.fingerprint, *fp) {
                plan.retained_blocked.push(BlockedItem {
                    item: item.clone(),
                    reason: rec.reason,
                });
                continue;
            }
            // Empreinte changée = plugin mis à jour → on lui redonne sa chance.
        }
        plan.to_scan.push(item.clone());
    }
    plan
}

/// Deux empreintes « identiques » : soit les deux None (items AU — on se fie à
/// l'appartenance à la découverte), soit deux `Some` égaux. Un passage
/// None↔Some (item devenu/plus un fichier) compte comme changement.
fn fingerprints_match(a: Option<FileFingerprint>, b: Option<FileFingerprint>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Construit le `CacheFile` à écrire après un scan : entries = réutilisés
/// regroupés par item d'origine + fraîchement scannés ; blocked = retenus +
/// nouveaux. On refingerprinte à l'écriture pour capturer l'état au moment du
/// scan.
///
/// `discovered_fp` fournit l'empreinte par item (pour ré-associer les plugins
/// fraîchement scannés à leur fichier).
pub fn build_cache_file(
    fresh_plugins: &[PluginInfo],
    fresh_blocked: &[BlockedItem],
    plan: &Plan,
    discovered_fp: &std::collections::HashMap<String, Option<FileFingerprint>>,
) -> CacheFile {
    let mut entries: Vec<CacheEntry> = Vec::new();

    // Regroupe TOUS les plugins (réutilisés + frais) par item d'origine.
    let mut by_item: std::collections::HashMap<String, Vec<PluginInfo>> =
        std::collections::HashMap::new();
    for p in plan.reused.iter().chain(fresh_plugins.iter()) {
        by_item.entry(item_of(p)).or_default().push(p.clone());
    }
    for (item, plugins) in by_item {
        let fingerprint = discovered_fp.get(&item).copied().flatten();
        entries.push(CacheEntry {
            item,
            fingerprint,
            plugins,
        });
    }
    entries.sort_by(|a, b| a.item.cmp(&b.item)); // déterministe (diffs propres)

    let mut blocked: Vec<BlockedRecord> = plan
        .retained_blocked
        .iter()
        .chain(fresh_blocked.iter())
        .map(|b| BlockedRecord {
            item: b.item.clone(),
            fingerprint: discovered_fp.get(&b.item).copied().flatten(),
            reason: b.reason,
        })
        .collect();
    blocked.sort_by(|a, b| a.item.cmp(&b.item));
    blocked.dedup_by(|a, b| a.item == b.item);

    CacheFile {
        scanner_abi: SCANNER_ABI,
        entries,
        blocked,
    }
}

/// Item d'origine d'un plugin (clé de regroupement du cache) : le path pour
/// VST3, l'identité `au:…` pour AU — cohérent avec les items de découverte.
fn item_of(p: &PluginInfo) -> String {
    use jamodio_audio_core::plugin_host::PluginRef;
    match &p.plugin_ref {
        PluginRef::Vst3 { path, .. } => path.clone(),
        PluginRef::Au {
            au_type,
            subtype,
            manufacturer,
        } => format!("{}{au_type}/{subtype}/{manufacturer}", AuItemPrefix::VALUE),
    }
}

// ---------- I/O ----------

/// Dossier data agent (sibling de logs/). Créé si absent.
pub fn data_dir() -> PathBuf {
    let dir = base_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(target_os = "macos")]
fn base_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/Jamodio")
}

#[cfg(target_os = "windows")]
fn base_data_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| "C:\\Temp".into());
    PathBuf::from(appdata).join("Jamodio")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn base_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/jamodio")
}

fn cache_path() -> PathBuf {
    data_dir().join(CACHE_FILENAME)
}

/// Charge le cache. Absent/corrompu/ABI périmé → cache vide (tout sera
/// rescanné) : jamais d'erreur propagée, le scan doit toujours pouvoir tourner.
pub fn load() -> CacheFile {
    let path = cache_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return CacheFile::default();
    };
    match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "jamodio::plugin",
                error = %e,
                "cache de scan illisible — rescan complet"
            );
            CacheFile::default()
        }
    }
}

/// Écrit le cache de façon atomique (temp + rename) pour ne jamais laisser un
/// fichier tronqué si l'agent meurt pendant l'écriture.
pub fn save(cache: &CacheFile) {
    let path = cache_path();
    let tmp = path.with_extension("json.tmp");
    let json = match serde_json::to_vec_pretty(cache) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(target: "jamodio::plugin", error = %e, "sérialisation cache échouée");
            return;
        }
    };
    if let Err(e) = std::fs::write(&tmp, &json).and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!(target: "jamodio::plugin", error = %e, "écriture cache échouée");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::plugin_host::PluginRef;

    fn vst3(path: &str, name: &str) -> PluginInfo {
        PluginInfo {
            name: name.into(),
            manufacturer: "T".into(),
            plugin_ref: PluginRef::Vst3 { path: path.into(), uid: "00".into() },
            latency_samples: 0,
            has_editor: false,
            incompatible: false,
            has_input_bus: true,
            is_instrument: false,
        }
    }

    fn fp(mtime: i64, size: u64) -> Option<FileFingerprint> {
        Some(FileFingerprint { mtime, size })
    }

    fn cache_with(entries: Vec<CacheEntry>, blocked: Vec<BlockedRecord>) -> CacheFile {
        CacheFile { scanner_abi: SCANNER_ABI, entries, blocked }
    }

    #[test]
    fn unchanged_items_are_reused_not_rescanned() {
        let cache = cache_with(
            vec![CacheEntry {
                item: "/a.vst3".into(),
                fingerprint: fp(100, 10),
                plugins: vec![vst3("/a.vst3", "A")],
            }],
            vec![],
        );
        let discovered = vec![("/a.vst3".to_string(), fp(100, 10))];
        let plan = reconcile(&discovered, &cache);
        assert!(plan.to_scan.is_empty(), "item inchangé ne doit pas être rescanné");
        assert_eq!(plan.reused.len(), 1);
    }

    #[test]
    fn changed_fingerprint_forces_rescan() {
        let cache = cache_with(
            vec![CacheEntry {
                item: "/a.vst3".into(),
                fingerprint: fp(100, 10),
                plugins: vec![vst3("/a.vst3", "A")],
            }],
            vec![],
        );
        // même item, taille différente (= mise à jour du plugin).
        let discovered = vec![("/a.vst3".to_string(), fp(200, 99))];
        let plan = reconcile(&discovered, &cache);
        assert_eq!(plan.to_scan, vec!["/a.vst3".to_string()]);
        assert!(plan.reused.is_empty());
    }

    #[test]
    fn blocked_item_stays_blocked_but_retest_on_update() {
        let cache = cache_with(
            vec![],
            vec![BlockedRecord {
                item: "/bad.vst3".into(),
                fingerprint: fp(100, 10),
                reason: BlockReason::Crash,
            }],
        );
        // Empreinte identique → reste bloqué, pas de scan.
        let same = vec![("/bad.vst3".to_string(), fp(100, 10))];
        let plan = reconcile(&same, &cache);
        assert!(plan.to_scan.is_empty());
        assert_eq!(plan.retained_blocked.len(), 1);
        assert_eq!(plan.retained_blocked[0].reason, BlockReason::Crash);

        // Mise à jour du plugin → on lui redonne sa chance (rescan).
        let updated = vec![("/bad.vst3".to_string(), fp(300, 20))];
        let plan = reconcile(&updated, &cache);
        assert_eq!(plan.to_scan, vec!["/bad.vst3".to_string()]);
        assert!(plan.retained_blocked.is_empty());
    }

    #[test]
    fn new_item_is_scanned() {
        let cache = CacheFile { scanner_abi: SCANNER_ABI, ..Default::default() };
        let discovered = vec![("/new.vst3".to_string(), fp(1, 1))];
        let plan = reconcile(&discovered, &cache);
        assert_eq!(plan.to_scan, vec!["/new.vst3".to_string()]);
    }

    #[test]
    fn uninstalled_item_is_dropped() {
        let cache = cache_with(
            vec![CacheEntry {
                item: "/gone.vst3".into(),
                fingerprint: fp(1, 1),
                plugins: vec![vst3("/gone.vst3", "Gone")],
            }],
            vec![],
        );
        // Plus découvert → n'apparaît nulle part dans le plan.
        let plan = reconcile(&[], &cache);
        assert!(plan.to_scan.is_empty());
        assert!(plan.reused.is_empty());
        assert!(plan.retained_blocked.is_empty());
    }

    #[test]
    fn abi_bump_invalidates_everything() {
        let mut cache = cache_with(
            vec![CacheEntry {
                item: "/a.vst3".into(),
                fingerprint: fp(1, 1),
                plugins: vec![vst3("/a.vst3", "A")],
            }],
            vec![],
        );
        cache.scanner_abi = SCANNER_ABI + 1; // futur/périmé
        let discovered = vec![("/a.vst3".to_string(), fp(1, 1))];
        let plan = reconcile(&discovered, &cache);
        assert_eq!(plan.to_scan, vec!["/a.vst3".to_string()]);
        assert!(plan.reused.is_empty());
    }

    #[test]
    fn au_items_reused_without_fingerprint() {
        let cache = cache_with(
            vec![CacheEntry {
                item: "au:aufx/mrev/appl".into(),
                fingerprint: None,
                plugins: vec![PluginInfo {
                    plugin_ref: PluginRef::Au {
                        au_type: "aufx".into(),
                        subtype: "mrev".into(),
                        manufacturer: "appl".into(),
                    },
                    ..vst3("x", "MatrixReverb")
                }],
            }],
            vec![],
        );
        let discovered = vec![("au:aufx/mrev/appl".to_string(), None)];
        let plan = reconcile(&discovered, &cache);
        assert!(plan.to_scan.is_empty());
        assert_eq!(plan.reused.len(), 1);
    }

    #[test]
    fn build_cache_file_merges_reused_and_fresh() {
        let mut fps = std::collections::HashMap::new();
        fps.insert("/a.vst3".to_string(), fp(1, 1));
        fps.insert("/b.vst3".to_string(), fp(2, 2));

        let plan = Plan {
            to_scan: vec!["/b.vst3".into()],
            reused: vec![vst3("/a.vst3", "A")],
            retained_blocked: vec![],
        };
        let fresh = vec![vst3("/b.vst3", "B")];
        let out = build_cache_file(&fresh, &[], &plan, &fps);
        assert_eq!(out.scanner_abi, SCANNER_ABI);
        assert_eq!(out.entries.len(), 2);
        // Trié par item : /a puis /b.
        assert_eq!(out.entries[0].item, "/a.vst3");
        assert_eq!(out.entries[0].fingerprint, fp(1, 1));
        assert_eq!(out.entries[1].item, "/b.vst3");
    }

    #[test]
    fn cache_file_round_trips_json() {
        let mut fps = std::collections::HashMap::new();
        fps.insert("/a.vst3".to_string(), fp(1, 1));
        let plan = Plan { to_scan: vec![], reused: vec![vst3("/a.vst3", "A")], retained_blocked: vec![] };
        let cache = build_cache_file(&[], &[], &plan, &fps);
        let json = serde_json::to_vec(&cache).unwrap();
        let back: CacheFile = serde_json::from_slice(&json).unwrap();
        assert_eq!(cache.entries, back.entries);
        assert_eq!(cache.scanner_abi, back.scanner_abi);
    }
}
