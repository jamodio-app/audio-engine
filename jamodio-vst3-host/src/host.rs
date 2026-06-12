//! Wrapper haut niveau autour de `IPluginFactory` → `IComponent` + `IAudioProcessor`.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

use jamodio_audio_core::plugin_host::MidiEvent;
use vst3::{
    ComPtr, ComWrapper,
    Steinberg::{
        FUnknown, IPluginBaseTrait, IPluginFactoryTrait, PClassInfo, PFactoryInfo, TUID,
        Vst::{
            AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, IAudioProcessor,
            IAudioProcessorTrait, IComponent, IComponentTrait, IComponent_iid, IEventList,
            IHostApplication, MediaTypes_, ProcessData, ProcessModes_, ProcessSetup, SpeakerArr,
            SpeakerArrangement, SymbolicSampleSizes_,
        },
    },
};

use crate::events::MidiEventList;
use crate::host_app::MinimalHost;
use crate::loader::LoadedModule;

/// Catégorie standard VST3 pour les plugins audio (synthés + effets).
/// Les "Component Controller Class" et "Test Class" sont ignorées.
pub const AUDIO_EFFECT_CATEGORY: &str = "Audio Module Class";

/// VST3 SDK fixed-size cstring fields use `char8` (i8) padded with nulls.
/// Decode stops at the first 0 byte or buffer end.
pub fn decode_cstr_i8(buf: &[i8]) -> String {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let bytes: Vec<u8> = buf[..len].iter().map(|&b| b as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Clone, Debug)]
pub struct FactoryInfo {
    pub vendor: String,
    /// Affiché par le POC en mode `info`, gardé pour diag/log futur.
    #[allow(dead_code)]
    pub url: String,
    /// Affiché par le POC en mode `info`, gardé pour diag/log futur.
    #[allow(dead_code)]
    pub email: String,
}

#[derive(Clone, Debug)]
pub struct ClassInfo {
    /// Index dans la factory — utile pour diag (= "la classe n°3 a échoué").
    #[allow(dead_code)]
    pub index: i32,
    pub cid: TUID,
    pub category: String,
    pub name: String,
}

impl ClassInfo {
    pub fn is_audio_effect(&self) -> bool {
        self.category == AUDIO_EFFECT_CATEGORY
    }

    /// Représentation hex 32-chars de l'UID de classe — utilisée dans
    /// `PluginRef::Vst3 { uid }` pour persister la sélection du plugin.
    pub fn uid_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for byte in self.cid.iter() {
            s.push_str(&format!("{:02X}", *byte as u8));
        }
        s
    }
}

pub fn factory_info(module: &LoadedModule) -> Result<FactoryInfo, String> {
    let mut info = PFactoryInfo {
        vendor: [0; 64],
        url: [0; 256],
        email: [0; 128],
        flags: 0,
    };
    let ok = unsafe { module.factory().getFactoryInfo(&mut info) };
    if ok != 0 {
        return Err(format!("getFactoryInfo tresult={ok}"));
    }
    Ok(FactoryInfo {
        vendor: decode_cstr_i8(&info.vendor),
        url: decode_cstr_i8(&info.url),
        email: decode_cstr_i8(&info.email),
    })
}

pub fn enumerate_classes(module: &LoadedModule) -> Vec<ClassInfo> {
    let factory = module.factory();
    let count = unsafe { factory.countClasses() };
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut info = PClassInfo {
            cid: [0; 16],
            cardinality: 0,
            category: [0; 32],
            name: [0; 64],
        };
        let ok = unsafe { factory.getClassInfo(i, &mut info) };
        if ok != 0 {
            tracing::warn!(target: "jamodio::vst3", index = i, tresult = ok, "getClassInfo failed");
            continue;
        }
        out.push(ClassInfo {
            index: i,
            cid: info.cid,
            category: decode_cstr_i8(&info.category),
            name: decode_cstr_i8(&info.name),
        });
    }
    out
}

/// Décode un UID hex 32-chars (= ce qu'on stocke dans `PluginRef::Vst3.uid`)
/// vers son `TUID` natif.
pub fn parse_uid_hex(hex: &str) -> Option<TUID> {
    if hex.len() != 32 {
        return None;
    }
    let mut out: TUID = [0; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let s = hex.get(i * 2..i * 2 + 2)?;
        *byte = u8::from_str_radix(s, 16).ok()? as i8;
    }
    Some(out)
}

/// Instance VST3 active. Détient `IComponent` + `IAudioProcessor` (ComPtrs
/// avec leur refcount). Drop libère l'AU dans le bon ordre via le trait `Drop`.
pub struct Instance {
    pub class: ClassInfo,
    pub component: ComPtr<IComponent>,
    pub audio: ComPtr<IAudioProcessor>,
    /// `true` dès que `IPluginBase::initialize` a réussi. Sépare l'init du
    /// composant (qui DOIT être balancée par `terminate()`) de `setup_done`
    /// (= setupProcessing/setActive). Un plugin dont `initialize` réussit mais
    /// `setup_stereo` échoue (fréquent au scan : pas de bus out) doit quand
    /// même être `terminate()` au drop — sinon contrat IPluginBase violé + leak.
    pub initialized: bool,
    pub setup_done: bool,
    pub active: bool,
    pub processing: bool,
    /// Liste d'events VST3 partagée entre nous (push via `set_batch`) et le
    /// plugin (lit via `IEventList` pendant `process()`). Allouée une fois
    /// au load, ré-utilisée à chaque bloc audio (= alloc-free dans le hot path).
    event_list: ComWrapper<MidiEventList>,
    /// Cache du `ComPtr<IEventList>` pour ne pas refaire `to_com_ptr` à chaque
    /// bloc audio (= éviterait un refcount inc/dec inutile sur le hot path).
    event_list_ptr: ComPtr<IEventList>,
    /// Host context passé à `IComponent::initialize` — le plugin peut garder
    /// le pointeur pendant toute sa vie, donc keep-alive jusqu'au drop
    /// (déclaré APRÈS component/audio = droppé après leur release).
    _host_app: ComPtr<IHostApplication>,
}

impl Instance {
    /// Crée une instance d'une classe Audio Module Class identifiée par son
    /// `cid` (= UID natif). Si pas trouvée OU pas Audio Module Class, retourne
    /// une erreur explicite.
    pub fn create_by_uid(module: &LoadedModule, cid: &TUID) -> Result<Self, String> {
        let class = enumerate_classes(module)
            .into_iter()
            .find(|c| &c.cid == cid && c.is_audio_effect())
            .ok_or_else(|| "UID inconnu ou pas Audio Module Class".to_string())?;
        Self::create_class(module, class)
    }

    fn create_class(module: &LoadedModule, class: ClassInfo) -> Result<Self, String> {
        let mut comp_raw: *mut c_void = std::ptr::null_mut();
        let ok = unsafe {
            module.factory().createInstance(
                class.cid.as_ptr() as *const i8,
                IComponent_iid.as_ptr() as *const i8,
                &mut comp_raw,
            )
        };
        if ok != 0 || comp_raw.is_null() {
            return Err(format!("createInstance(IComponent) tresult={ok}"));
        }
        let component = unsafe { ComPtr::<IComponent>::from_raw(comp_raw as *mut IComponent) }
            .ok_or_else(|| "createInstance retourne non-null mais ComPtr a refusé".to_string())?;

        // IPluginBase::initialize(hostContext). Le host context fournit
        // getName + createInstance(IMessage/IAttributeList) — requis pour que
        // le component puisse allouer des messages (`ComponentBase::
        // allocateMessage` des plugins JUCE jassert hostContext != null).
        let host_app_wrapper = ComWrapper::new(MinimalHost);
        let host_app = host_app_wrapper
            .to_com_ptr::<IHostApplication>()
            .ok_or_else(|| "ComWrapper::to_com_ptr<IHostApplication> a échoué".to_string())?;
        let init_ok = unsafe { component.initialize(host_app.as_ptr() as *mut FUnknown) };
        if init_ok != 0 {
            return Err(format!("IComponent::initialize tresult={init_ok}"));
        }

        // IAudioProcessor est sur la même instance que IComponent dans 99% des
        // plugins commerciaux (= "single component" pattern). On query par cast.
        let audio = component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| "plugin n'expose pas IAudioProcessor".to_string())?;

        // IEventList partagé pour MIDI dispatch (HTML keyboard + USB MIDI).
        // Alloué une fois ici, reset à chaque bloc dans process_stereo.
        let event_list = ComWrapper::new(MidiEventList::new());
        let event_list_ptr = event_list
            .to_com_ptr::<IEventList>()
            .ok_or_else(|| "MidiEventList::to_com_ptr<IEventList> a échoué".to_string())?;

        Ok(Self {
            class,
            component,
            audio,
            initialized: true, // component.initialize() a réussi ci-dessus
            setup_done: false,
            active: false,
            processing: false,
            event_list,
            event_list_ptr,
            _host_app: host_app,
        })
    }

    /// Configure l'instance pour du traitement stéréo float32 realtime.
    /// `sample_rate = 48000`, `max_samples = bloc CPAL max (typiquement 64)`.
    pub fn setup_stereo(&mut self, sample_rate: f64, max_samples: i32) -> Result<(), String> {
        // canProcessSampleSize(kSample32) — float32 = 99% des plugins modernes.
        let can_32 = unsafe {
            self.audio
                .canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32)
        };
        if can_32 != 0 {
            return Err(format!(
                "'{}' ne supporte pas float32 (tresult={can_32})",
                self.class.name
            ));
        }

        let n_in = unsafe {
            self.component
                .getBusCount(MediaTypes_::kAudio as i32, BusDirections_::kInput as i32)
        };
        let n_out = unsafe {
            self.component
                .getBusCount(MediaTypes_::kAudio as i32, BusDirections_::kOutput as i32)
        };
        tracing::debug!(
            target: "jamodio::vst3",
            name = %self.class.name,
            audio_in = n_in,
            audio_out = n_out,
            "bus counts"
        );
        if n_out < 1 {
            return Err(format!(
                "'{}' n'a aucun bus audio out (MIDI-only ?)",
                self.class.name
            ));
        }

        // setBusArrangements(stereo). Si le plugin refuse, on log et on continue —
        // il appliquera son layout par défaut, et process() dira si c'est fatal.
        let mut in_arr: SpeakerArrangement = SpeakerArr::kStereo;
        let mut out_arr: SpeakerArrangement = SpeakerArr::kStereo;
        let arr_ok = unsafe {
            self.audio.setBusArrangements(
                if n_in > 0 { &mut in_arr } else { std::ptr::null_mut() },
                if n_in > 0 { 1 } else { 0 },
                &mut out_arr,
                1,
            )
        };
        if arr_ok != 0 {
            tracing::warn!(
                target: "jamodio::vst3",
                name = %self.class.name,
                tresult = arr_ok,
                "setBusArrangements stereo refusé — layout default plugin retenu"
            );
        }

        // activateBus : on N'ACTIVE QUE le bus 0 (= main) pour chaque media×dir.
        //
        // Le piège VST3 (analogue de AU `bus.enabled=NO`) demande d'activer
        // explicitement les bus, mais la spec dit que `numInputs/numOutputs`
        // dans `ProcessData` doit ÉGALER le nombre de bus actifs et que
        // `inputs[]`/`outputs[]` correspond exactement. Si on active 3 bus de
        // sortie (= cas Surge XT main + aux1 + aux2) mais qu'on déclare
        // `numOutputs=1`, le plugin peut écrire silencieusement dans des bus
        // non couverts par notre buffer → audio perdu, VU à zéro.
        //
        // MVP : on désactive explicitement les bus aux pour rester sur le
        // pattern simple "1 bus main I/O + 1 bus event in". Le multi-bus
        // (sidechain, aux sends pour mixing in-plugin) sera un sprint dédié
        // post-beta.
        for media in [MediaTypes_::kAudio, MediaTypes_::kEvent] {
            for dir in [BusDirections_::kInput, BusDirections_::kOutput] {
                let n = unsafe {
                    self.component.getBusCount(media as i32, dir as i32)
                };
                for idx in 0..n {
                    let state: u8 = if idx == 0 { 1 } else { 0 };
                    let act_ok = unsafe {
                        self.component.activateBus(media as i32, dir as i32, idx, state)
                    };
                    if act_ok != 0 {
                        tracing::warn!(
                            target: "jamodio::vst3",
                            media = ?media,
                            dir = ?dir,
                            idx,
                            state,
                            tresult = act_ok,
                            "activateBus failed"
                        );
                    } else {
                        tracing::debug!(
                            target: "jamodio::vst3",
                            media = ?media,
                            dir = ?dir,
                            idx,
                            state,
                            "activateBus"
                        );
                    }
                }
            }
        }

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: max_samples,
            sampleRate: sample_rate,
        };
        let setup_ok = unsafe { self.audio.setupProcessing(&mut setup) };
        if setup_ok != 0 {
            return Err(format!("setupProcessing tresult={setup_ok}"));
        }
        self.setup_done = true;

        let active_ok = unsafe { self.component.setActive(1) };
        if active_ok != 0 {
            return Err(format!("setActive(true) tresult={active_ok}"));
        }
        self.active = true;

        // setProcessing(true) — recommandé par la spec, certains plugins
        // n'allouent leur état interne qu'à ce moment. Tolérant à l'échec
        // (vieux plugins ignorent l'appel).
        let proc_ok = unsafe { self.audio.setProcessing(1) };
        if proc_ok == 0 {
            self.processing = true;
        }

        Ok(())
    }

    /// Latence intrinsèque rapportée par le plugin (en samples). Stable
    /// après setupProcessing.
    pub fn latency_samples(&self) -> u32 {
        unsafe { self.audio.getLatencySamples() }
    }

    pub fn has_input_bus(&self) -> bool {
        let n = unsafe {
            self.component
                .getBusCount(MediaTypes_::kAudio as i32, BusDirections_::kInput as i32)
        };
        n > 0
    }

    /// Process un bloc stéréo float32 IN-PLACE, avec dispatch optionnel d'events MIDI.
    ///
    /// `left` et `right` contiennent l'entrée à l'appel, la sortie au retour.
    /// `midi_events` est forwardé au plugin via l'`IEventList` partagé — pour
    /// les plugins instrument qui génèrent leur audio depuis des notes MIDI.
    /// Appelé depuis l'encoder_thread (RT) — alloc-free (set_batch fait juste
    /// un clear+push dans un Vec préalloué à 64 events).
    pub fn process_stereo(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        midi_events: &[MidiEvent],
    ) -> Result<(), String> {
        if !self.active {
            return Err("instance not active".into());
        }
        if left.len() != right.len() {
            return Err("L/R len mismatch".into());
        }
        let n = left.len() as i32;

        // Remplit l'IEventList avec les events MIDI du bloc courant (NoteOn/Off).
        // Le plugin les lit pendant `process` via `IEventList::getEvent`.
        self.event_list.set_batch(midi_events);

        let has_input = self.has_input_bus();

        let mut in_ptrs: [*mut f32; 2] = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [left.as_mut_ptr(), right.as_mut_ptr()];
        // ⚠️ In-place I/O : on partage les buffers L/R entre IN et OUT. Les
        // plugins audio VST3 doivent tolérer ça (et le font tous, c'est le
        // pattern DAW standard). Si jamais un plugin glitche, basculer sur
        // des buffers OUT séparés.

        let mut in_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: in_ptrs.as_mut_ptr(),
            },
        };
        let mut out_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: out_ptrs.as_mut_ptr(),
            },
        };

        let num_inputs = if has_input { 1 } else { 0 };
        let input_events_ptr = if midi_events.is_empty() {
            std::ptr::null_mut()
        } else {
            self.event_list_ptr.as_ptr()
        };
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n,
            numInputs: num_inputs,
            numOutputs: 1,
            inputs: if has_input {
                &mut in_bus
            } else {
                std::ptr::null_mut()
            },
            outputs: &mut out_bus,
            inputParameterChanges: std::ptr::null_mut(),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: input_events_ptr,
            outputEvents: std::ptr::null_mut(),
            processContext: std::ptr::null_mut(),
        };

        let ok = unsafe { self.audio.process(&mut data) };
        if ok != 0 {
            return Err(format!("process tresult={ok}"));
        }
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if self.processing {
                let _ = self.audio.setProcessing(0);
            }
            if self.active {
                let _ = self.component.setActive(0);
            }
            // terminate() balance initialize() — indépendant de setup_done
            // (sinon une instance initialize-ok/setup-fail fuit, cf. scan).
            if self.initialized {
                let _ = self.component.terminate();
            }
        }
        // ComPtr<IAudioProcessor> + ComPtr<IComponent> droppent ensuite dans
        // l'ordre de déclaration → release → la DLL elle-même est gardée
        // vivante via LoadedModule jusqu'à drop de l'Entry par le Vst3Host.
    }
}
