//! Freinage réseau de Windows (`NetworkThrottlingIndex`) — Lots W3 et W3-bis de
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
//! L'INSTALLATION est faite par l'installeur Windows lui-même
//! (`wix/network-throttling.wxs`, W3-bis) : il désactive le freinage et mémorise
//! la valeur d'origine du poste, sans lancer aucun programme. W3 confiait ce
//! travail à ce module, lancé par l'installeur : chez un testeur (30/09), il n'a
//! jamais exécuté son code — cause non établie, échec ignoré sans bruit.
//!
//! - [`installer_step`] : mode `--network-throttling uninstall`, lancé par
//!   l'installeur en compte système à la vraie désinstallation : remet
//!   l'origine — sauf si la valeur a changé depuis (un réglage fait par le
//!   musicien ou son informaticien n'est jamais écrasé).
//! - [`log_state`] : l'état au journal, au lancement et à chaque capture, avec
//!   la trace de l'installation — et l'anomalie « installé, mais réglage jamais
//!   posé » dite en toutes lettres. Rien n'est montré au musicien : nous
//!   corrigeons, il n'a rien à faire.
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
    /// l'appelant refuse d'agir. Deux écritures possibles :
    /// - par l'installeur (W3-bis) : `msi:absent`, ou `msi:` suivi de la valeur
    ///   telle qu'il la lit — `#10`, et `#-1` ou `#4294967295` pour ffffffff ;
    /// - par ce module (W3, versions 0.6.6-12 à 0.6.6-14) : `absent` ou `10`.
    pub fn from_memory(s: &str) -> Option<Reading> {
        let s = s.trim();
        if let Some(msi) = s.strip_prefix("msi:") {
            if msi == "absent" {
                return Some(Reading::Absent);
            }
            // DWORD lu par l'installeur : signé ou non selon la version de Windows.
            let n: i64 = msi.strip_prefix('#')?.parse().ok()?;
            return match n {
                0..=0xFFFF_FFFF => Some(Reading::Value(n as u32)),
                -0x8000_0000..=-1 => Some(Reading::Value(n as i32 as u32)),
                _ => None,
            };
        }
        match s {
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

/// Ce que le journal dit de l'installation : la trace laissée par
/// l'installeur, ou l'anomalie d'une installation sans trace.
/// `managed` = le marqueur `NetworkThrottlingManaged`, écrit par tout
/// installeur qui gère ce réglage (depuis la 0.6.6-12).
pub fn installer_text(managed: bool, step: Option<&str>) -> String {
    match (managed, step) {
        (_, Some(step)) => step.to_string(),
        (true, None) => "ANOMALIE : installé par l'installeur Jamodio, mais aucune trace \
                          de l'étape du freinage — réglage jamais posé par l'installation"
            .into(),
        (false, None) => "aucune installation connue de ce réglage".into(),
    }
}

/// Point d'entrée du mode `--network-throttling uninstall` : code de sortie du
/// processus (0 = fait, 1 = échec, 2 = étape inconnue). Un échec est écrit dans
/// notre clé (lu par [`log_state`] si l'Audio Engine est réinstallé).
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
    use super::{installer_text, uninstall_plan, written_since_boot, Reading, Restore, DISABLED};
    use std::io;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
    use winreg::{RegKey, HKEY};

    const VALUE: &str = "NetworkThrottlingIndex";
    const ORIGIN: &str = "NetworkThrottlingIndexOrigin";
    const LAST_STEP: &str = "NetworkThrottlingLastStep";
    const MARKER: &str = "NetworkThrottlingManaged";

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
            // l'état réel et l'absence de trace.
            if let Ok(k) = self.memory_key() {
                let _ = k.set_value(LAST_STEP, &text.to_string());
            }
        }

        /// Le marqueur posé par l'installeur (DWORD 1), absent = `false`.
        fn managed(&self) -> io::Result<bool> {
            let key = match self.root().open_subkey_with_flags(&self.memory, KEY_READ) {
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            };
            match key.get_value::<u32, _>(MARKER) {
                Ok(v) => Ok(v == 1),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }

        /// Date d'écriture de la clé du réglage, en centaines de ns (FILETIME).
        fn profile_last_write(&self) -> io::Result<u64> {
            let key = self.root().open_subkey_with_flags(&self.profile, KEY_READ)?;
            let ft = key.query_info()?.last_write_time;
            Ok((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime))
        }
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
        if step != Some("uninstall") {
            return 2;
        }
        match uninstall(keys) {
            Ok(_) => 0,
            Err(e) => {
                keys.write_step(&format!("uninstall : ÉCHEC — {e}"));
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
        let installer = match (keys.managed(), keys.read_memory_text(LAST_STEP)) {
            (Ok(managed), Ok(step)) => installer_text(managed, step.as_deref()),
            (Err(e), _) | (_, Err(e)) => format!("illisible ({e})"),
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

        /// L'état que laisse l'installeur (W3-bis) : marqueur, origine notée à sa
        /// façon, freinage désactivé.
        fn installed(tag: &str, origin: &str) -> TestKeys {
            let t = TestKeys::new(tag);
            t.set(Reading::Value(DISABLED));
            let k = t.0.memory_key().unwrap();
            k.set_value(MARKER, &1u32).unwrap();
            k.set_value(ORIGIN, &origin.to_string()).unwrap();
            k.set_value(LAST_STEP, &"install (installeur) : …".to_string()).unwrap();
            t
        }

        fn uninstall_ok(t: &TestKeys) {
            assert_eq!(installer_step(&t.0, Some("uninstall")), 0);
            assert_eq!(t.0.read_origin().unwrap(), None, "mémoire effacée");
            assert_eq!(t.0.read_memory_text(LAST_STEP).unwrap(), None, "trace effacée");
        }

        #[test]
        fn l_origine_notee_par_l_installeur_est_remise() {
            let t = installed("origine10", "msi:#10");
            uninstall_ok(&t);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(10));
        }

        #[test]
        fn une_origine_notee_par_la_version_precedente_est_remise() {
            // 0.6.6-12 à 0.6.6-14 : l'origine était écrite par ce module.
            let t = installed("ancienne", "10");
            uninstall_ok(&t);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(10));
        }

        #[test]
        fn une_valeur_absente_redevient_absente() {
            let t = installed("absente", "msi:absent");
            uninstall_ok(&t);
            assert_eq!(t.0.read_current().unwrap(), Reading::Absent);
        }

        #[test]
        fn une_origine_deja_desactivee_reste_desactivee() {
            for (tag, origin) in [("deja-signe", "msi:#-1"), ("deja-non-signe", "msi:#4294967295")] {
                let t = installed(tag, origin);
                uninstall_ok(&t);
                assert_eq!(t.0.read_current().unwrap(), Reading::Value(DISABLED), "{origin}");
            }
        }

        #[test]
        fn un_changement_fait_depuis_n_est_jamais_ecrase() {
            let t = installed("change", "msi:#10");
            t.set(Reading::Value(20));
            uninstall_ok(&t);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(20));
        }

        #[test]
        fn une_origine_illisible_ne_touche_a_rien_et_le_dit() {
            let t = installed("illisible", "msi:dix");
            assert_eq!(installer_step(&t.0, Some("uninstall")), 1);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(DISABLED));
            assert!(t.0.read_memory_text(LAST_STEP).unwrap().unwrap().contains("ÉCHEC"));
        }

        #[test]
        fn une_etape_inconnue_ne_touche_a_rien() {
            let t = installed("inconnue", "msi:#10");
            // L'installation n'est plus une étape de ce module (W3-bis).
            assert_eq!(installer_step(&t.0, Some("install")), 2);
            assert_eq!(installer_step(&t.0, Some("bidule")), 2);
            assert_eq!(installer_step(&t.0, None), 2);
            assert_eq!(t.0.read_current().unwrap(), Reading::Value(DISABLED));
            assert_eq!(t.0.read_origin().unwrap(), Some(Reading::Value(10)));
        }

        #[test]
        fn le_marqueur_se_lit() {
            let t = TestKeys::new("marqueur");
            assert!(!t.0.managed().unwrap());
            t.0.memory_key().unwrap().set_value(MARKER, &1u32).unwrap();
            assert!(t.0.managed().unwrap());
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

    /// Ce que l'installeur écrit (W3-bis) : `msi:` + la valeur telle qu'il la lit.
    #[test]
    fn la_memoire_ecrite_par_l_installeur_se_relit() {
        assert_eq!(Reading::from_memory("msi:#10"), Some(Reading::Value(10)));
        assert_eq!(Reading::from_memory("msi:#-1"), Some(Reading::Value(DISABLED)));
        assert_eq!(Reading::from_memory("msi:#4294967295"), Some(Reading::Value(DISABLED)));
        assert_eq!(Reading::from_memory("msi:absent"), Some(Reading::Absent));
        for bad in ["msi:", "msi:10", "msi:#", "msi:#dix", "msi:#4294967296", "msi:#-2147483649", "msi:#x0a"] {
            assert_eq!(Reading::from_memory(bad), None, "{bad}");
        }
    }

    #[test]
    fn le_journal_dit_l_installation_sans_trace() {
        assert_eq!(installer_text(true, Some("install (installeur) : …")), "install (installeur) : …");
        assert!(installer_text(true, None).starts_with("ANOMALIE"));
        assert_eq!(installer_text(false, None), "aucune installation connue de ce réglage");
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
