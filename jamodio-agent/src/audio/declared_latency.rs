//! Latence matérielle DÉCLARÉE par le pilote pour le périphérique ouvert.
//!
//! Aujourd'hui la latence note→oreille compte le matériel comme « taille du
//! buffer + 2 ms » (constante). Ce module lit ce que le système déclare réellement
//! AU-DELÀ du buffer — convertisseurs, transport, marges du pilote — et le type
//! de transport (Bluetooth), pour la ligne « Matériel » de l'infobulle de latence
//! (dépôt web : `internal-docs/plans/PLAN-INFOBULLE-LATENCE-2026-09.md`).
//!
//! - **macOS** : CoreAudio — latence du périphérique + safety offset + latence
//!   du flux, dans le sens voulu.
//! - **Windows** : pas encore lu. `ASIOGetLatencies` sera branché après validation
//!   de la sonde `examples/asio_latency_probe.rs` sur une vraie machine.
//!
//! Lu à l'OUVERTURE du périphérique, hors du thread temps réel. Une valeur qui ne
//! peut pas être attribuée avec certitude (deux périphériques homonymes, flux aux
//! latences différentes) n'est PAS déclarée : l'appelant garde alors la constante,
//! publiée comme estimation. Jamais de valeur devinée.

use jamodio_audio_core::protocol::AudioTransport;

/// Sens du périphérique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Input,
    Output,
}

/// Ce que le pilote déclare pour le périphérique ouvert.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeclaredLatency {
    /// Latence matérielle au-delà du buffer, en millisecondes.
    pub hw_ms: f32,
    /// Type de transport du périphérique (Bluetooth ou autre).
    pub transport: AudioTransport,
}

/// Latence déclarée pour l'ENTRÉE ouverte (`device_name` = nom exact rendu par cpal).
pub fn input(device_name: &str) -> Option<DeclaredLatency> {
    declared(device_name, Scope::Input)
}

/// Latence déclarée pour la SORTIE ouverte (`device_name` = nom exact rendu par cpal).
pub fn output(device_name: &str) -> Option<DeclaredLatency> {
    declared(device_name, Scope::Output)
}

#[cfg(target_os = "macos")]
fn declared(device_name: &str, scope: Scope) -> Option<DeclaredLatency> {
    let result = coreaudio::declared(device_name, scope);
    match &result {
        Some(d) => tracing::info!(
            target: "jamodio::audio",
            device = %device_name,
            ?scope,
            hw_ms = d.hw_ms,
            transport = ?d.transport,
            "latence matérielle déclarée par CoreAudio"
        ),
        None => tracing::info!(
            target: "jamodio::audio",
            device = %device_name,
            ?scope,
            "latence matérielle non attribuable avec certitude — constante de repli publiée comme estimation"
        ),
    }
    result
}

#[cfg(not(target_os = "macos"))]
fn declared(_device_name: &str, _scope: Scope) -> Option<DeclaredLatency> {
    None
}

/// Un périphérique système candidat : identifiant, nom exact, flux dans le sens voulu.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Candidate {
    id: u32,
    name: String,
    streams: Vec<u32>,
}

/// Le seul périphérique dont le nom est EXACTEMENT celui ouvert et qui a au moins
/// un flux dans le sens voulu. Deux homonymes → `None` : on ne devine pas lequel
/// cpal a ouvert (même règle que l'identifiant strict `{idx}:{name}`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn unique_match<'a>(candidates: &'a [Candidate], name: &str) -> Option<&'a Candidate> {
    let mut matching = candidates
        .iter()
        .filter(|c| c.name == name && !c.streams.is_empty());
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

/// Latence matérielle en trames = latence du périphérique + safety offset + latence
/// du flux. Plusieurs flux aux latences différentes → `None` : on ne sait pas lequel
/// porte les canaux utilisés.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn hardware_frames(device: u32, safety_offset: u32, streams: &[u32]) -> Option<u32> {
    let stream = match streams {
        [] => 0,
        [first, rest @ ..] => {
            if rest.iter().any(|s| s != first) {
                return None;
            }
            *first
        }
    };
    device.checked_add(safety_offset)?.checked_add(stream)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn frames_to_ms(frames: u32, sample_rate: f64) -> Option<f32> {
    (sample_rate.is_finite() && sample_rate > 0.0)
        .then(|| (f64::from(frames) * 1000.0 / sample_rate) as f32)
}

#[cfg(target_os = "macos")]
mod coreaudio {
    use super::{frames_to_ms, hardware_frames, unique_match, Candidate, DeclaredLatency, Scope};
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::string::CFStringRef;
    use coreaudio_sys::{
        kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyLatency,
        kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
        kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
        kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
        kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioStreamPropertyLatency,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress,
    };
    use jamodio_audio_core::protocol::AudioTransport;
    use std::mem::size_of;
    use std::ptr::null;

    fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// Lit une propriété de taille fixe (`u32`, `f64`, `CFStringRef`).
    fn read<T: Copy>(object: AudioObjectID, selector: u32, scope: u32, zero: T) -> Option<T> {
        let addr = address(selector, scope);
        let mut value = zero;
        let mut size = size_of::<T>() as u32;
        // SAFETY : `value` est un `T` vivant de `size` octets ; CoreAudio n'écrit pas
        // au-delà de `size` et renvoie la taille réellement écrite.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                null(),
                &mut size,
                &mut value as *mut T as *mut _,
            )
        };
        (status == 0 && size as usize == size_of::<T>()).then_some(value)
    }

    /// Lit une propriété tableau d'`AudioObjectID` (périphériques, flux).
    fn read_ids(object: AudioObjectID, selector: u32, scope: u32) -> Option<Vec<AudioObjectID>> {
        let addr = address(selector, scope);
        let mut size: u32 = 0;
        // SAFETY : adresse valide, `size` est une locale vivante.
        let status = unsafe { AudioObjectGetPropertyDataSize(object, &addr, 0, null(), &mut size) };
        if status != 0 {
            return None;
        }
        let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
        if ids.is_empty() {
            return Some(ids);
        }
        // SAFETY : `ids` offre exactement `size` octets ; CoreAudio renvoie la taille écrite.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                null(),
                &mut size,
                ids.as_mut_ptr() as *mut _,
            )
        };
        if status != 0 {
            return None;
        }
        ids.truncate(size as usize / size_of::<AudioObjectID>());
        Some(ids)
    }

    /// Nom du périphérique, lu EXACTEMENT comme cpal le lit (même propriété, même
    /// portée) : c'est ce qui permet la comparaison stricte avec le nom ouvert.
    fn device_name(device: AudioObjectID) -> Option<String> {
        let cf: CFStringRef = read(
            device,
            kAudioDevicePropertyDeviceNameCFString,
            kAudioObjectPropertyScopeOutput,
            null(),
        )?;
        if cf.is_null() {
            return None;
        }
        let name = crate::cf_string::to_string(cf);
        // SAFETY : la propriété rend une CFString possédée par l'appelant.
        unsafe { CFRelease(cf as CFTypeRef) };
        name
    }

    fn scope_of(scope: Scope) -> u32 {
        match scope {
            Scope::Input => kAudioObjectPropertyScopeInput,
            Scope::Output => kAudioObjectPropertyScopeOutput,
        }
    }

    fn candidates(scope: Scope) -> Option<Vec<Candidate>> {
        let devices = read_ids(
            kAudioObjectSystemObject,
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        )?;
        Some(
            devices
                .into_iter()
                .filter_map(|id| {
                    Some(Candidate {
                        id,
                        name: device_name(id)?,
                        streams: read_ids(id, kAudioDevicePropertyStreams, scope_of(scope))?,
                    })
                })
                .collect(),
        )
    }

    pub(super) fn declared(device_name: &str, scope: Scope) -> Option<DeclaredLatency> {
        let all = candidates(scope)?;
        let device = unique_match(&all, device_name)?;
        let ca_scope = scope_of(scope);
        let rate = read(
            device.id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
            0f64,
        )?;
        let latency = read(device.id, kAudioDevicePropertyLatency, ca_scope, 0u32)?;
        let safety_offset = read(device.id, kAudioDevicePropertySafetyOffset, ca_scope, 0u32)?;
        let stream_latencies = device
            .streams
            .iter()
            .map(|s| {
                read(
                    *s,
                    kAudioStreamPropertyLatency,
                    kAudioObjectPropertyScopeGlobal,
                    0u32,
                )
            })
            .collect::<Option<Vec<u32>>>()?;
        let frames = hardware_frames(latency, safety_offset, &stream_latencies)?;
        let transport = read(
            device.id,
            kAudioDevicePropertyTransportType,
            kAudioObjectPropertyScopeGlobal,
            0u32,
        )?;
        Some(DeclaredLatency {
            hw_ms: frames_to_ms(frames, rate)?,
            transport: if transport == kAudioDeviceTransportTypeBluetooth
                || transport == kAudioDeviceTransportTypeBluetoothLE
            {
                AudioTransport::Bluetooth
            } else {
                AudioTransport::Other
            },
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Relevé des latences déclarées par TOUS les périphériques de la machine.
        /// Ignoré par défaut (dépend du matériel branché) :
        /// `cargo test -p jamodio-agent declared_latency -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn releve_des_peripheriques() {
            for scope in [Scope::Input, Scope::Output] {
                let Some(all) = candidates(scope) else {
                    println!("{scope:?} : énumération impossible");
                    continue;
                };
                for c in all.iter().filter(|c| !c.streams.is_empty()) {
                    println!("{scope:?} | {} | {:?}", c.name, declared(&c.name, scope));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: u32, name: &str, streams: usize) -> Candidate {
        Candidate {
            id,
            name: name.to_string(),
            streams: (0..streams as u32).collect(),
        }
    }

    #[test]
    fn un_seul_peripherique_au_nom_exact_avec_des_flux() {
        let all = [
            candidate(1, "MacBook Pro Microphone", 1),
            candidate(2, "Scarlett 2i2 USB", 1),
        ];
        assert_eq!(
            unique_match(&all, "Scarlett 2i2 USB").map(|c| c.id),
            Some(2)
        );
    }

    #[test]
    fn deux_homonymes_ne_sont_pas_departages() {
        let all = [candidate(1, "USB Audio", 1), candidate(2, "USB Audio", 1)];
        assert!(unique_match(&all, "USB Audio").is_none());
    }

    #[test]
    fn un_homonyme_sans_flux_dans_ce_sens_ne_compte_pas() {
        let all = [candidate(1, "USB Audio", 0), candidate(2, "USB Audio", 2)];
        assert_eq!(unique_match(&all, "USB Audio").map(|c| c.id), Some(2));
    }

    #[test]
    fn nom_absent_ou_approchant_rien() {
        let all = [candidate(1, "Scarlett 2i2 USB", 1)];
        assert!(unique_match(&all, "Scarlett 2i2").is_none());
    }

    #[test]
    fn trames_materielles_additionnees() {
        assert_eq!(hardware_frames(24, 12, &[48]), Some(84));
        assert_eq!(hardware_frames(24, 12, &[48, 48]), Some(84));
        assert_eq!(hardware_frames(24, 12, &[]), Some(36));
    }

    #[test]
    fn flux_aux_latences_differentes_non_declares() {
        assert_eq!(hardware_frames(24, 12, &[48, 96]), None);
    }

    #[test]
    fn debordement_non_declare() {
        assert_eq!(hardware_frames(u32::MAX, 1, &[0]), None);
    }

    #[test]
    fn conversion_trames_millisecondes() {
        assert_eq!(frames_to_ms(96, 48_000.0), Some(2.0));
        assert_eq!(frames_to_ms(96, 0.0), None);
        assert_eq!(frames_to_ms(96, f64::NAN), None);
    }
}
