//! Sprint S2 (PLAN-EXECUTION-AGENT-STABILITE.md §S2.2) — wrapper Rust
//! safe pour le binding CoreAudio Workgroup (cf. `cpp/audio_workgroup.mm`).
//!
//! Permet à l'`encoder_thread` côté `jamodio-agent` de joindre le workgroup
//! audio HAL du device output. Sans ça, le thread tourne en SCHED_OTHER
//! (= priorité normale ignorée par Darwin) → préemptible par tout autre
//! process → spikes 10-25 ms observés en baseline v0.4.1.
//!
//! ## Contrat thread
//!
//! `AudioWorkgroup::join` doit être appelé **depuis le thread qui veut
//! bénéficier du scheduling audio**. Le `Drop` libère la jointure depuis
//! le même thread (le wrapper est `Send` pour passer entre constructeur
//! et thread RT, mais `!Sync` car le token de leave est lié au thread
//! qui a fait le join).
//!
//! En pratique :
//! ```ignore
//! std::thread::spawn(move || {
//!     let _wg = workgroup::AudioWorkgroup::join_default(); // joint sur ce thread
//!     // ... boucle RT audio ...
//!     // _wg drop ⇒ leave sur ce thread
//! });
//! ```

use std::ffi::{c_char, c_void, CString};

extern "C" {
    fn jamodio_audio_workgroup_available() -> bool;
    fn jamodio_audio_workgroup_join(device_name_substr: *const c_char) -> *mut c_void;
    fn jamodio_audio_workgroup_leave(handle: *mut c_void);
}

/// Vrai si `os_workgroup_join` est disponible (macOS ≥ 11). Détection à
/// l'exécution via `__builtin_available` côté C. Sur OS plus ancien, le
/// caller doit utiliser un fallback (QoS, cf. `rt_priority.rs` côté agent).
pub fn is_available() -> bool {
    // SAFETY : aucun pointeur, aucun side-effect mémoire — appel C trivial.
    unsafe { jamodio_audio_workgroup_available() }
}

/// Jointure active dans un workgroup audio CoreAudio. RAII : le `Drop`
/// libère automatiquement depuis le thread qui a fait `join`.
///
/// `!Sync` (token de leave thread-local) mais `Send` autorisé pour passer
/// le handle vers le thread RT au spawn (le `join` se fait dans le thread
/// cible, pas dans le thread d'origine).
pub struct AudioWorkgroup {
    handle: *mut c_void,
}

// SAFETY : le handle est un pointeur opaque ObjC++ qui ne dépend pas
// d'état thread-local côté Rust. Le token de join est dedans, mais c'est
// au thread qui fait le `Drop` de garantir qu'il est le même que celui
// qui a fait le `join` (contrat documenté). On autorise `Send` pour pouvoir
// retourner `Option<AudioWorkgroup>` depuis le constructeur ; on n'autorise
// PAS `Sync` (jamais deux threads en même temps sur le même handle).
unsafe impl Send for AudioWorkgroup {}

impl AudioWorkgroup {
    /// Tente de joindre le workgroup du device output **default système**.
    /// Retourne `None` si :
    /// - macOS < 11 (pas d'API workgroup),
    /// - aucun default output trouvé,
    /// - le device n'expose pas de workgroup (BlackHole, Aggregate, etc.),
    /// - `os_workgroup_join` a échoué (thread déjà dans un autre workgroup).
    ///
    /// **À appeler depuis le thread qui veut être scheduled audio**.
    pub fn join_default() -> Option<Self> {
        Self::join_internal(std::ptr::null())
    }

    /// Tente de joindre le workgroup d'un device dont le nom contient
    /// `name_substr` (case-insensitive, fallback sur default output si match
    /// nominal échoue). À privilégier quand on connaît le device de
    /// playback (ex : "Scarlett Solo 4th Gen").
    pub fn join_by_name(name_substr: &str) -> Option<Self> {
        let c = CString::new(name_substr).ok()?;
        Self::join_internal(c.as_ptr())
    }

    fn join_internal(name_ptr: *const c_char) -> Option<Self> {
        // SAFETY : `name_ptr` est NULL ou une chaîne C valide pour la durée
        // de l'appel (référence empruntée à un CString qui vit jusqu'à la fin
        // de la fonction). La FFI ne retient pas la chaîne après retour.
        let handle = unsafe { jamodio_audio_workgroup_join(name_ptr) };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle })
        }
    }
}

impl Drop for AudioWorkgroup {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY : `self.handle` est un handle valide créé par
            // `jamodio_audio_workgroup_join`. La FFI s'occupe de
            // `os_workgroup_leave` + `os_release` + `free`. Doit être appelé
            // depuis le thread qui a fait le `join` (contrat de la struct).
            unsafe { jamodio_audio_workgroup_leave(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_returns_bool() {
        // Sur runner CI macOS récent : true. Sur macOS 10.x : false.
        // On vérifie juste que l'appel ne panique pas et renvoie un bool valide.
        let _ = is_available();
    }

    #[test]
    fn join_default_returns_option() {
        // Sur runner CI sans audio device configuré : peut renvoyer None.
        // Sur poste de dev : devrait renvoyer Some(_).
        // On valide juste que l'API ne panique pas et est sound.
        let wg = AudioWorkgroup::join_default();
        if wg.is_some() {
            // Drop = leave. Si on arrive ici sans crash, le cycle complet est OK.
            drop(wg);
        }
    }

    #[test]
    fn join_nonexistent_device_returns_none_or_falls_back() {
        // Avec un nom qui ne matche aucun device, l'impl C fait fallback sur
        // le default output. Soit on retourne Some (fallback OK), soit None
        // (aucun default output, p.ex. CI headless) — jamais de panique.
        let wg = AudioWorkgroup::join_by_name("\x01definitely-not-a-real-device\x01");
        drop(wg);
    }
}
