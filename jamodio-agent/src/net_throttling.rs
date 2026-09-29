//! Freinage réseau de Windows (`NetworkThrottlingIndex`) — Lot W3 de
//! `PLAN-FREINAGE-RESEAU-WINDOWS-2026-09.md` (dépôt du site).
//!
//! # Pourquoi
//!
//! Tant qu'un fil « multimédia » (MMCSS) tourne — les nôtres, ceux du pilote
//! ASIO, ceux du navigateur qui joue du son —, Windows retient une partie des
//! paquets reçus et les livre en rafale 7 à 22 ms plus tard. Chaque flux reçu
//! fait alors un trou, et chaque tampon monte pour s'en protéger. Mesuré au banc
//! (NUC, 9 musiciens, relais Ethernet, 28-29/09/2026) :
//!
//! | `NetworkThrottlingIndex`                      | trous/min | tampon |
//! |-----------------------------------------------|-----------|--------|
//! | 10 (défaut de Windows)                        | 15 à 18   | 11-16 ms |
//! | 70 (maximum documenté)                        | 19        | 17 ms  |
//! | `ffffffff` (désactivé), après redémarrage     | **0**     | **5 ms** |
//! | `ffffffff` écrit SANS redémarrer              | 16        | 14 ms  |
//!
//! Seule la désactivation agit, et seulement après un redémarrage. Se passer de
//! MMCSS pour nos fils ne suffit pas : le navigateur ou le pilote le déclenchent
//! sans nous. Un téléchargement à pleine vitesse pendant une session ne dégrade
//! pas l'audio, freinage désactivé ou non (essais i/j).
//!
//! # Ce que fait ce module
//!
//! - [`installer_step`] : mode `--network-throttling install|uninstall`, lancé
//!   par l'installeur en compte système (`wix/network-throttling.wxs`).
//!   L'installation mémorise la valeur d'origine du poste (une seule fois : une
//!   mise à jour ne l'écrase jamais), puis désactive le freinage. La
//!   désinstallation remet l'origine — sauf si la valeur a changé depuis (un
//!   réglage fait par le musicien ou son informaticien n'est jamais écrasé).
//! - [`log_state`] : l'état au journal, au lancement et à chaque capture, avec
//!   l'issue de la dernière étape d'installation. Rien n'est montré au
//!   musicien : nous corrigeons, il n'a rien à faire.
//!
//! Rien ici ne touche au chemin du son. macOS n'est pas concerné.

// Hors Windows, les décisions ne servent qu'aux tests (vérifiées sur tous les OS).
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// Valeur qui désactive le freinage.
pub const DISABLED: u32 = u32::MAX;

/// La valeur lue dans le registre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    Value(u32),
    /// Pas de valeur : Windows applique son défaut (freinage actif).
    Absent,
}

impl Reading {
    /// Forme mémorisée dans notre clé (texte : lisible au support).
    pub fn to_memory(self) -> String {
        match self {
            Reading::Value(v) => v.to_string(),
            Reading::Absent => "absent".into(),
        }
    }

    /// Relit la forme mémorisée. Un texte illisible n'est JAMAIS interprété :
    /// l'appelant refuse d'agir.
    pub fn from_memory(s: &str) -> Option<Reading> {
        match s.trim() {
            "absent" => Some(Reading::Absent),
            other => other.parse().ok().map(Reading::Value),
        }
    }

    /// Mise en mots, pour le journal.
    pub fn describe(self) -> String {
        match self {
            Reading::Value(DISABLED) => "désactivé (ffffffff)".into(),
            Reading::Value(v) => format!("ACTIF (valeur {v})"),
            Reading::Absent => "ACTIF (valeur absente = défaut de Windows)".into(),
        }
    }
}

/// Ce que l'installation doit faire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallPlan {
    /// Valeur d'origine à mémoriser ; `None` = une origine l'est déjà (mise à
    /// jour, réinstallation) et ne doit pas être écrasée.
    pub remember: Option<Reading>,
    /// Écrire `ffffffff` (faux si c'est déjà la valeur).
    pub disable: bool,
}

/// Décision d'installation, sans effet de bord.
pub fn install_plan(current: Reading, remembered: Option<Reading>) -> InstallPlan {
    InstallPlan {
        remember: if remembered.is_none() { Some(current) } else { None },
        disable: current != Reading::Value(DISABLED),
    }
}

/// Ce que la désinstallation fait de la valeur du poste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// Remettre cette valeur.
    Value(u32),
    /// La valeur n'existait pas : la supprimer.
    DeleteValue,
    /// Rien à remettre : l'origine était déjà `ffffffff`.
    AlreadyOrigin,
    /// La valeur a changé depuis l'installation : ce n'est plus la nôtre.
    ChangedSince,
    /// Aucune origine mémorisée : on ne devine jamais.
    NothingRemembered,
}

/// Décision de désinstallation, sans effet de bord.
pub fn uninstall_plan(current: Reading, remembered: Option<Reading>) -> Restore {
    let Some(origin) = remembered else {
        return Restore::NothingRemembered;
    };
    if current != Reading::Value(DISABLED) {
        return Restore::ChangedSince;
    }
    match origin {
        Reading::Value(DISABLED) => Restore::AlreadyOrigin,
        Reading::Value(v) => Restore::Value(v),
        Reading::Absent => Restore::DeleteValue,
    }
}

/// La clé a-t-elle été écrite après le démarrage de Windows ? Le réglage n'agit
/// qu'après un redémarrage (essai g du 29/09) : écrite depuis, la valeur lue
/// peut ne pas être celle qui s'applique. Temps en centaines de ns (FILETIME).
pub fn written_since_boot(last_write: u64, now: u64, uptime_ms: u64) -> bool {
    let boot = now.saturating_sub(uptime_ms.saturating_mul(10_000));
    last_write > boot
}

/// Point d'entrée du mode `--network-throttling <étape>` : code de sortie du
/// processus (0 = fait, 1 = échec, 2 = étape inconnue). L'issue est aussi
/// écrite dans notre clé, et [`log_state`] la reporte au journal au lancement
/// suivant : un échec d'installation n'est jamais muet.
pub fn installer_step(step: Option<&str>) -> i32 {
    #[cfg(target_os = "windows")]
    {
        windows::installer_step(&windows::Keys::system(), step)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = step;
        2
    }
}

/// État du freinage au journal (lancement, début de chaque capture). Lecture
/// seule, sans droits administrateur, hors du chemin du son.
pub fn log_state(when: &'static str) {
    #[cfg(target_os = "windows")]
    windows::log_state(&windows::Keys::system(), when);
    #[cfg(not(target_os = "windows"))]
    let _ = when;
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{install_plan, uninstall_plan, written_since_boot, Reading, Restore, DISABLED};
    use std::io;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
    use winreg::{RegKey, HKEY};

    const VALUE: &str = "NetworkThrottlingIndex";
    const ORIGIN: &str = "NetworkThrottlingIndexOrigin";
    const LAST_STEP: &str = "NetworkThrottlingLastStep";

    /// Où lire et écrire. Les tests visent une clé de TEST, jamais celle du poste.
    pub struct Keys {
        pub root: HKEY,
        pub profile: String,
        pub memory: String,
    }

    impl Keys {
        pub fn system() -> Self {
            Self {
                root: HKEY_LOCAL_MACHINE,
                profile: r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile".into(),
                memory: r"SOFTWARE\Jamodio\AudioEngine".into(),
            }
        }

        fn root(&self) -> RegKey {
            RegKey::predef(self.root)
        }

        pub fn read_current(&self) -> io::Result<Reading> {
            let key = match self.root().open_subkey_with_flags(&self.profile, KEY_READ) {
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Reading::Absent),
                Err(e) => return Err(e),
            };
            match key.get_value::<u32, _>(VALUE) {
                Ok(v) => Ok(Reading::Value(v)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Reading::Absent),
                Err(e) => Err(e),
            }
        }

        /// `Ok(None)` = rien de mémorisé ; une mémoire illisible est une erreur.
        pub fn read_origin(&self) -> io::Result<Option<Reading>> {
            let Some(text) = self.read_memory_text(ORIGIN)? else {
                return Ok(None);
            };
            Reading::from_memory(&text).map(Some).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, format!("origine mémorisée illisible : « {text} »"))
            })
        }

        fn read_memory_text(&self, name: &str) -> io::Result<Option<String>> {
            let key = match self.root().open_subkey_with_flags(&self.memory, KEY_READ) {
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            match key.get_value::<String, _>(name) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn memory_key(&self) -> io::Result<RegKey> {
            self.root().create_subkey_with_flags(&self.memory, KEY_READ | KEY_SET_VALUE).map(|(k, _)| k)
        }

        fn profile_key(&self) -> io::Result<RegKey> {
            self.root().create_subkey_with_flags(&self.profile, KEY_READ | KEY_SET_VALUE).map(|(k, _)| k)
        }

        fn write_step(&self, text: &str) {
            // Au mieux : si même notre clé est inaccessible, le journal dira
            // « aucune étape d'installation connue » et l'état réel.
            if let Ok(k) = self.memory_key() {
                let _ = k.set_value(LAST_STEP, &text.to_string());
            }
        }

        /// Date d'écriture de la clé du réglage, en centaines de ns (FILETIME).
        fn profile_last_write(&self) -> io::Result<u64> {
            let key = self.root().open_subkey_with_flags(&self.profile, KEY_READ)?;
            let ft = key.query_info()?.last_write_time;
            Ok((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime))
        }
    }

    fn install(keys: &Keys) -> io::Result<String> {
        let current = keys.read_current()?;
        let plan = install_plan(current, keys.read_origin()?);
        if let Some(origin) = plan.remember {
            keys.memory_key()?.set_value(ORIGIN, &origin.to_memory())?;
        }
        if plan.disable {
            keys.profile_key()?.set_value(VALUE, &DISABLED)?;
        }
        let origin = keys.read_origin()?.map(Reading::describe).unwrap_or_else(|| "?".into());
        Ok(if plan.disable {
            format!("install : freinage désactivé, effectif au prochain redémarrage (origine : {origin})")
        } else {
            format!("install : déjà désactivé, rien écrit (origine : {origin})")
        })
    }

    fn uninstall(keys: &Keys) -> io::Result<String> {
        let current = keys.read_current()?;
        let restore = uninstall_plan(current, keys.read_origin()?);
        let text = match restore {
            Restore::Value(v) => {
                keys.profile_key()?.set_value(VALUE, &v)?;
                format!("uninstall : valeur d'origine {v} remise")
            }
            Restore::DeleteValue => {
                keys.profile_key()?.delete_value(VALUE)?;
                "uninstall : valeur retirée (absente à l'origine)".into()
            }
            Restore::AlreadyOrigin => "uninstall : déjà désactivé à l'origine, rien à remettre".into(),
            Restore::ChangedSince => format!("uninstall : laissé tel quel, modifié depuis l'installation ({})", current.describe()),
            Restore::NothingRemembered => "uninstall : aucune origine mémorisée, rien touché".into(),
        };
        // Plus rien de nous dans le registre du poste.
        if let Ok(k) = keys.root().open_subkey_with_flags(&keys.memory, KEY_READ | KEY_SET_VALUE) {
            for name in [ORIGIN, LAST_STEP] {
                match k.delete_value(name) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(text)
    }

    pub fn installer_step(keys: &Keys, step: Option<&str>) -> i32 {
        let outcome = match step {
            Some("install") => install(keys),
            Some("uninstall") => uninstall(keys),
            _ => return 2,
        };
        match outcome {
            Ok(text) => {
                if step == Some("install") {
                    keys.write_step(&text);
                }
                0
            }
            Err(e) => {
                keys.write_step(&format!("{} : ÉCHEC — {e}", step.unwrap_or("?")));
                1
            }
        }
    }

    /// Temps courant et durée depuis le démarrage, pour `written_since_boot`.
    fn now_and_uptime() -> (u64, u64) {
        use windows_sys::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount64};
        // SAFETY : écrit une structure de la pile ; aucune précondition.
        let ft = unsafe {
            let mut ft = std::mem::zeroed();
            GetSystemTimeAsFileTime(&mut ft);
            ft
        };
        // SAFETY : aucune précondition.
        let uptime = unsafe { GetTickCount64() };
        ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime), uptime)
    }

    pub fn log_state(keys: &Keys, when: &'static str) {
        let installer = match keys.read_memory_text(LAST_STEP) {
            Ok(Some(s)) => s,
            Ok(None) => "aucune étape d'installation connue".into(),
            Err(e) => format!("illisible ({e})"),
        };
        let origin = match keys.read_origin() {
            Ok(Some(r)) => r.to_memory(),
            Ok(None) => "non mémorisée".into(),
            Err(e) => format!("illisible ({e})"),
        };
        match keys.read_current() {
            Ok(current) => {
                let (now, uptime) = now_and_uptime();
                let since_boot = match keys.profile_last_write() {
                    Ok(t) => if written_since_boot(t, now, uptime) { "oui" } else { "non" },
                    Err(_) => "inconnu",
                };
                let state = current.describe();
                if current == Reading::Value(DISABLED) && since_boot == "non" {
                    tracing::info!(
                        target: "jamodio::net_throttling", when, state, origin, installer,
                        "freinage réseau de Windows : désactivé"
                    );
                } else {
                    // Actif, ou posé depuis le démarrage (pas encore en vigueur) :
                    // les trous de réception par rafale sont à attendre.
                    tracing::warn!(
                        target: "jamodio::net_throttling", when, state,
                        cle_modifiee_depuis_le_demarrage = since_boot, origin, installer,
                        "freinage réseau de Windows : peut-être en vigueur (trous de réception par rafale possibles)"
                    );
                }
            }
            Err(e) => tracing::warn!(
                target: "jamodio::net_throttling", when, error = %e, origin, installer,
                "freinage réseau de Windows : état illisible — non vérifié"
            ),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use winreg::enums::HKEY_CURRENT_USER;

        /// Clés de TEST sous HKCU, propres à chaque test, effacées avant et après.
        struct TestKeys(Keys);

        impl TestKeys {
            fn new(tag: &str) -> Self {
                let base = format!(r"Software\Jamodio\Tests\net_throttling\{tag}");
                let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&base);
                Self(Keys {
                    root: HKEY_CURRENT_USER,
                    profile: format!(r"{base}\SystemProfile"),
                    memory: format!(r"{base}\AudioEngine"),
                })
            }
            fn set(&self, r: Reading) {
                let k = self.0.profile_key().unwrap();
                match r {
                    Reading::Value(v) => k.set_value(VALUE, &v).unwrap(),
                    Reading::Absent => {
                        let _ = k.delete_value(VALUE);
                    }
                }
            }
        }

        impl Drop for TestKeys {
            fn drop(&mut self) {
                let base = self.0.memory.trim_end_matches(r"\AudioEngine").to_string();
                let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(base);
            }
        }

        fn cycle(tag: &str, start: Reading) -> TestKeys {
            let t = TestKeys::new(tag);
            t.set(start);
            assert_eq!(installer_step(&t.0, Some("install")), 0);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(DISABLED));
            assert_eq!(t.0.read_origin().unwrap(), Some(start));
            t
        }

        #[test]
        fn installer_puis_desinstaller_remet_l_origine() {
            let t = cycle("origine10", Reading::Value(10));
            assert!(t.0.read_memory_text(LAST_STEP).unwrap().unwrap().contains("désactivé"));
            assert_eq!(installer_step(&t.0, Some("uninstall")), 0);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(10));
            assert_eq!(t.0.read_origin().unwrap(), None, "mémoire effacée");
        }

        #[test]
        fn une_valeur_absente_redevient_absente() {
            let t = cycle("absente", Reading::Absent);
            assert_eq!(installer_step(&t.0, Some("uninstall")), 0);
            assert_eq!(t.0.read_current().unwrap(), Reading::Absent);
        }

        #[test]
        fn une_mise_a_jour_n_ecrase_pas_l_origine() {
            let t = cycle("maj", Reading::Value(10));
            assert_eq!(installer_step(&t.0, Some("install")), 0);
            assert_eq!(t.0.read_origin().unwrap(), Some(Reading::Value(10)));
        }

        #[test]
        fn un_changement_fait_depuis_n_est_jamais_ecrase() {
            let t = cycle("change", Reading::Value(10));
            t.set(Reading::Value(20));
            assert_eq!(installer_step(&t.0, Some("uninstall")), 0);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(20));
        }

        #[test]
        fn une_origine_deja_desactivee_reste_desactivee() {
            let t = cycle("deja", Reading::Value(DISABLED));
            assert!(t.0.read_memory_text(LAST_STEP).unwrap().unwrap().contains("déjà désactivé"));
            assert_eq!(installer_step(&t.0, Some("uninstall")), 0);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(DISABLED));
        }

        #[test]
        fn une_etape_inconnue_ne_touche_a_rien() {
            let t = TestKeys::new("inconnue");
            t.set(Reading::Value(10));
            assert_eq!(installer_step(&t.0, Some("bidule")), 2);
            assert_eq!(installer_step(&t.0, None), 2);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(10));
        }

        /// Lecture seule de la VRAIE clé du poste (CI Windows) : jamais d'erreur,
        /// sans droits administrateur.
        #[test]
        fn la_cle_du_poste_se_lit() {
            Keys::system().read_current().unwrap();
            log_state(&Keys::system(), "test");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_installation_memorise_une_seule_fois_et_desactive() {
        assert_eq!(
            install_plan(Reading::Value(10), None),
            InstallPlan { remember: Some(Reading::Value(10)), disable: true }
        );
        assert_eq!(
            install_plan(Reading::Absent, None),
            InstallPlan { remember: Some(Reading::Absent), disable: true }
        );
        // Mise à jour : l'origine mémorisée à la première installation reste.
        assert_eq!(
            install_plan(Reading::Value(DISABLED), Some(Reading::Value(10))),
            InstallPlan { remember: None, disable: false }
        );
        // Déjà désactivé avant nous : on le note comme origine, on n'écrit rien.
        assert_eq!(
            install_plan(Reading::Value(DISABLED), None),
            InstallPlan { remember: Some(Reading::Value(DISABLED)), disable: false }
        );
    }

    #[test]
    fn la_desinstallation_ne_remet_que_ce_qui_est_encore_a_nous() {
        let off = Reading::Value(DISABLED);
        assert_eq!(uninstall_plan(off, Some(Reading::Value(10))), Restore::Value(10));
        assert_eq!(uninstall_plan(off, Some(Reading::Absent)), Restore::DeleteValue);
        assert_eq!(uninstall_plan(off, Some(off)), Restore::AlreadyOrigin);
        assert_eq!(uninstall_plan(Reading::Value(20), Some(Reading::Value(10))), Restore::ChangedSince);
        assert_eq!(uninstall_plan(Reading::Absent, Some(Reading::Value(10))), Restore::ChangedSince);
        assert_eq!(uninstall_plan(off, None), Restore::NothingRemembered);
    }

    #[test]
    fn la_memoire_se_relit_telle_quelle_ou_pas_du_tout() {
        for r in [Reading::Value(10), Reading::Value(DISABLED), Reading::Absent] {
            assert_eq!(Reading::from_memory(&r.to_memory()), Some(r));
        }
        assert_eq!(Reading::from_memory("dix"), None);
        assert_eq!(Reading::from_memory(""), None);
    }

    #[test]
    fn ecrite_apres_le_demarrage_ou_avant() {
        // Démarré il y a 1 000 s (10^10 centaines de ns) ; maintenant = 10^12.
        let now = 1_000_000_000_000u64;
        let uptime_ms = 1_000_000u64;
        assert!(written_since_boot(now - 1, now, uptime_ms));
        assert!(!written_since_boot(now - 20_000_000_000, now, uptime_ms));
        // Valeurs absurdes : pas de débordement, le démarrage retombe à 0.
        assert!(written_since_boot(1, 0, u64::MAX));
    }

    #[test]
    fn les_mots_du_journal() {
        assert_eq!(Reading::Value(DISABLED).describe(), "désactivé (ffffffff)");
        assert_eq!(Reading::Value(10).describe(), "ACTIF (valeur 10)");
        assert!(Reading::Absent.describe().contains("défaut de Windows"));
    }
}
