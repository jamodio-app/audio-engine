//! Interrupteurs de banc — un seul mécanisme, vérifiable dans le journal.
//!
//! Un banc ne vaut que si l'on sait, sans le supposer, quel réglage tournait.
//! Le 14/09, une comparaison de quatre sessions a été perdue parce qu'une
//! variable d'environnement posée dans une console n'était pas héritée par
//! l'Audio Engine relancé depuis le studio : les quatre sessions ont tourné
//! avec le même réglage sans que rien ne le dise.
//!
//! D'où ce fichier, lu au démarrage de chaque capture, à côté des journaux —
//! donc trouvable par la personne qui tient le banc —, et **toujours**
//! journalisé : la ligne dit ce qui est actif, ou qu'il n'y a rien.
//!
//! Format, une ligne par interrupteur, `#` pour un commentaire :
//! ```text
//! # aucune ligne = comportement normal
//! no-rtcp = 1
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Nom du fichier, dans le dossier des journaux (`logging::log_dir`).
pub const BENCH_FLAGS_FILE: &str = "bench-flags";

/// Les interrupteurs lus pour cette capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BenchFlags {
    /// Coupe la tâche RTCP (Sender Reports et lecture des rapports du SFU) pour
    /// comparer, avec le même binaire, une session avec et une sans.
    pub no_rtcp: bool,
}

impl BenchFlags {
    /// Lit le fichier s'il existe. Fichier absent = tout est normal ; fichier
    /// illisible = tout est normal, et on le dit (jamais un réglage deviné).
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::bench",
                    path = %path.display(),
                    error = %e,
                    "interrupteurs de banc illisibles — comportement normal"
                );
                Self::default()
            }
        }
    }

    /// Chemin du fichier : à côté des journaux, là où l'export support va déjà.
    pub fn path() -> PathBuf {
        crate::logging::log_dir().join(BENCH_FLAGS_FILE)
    }

    /// Analyse le contenu. Une clé inconnue ou une valeur autre que `1` laisse
    /// le comportement normal : un banc ne doit jamais dépendre d'une faute de
    /// frappe silencieuse, d'où la trace.
    pub fn parse(contents: &str) -> Self {
        let mut flags = Self::default();
        for (key, value) in Self::entries(contents) {
            let on = value == "1";
            match key.as_str() {
                "no-rtcp" => flags.no_rtcp = on,
                other => tracing::warn!(
                    target: "jamodio::bench",
                    flag = other,
                    "interrupteur de banc inconnu — ignoré"
                ),
            }
            if !on && value != "0" {
                tracing::warn!(
                    target: "jamodio::bench",
                    flag = key,
                    value = value,
                    "valeur d'interrupteur non reconnue — inactif"
                );
            }
        }
        flags
    }

    fn entries(contents: &str) -> BTreeMap<String, String> {
        contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect()
    }

    /// Ce qui est actif, pour le journal. Vide = rien n'est détourné.
    pub fn active(&self) -> Vec<&'static str> {
        let mut active = Vec::new();
        if self.no_rtcp {
            active.push("no-rtcp");
        }
        active
    }

    /// Trace obligatoire au démarrage d'une capture : sans elle, aucune mesure
    /// de banc n'est interprétable.
    pub fn log(&self) {
        let active = self.active();
        if active.is_empty() {
            tracing::info!(target: "jamodio::bench", "interrupteurs de banc : aucun (comportement normal)");
        } else {
            tracing::warn!(
                target: "jamodio::bench",
                flags = active.join(","),
                path = %Self::path().display(),
                "interrupteurs de banc ACTIFS — ce n'est pas le comportement normal"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_fichier_vide_ne_detourne_rien() {
        assert_eq!(BenchFlags::parse(""), BenchFlags::default());
        assert!(BenchFlags::parse("# rien\n\n").active().is_empty());
    }

    #[test]
    fn un_interrupteur_pose_est_lu_et_annonce() {
        let flags = BenchFlags::parse("no-rtcp = 1\n");
        assert!(flags.no_rtcp);
        assert_eq!(flags.active(), vec!["no-rtcp"]);
    }

    #[test]
    fn zero_espaces_et_casse_sont_tolerés() {
        assert!(BenchFlags::parse("  NO-RTCP=1  ").no_rtcp);
        assert!(!BenchFlags::parse("no-rtcp = 0").no_rtcp);
    }

    /// Une faute de frappe ne doit jamais activer un réglage ni en cacher un.
    #[test]
    fn une_valeur_ou_une_cle_inconnue_laisse_le_comportement_normal() {
        assert!(!BenchFlags::parse("no-rtcp = oui").no_rtcp);
        assert_eq!(BenchFlags::parse("no-rtcpp = 1"), BenchFlags::default());
    }
}
