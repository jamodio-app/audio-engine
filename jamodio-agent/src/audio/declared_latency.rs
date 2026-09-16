//! Latence matérielle du périphérique ouvert, AU-DELÀ du buffer : ce que le pilote
//! déclare, remplacé par une mesure Jamodio quand la déclaration est connue pour
//! être fausse (`measured_latency`).
//!
//! Elle alimente la ligne « Matériel » de l'infobulle de latence (dépôt web :
//! `internal-docs/plans/PLAN-INFOBULLE-LATENCE-2026-09.md`).
//!
//! - **macOS** : CoreAudio — latence du périphérique + safety offset + latence
//!   du flux, dans le sens voulu. Les trois termes sont journalisés : le banc du
//!   15/09/2026 a montré que la latence de flux du micro intégré des Mac surestime
//!   la réalité d'environ 20 ms.
//! - **Windows** : `ASIOGetLatencies`, lu par le host ASIO après la création des
//!   buffers (`asio_beyond_buffer`). La latence déclarée INCLUT le buffer ; on en
//!   publie la part au-delà. ASIO ne dit rien du transport (Bluetooth indétectable
//!   derrière un pilote générique). Validé par la sonde du 15/09/2026 (Focusrite USB :
//!   2,60 ms au-delà du buffer ; ASIO4ALL : 10,29 ms).
//!
//! Lu à l'OUVERTURE du périphérique, hors du thread temps réel. Une valeur qui ne
//! peut pas être attribuée avec certitude (deux périphériques homonymes, flux aux
//! latences différentes) n'est PAS publiée : l'appelant garde alors la constante,
//! publiée comme estimation. Jamais de valeur devinée.

use super::measured_latency::Measured;
use jamodio_audio_core::protocol::{AudioTransport, HwDeviceKind, HwLatencySource};

/// Sens du périphérique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Input,
    Output,
}

/// Latence matérielle retenue pour le périphérique ouvert.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HardwareLatency {
    /// Au-delà du buffer, en millisecondes : la mesure Jamodio s'il y en a une,
    /// sinon la déclaration du pilote.
    pub hw_ms: f32,
    /// Type de transport du périphérique (Bluetooth ou autre) ; `None` quand le
    /// système ne le déclare pas (ASIO).
    pub transport: Option<AudioTransport>,
    /// Présent quand une mesure Jamodio remplace la déclaration du système.
    pub bench: Option<BenchReplacement>,
}

/// Une mesure Jamodio retenue à la place de la déclaration du système.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BenchReplacement {
    pub kind: HwDeviceKind,
    /// Ce que le système déclarait, gardé pour la transparence (journal, infobulle).
    pub declared_hw_ms: f32,
}

impl HardwareLatency {
    /// Une déclaration du pilote, sans mesure Jamodio.
    fn declared(hw_ms: f32, transport: Option<AudioTransport>) -> Self {
        Self {
            hw_ms,
            transport,
            bench: None,
        }
    }

    /// Origine de `hw_ms` (`Stats.inputHwSource` / `outputHwSource`).
    pub fn source(&self) -> HwLatencySource {
        if self.bench.is_some() {
            HwLatencySource::Bench
        } else {
            HwLatencySource::Declared
        }
    }

    /// La mesure Jamodio remplace la déclaration quand il y en a une.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn with_measured(self, measured: Option<Measured>) -> Self {
        match measured {
            Some(m) => Self {
                hw_ms: m.hw_ms,
                transport: self.transport,
                bench: Some(BenchReplacement {
                    kind: m.kind,
                    declared_hw_ms: self.hw_ms,
                }),
            },
            None => self,
        }
    }
}

/// Latence matérielle de l'ENTRÉE ouverte (`device_name` = nom exact rendu par cpal).
pub fn input(device_name: &str) -> Option<HardwareLatency> {
    retained(device_name, Scope::Input)
}

/// Latence matérielle de la SORTIE ouverte (`device_name` = nom exact rendu par cpal).
pub fn output(device_name: &str) -> Option<HardwareLatency> {
    retained(device_name, Scope::Output)
}

/// Latence déclarée par un pilote ASIO (`ASIOGetLatencies`, buffer INCLUS), ramenée à
/// la part au-delà du buffer. Transport inconnu (`None`). Valeur incohérente
/// (négative, inférieure au buffer, rate invalide) → `None` : jamais devinée.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn asio_beyond_buffer(
    latency_frames: i32,
    buffer_frames: u32,
    sample_rate: u32,
) -> Option<HardwareLatency> {
    let beyond = u32::try_from(latency_frames)
        .ok()?
        .checked_sub(buffer_frames)?;
    Some(HardwareLatency::declared(
        frames_to_ms(beyond, f64::from(sample_rate))?,
        None,
    ))
}

#[cfg(target_os = "macos")]
fn retained(device_name: &str, scope: Scope) -> Option<HardwareLatency> {
    let Some(reading) = coreaudio::read(device_name, scope) else {
        tracing::info!(
            target: "jamodio::audio",
            device = %device_name,
            ?scope,
            "latence matérielle non attribuable avec certitude (périphérique introuvable ou homonyme) — constante de repli publiée comme estimation"
        );
        return None;
    };
    let declared_ms = hardware_frames(
        reading.device_frames,
        reading.safety_offset_frames,
        &reading.stream_frames,
    )
    .and_then(|frames| frames_to_ms(frames, reading.sample_rate));
    tracing::info!(
        target: "jamodio::audio",
        device = %device_name,
        ?scope,
        device_frames = reading.device_frames,
        safety_offset_frames = reading.safety_offset_frames,
        stream_frames = ?reading.stream_frames,
        sample_rate = reading.sample_rate,
        hw_ms = ?declared_ms,
        transport = ?reading.transport,
        built_in = reading.identity.built_in,
        data_source = %reading.identity.data_source.map_or_else(|| "-".to_string(), fourcc_text),
        "latence matérielle déclarée par CoreAudio"
    );
    let Some(declared_ms) = declared_ms else {
        tracing::info!(
            target: "jamodio::audio",
            device = %device_name,
            ?scope,
            "latence matérielle non attribuable avec certitude (flux aux latences différentes) — constante de repli publiée comme estimation"
        );
        return None;
    };
    let latency = HardwareLatency::declared(declared_ms, Some(reading.transport))
        .with_measured(super::measured_latency::coreaudio(reading.identity));
    if let Some(bench) = latency.bench {
        tracing::info!(
            target: "jamodio::audio",
            device = %device_name,
            ?scope,
            kind = ?bench.kind,
            hw_ms = latency.hw_ms,
            declared_hw_ms = bench.declared_hw_ms,
            "latence matérielle retenue : mesurée par Jamodio (la déclaration du système est connue pour être fausse)"
        );
    }
    Some(latency)
}

#[cfg(not(target_os = "macos"))]
fn retained(_device_name: &str, _scope: Scope) -> Option<HardwareLatency> {
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

fn frames_to_ms(frames: u32, sample_rate: f64) -> Option<f32> {
    (sample_rate.is_finite() && sample_rate > 0.0)
        .then(|| (f64::from(frames) * 1000.0 / sample_rate) as f32)
}

/// Code CoreAudio à quatre caractères, lisible (`imic`) ou en hexadécimal.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn fourcc_text(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(u8::is_ascii_graphic) {
        bytes.iter().map(|&b| char::from(b)).collect()
    } else {
        format!("{code:#010x}")
    }
}

#[cfg(target_os = "macos")]
mod coreaudio {
    use super::super::measured_latency::CoreAudioIdentity;
    use super::{unique_match, Candidate, Scope};
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::string::CFStringRef;
    use coreaudio_sys::{
        kAudioDevicePropertyDataSource, kAudioDevicePropertyDeviceNameCFString,
        kAudioDevicePropertyLatency, kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertySafetyOffset, kAudioDevicePropertyStreams,
        kAudioDevicePropertyTransportType, kAudioDeviceTransportTypeBluetooth,
        kAudioDeviceTransportTypeBluetoothLE, kAudioDeviceTransportTypeBuiltIn,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
        kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioStreamPropertyLatency,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress,
    };
    use jamodio_audio_core::protocol::AudioTransport;
    use std::mem::size_of;
    use std::ptr::null;

    /// Ce que CoreAudio déclare pour le périphérique ouvert, terme par terme.
    pub(super) struct Reading {
        pub device_frames: u32,
        pub safety_offset_frames: u32,
        pub stream_frames: Vec<u32>,
        pub sample_rate: f64,
        pub transport: AudioTransport,
        pub identity: CoreAudioIdentity,
    }

    fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// Lit une propriété de taille fixe (`u32`, `f64`, `CFStringRef`).
    fn property<T: Copy>(object: AudioObjectID, selector: u32, scope: u32, zero: T) -> Option<T> {
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
        let cf: CFStringRef = property(
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

    pub(super) fn read(device_name: &str, scope: Scope) -> Option<Reading> {
        let all = candidates(scope)?;
        let device = unique_match(&all, device_name)?;
        let ca_scope = scope_of(scope);
        let sample_rate = property(
            device.id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
            0f64,
        )?;
        let device_frames = property(device.id, kAudioDevicePropertyLatency, ca_scope, 0u32)?;
        let safety_offset_frames =
            property(device.id, kAudioDevicePropertySafetyOffset, ca_scope, 0u32)?;
        let stream_frames = device
            .streams
            .iter()
            .map(|s| {
                property(
                    *s,
                    kAudioStreamPropertyLatency,
                    kAudioObjectPropertyScopeGlobal,
                    0u32,
                )
            })
            .collect::<Option<Vec<u32>>>()?;
        let transport_type = property(
            device.id,
            kAudioDevicePropertyTransportType,
            kAudioObjectPropertyScopeGlobal,
            0u32,
        )?;
        Some(Reading {
            device_frames,
            safety_offset_frames,
            stream_frames,
            sample_rate,
            transport: if transport_type == kAudioDeviceTransportTypeBluetooth
                || transport_type == kAudioDeviceTransportTypeBluetoothLE
            {
                AudioTransport::Bluetooth
            } else {
                AudioTransport::Other
            },
            identity: CoreAudioIdentity {
                scope,
                built_in: transport_type == kAudioDeviceTransportTypeBuiltIn,
                // Facultative : un périphérique sans sources de données n'en déclare pas.
                data_source: property(device.id, kAudioDevicePropertyDataSource, ca_scope, 0u32),
            },
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Relevé des latences retenues pour TOUS les périphériques de la machine.
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
                    println!("{scope:?} | {} | {:?}", c.name, super::super::retained(&c.name, scope));
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

    #[test]
    fn micro_integre_du_mac_releve_du_banc() {
        // Relevé du 15/09/2026 : appareil 0 + marge 50 + flux 2399 trames à 48 kHz.
        let frames = hardware_frames(0, 50, &[2399]).expect("trames");
        let declared = frames_to_ms(frames, 48_000.0).expect("ms");
        assert!((declared - 51.020_832).abs() < 1e-3);
    }

    #[test]
    fn une_declaration_sans_mesure_jamodio_reste_declaree() {
        let d = HardwareLatency::declared(3.54, Some(AudioTransport::Other)).with_measured(None);
        assert_eq!(d.hw_ms, 3.54);
        assert_eq!(d.bench, None);
        assert_eq!(d.source(), HwLatencySource::Declared);
    }

    #[test]
    fn la_mesure_jamodio_remplace_la_declaration_et_la_garde() {
        let measured = Measured {
            kind: HwDeviceKind::AppleBuiltInMic,
            hw_ms: 30.0,
        };
        let d = HardwareLatency::declared(51.02, Some(AudioTransport::Other))
            .with_measured(Some(measured));
        assert_eq!(d.hw_ms, 30.0);
        assert_eq!(d.transport, Some(AudioTransport::Other));
        assert_eq!(d.source(), HwLatencySource::Bench);
        assert_eq!(
            d.bench,
            Some(BenchReplacement {
                kind: HwDeviceKind::AppleBuiltInMic,
                declared_hw_ms: 51.02
            })
        );
    }

    #[test]
    fn code_a_quatre_caracteres_lisible() {
        assert_eq!(fourcc_text(u32::from_be_bytes(*b"imic")), "imic");
        assert_eq!(fourcc_text(7), "0x00000007");
    }

    #[test]
    fn asio_part_au_dela_du_buffer_releves_de_la_sonde() {
        // Sonde du 15/09/2026, buffer 64 à 48 kHz.
        let focusrite = asio_beyond_buffer(189, 64, 48_000).expect("Focusrite USB ASIO");
        assert!((focusrite.hw_ms - 2.604_166_7).abs() < 1e-4);
        assert_eq!(focusrite.transport, None);
        assert_eq!(focusrite.source(), HwLatencySource::Declared);
        let generic = asio_beyond_buffer(558, 64, 48_000).expect("ASIO4ALL");
        assert!((generic.hw_ms - 10.291_667).abs() < 1e-4);
    }

    #[test]
    fn asio_latence_egale_au_buffer_rien_au_dela() {
        assert_eq!(
            asio_beyond_buffer(64, 64, 48_000).map(|d| d.hw_ms),
            Some(0.0)
        );
    }

    #[test]
    fn asio_valeurs_incoherentes_non_declarees() {
        assert_eq!(asio_beyond_buffer(32, 64, 48_000), None);
        assert_eq!(asio_beyond_buffer(-1, 64, 48_000), None);
        assert_eq!(asio_beyond_buffer(189, 64, 0), None);
    }
}
