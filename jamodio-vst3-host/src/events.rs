//! `MidiEventList` — impl `IEventList` Steinberg pour pousser des notes MIDI
//! aux plugins VST3 instrument pendant `IAudioProcessor::process()`.
//!
//! Convertit nos `MidiEvent` (= 3 bytes MIDI brut + frame_offset) en `Event`
//! VST3 (= struct C avec union noteOn/noteOff/CC), puis expose le tout via
//! l'interface COM `IEventList` que le plugin lit pendant son render.
//!
//! Scope MVP : NoteOn / NoteOff uniquement. CC, pitch bend, channel pressure
//! viendront dans un sprint dédié si besoin (le clavier HTML de Jamodio ne
//! produit que des NoteOn/NoteOff pour l'instant).
//!
//! Threading : utilisé sur l'audio RT thread (encoder_thread). Pas
//! d'allocation après création — la `Mutex<Vec<Event>>` est pré-réservée
//! avec une capacité raisonnable (64 events/bloc) et `clear()` est lock-free
//! côté Vec.

#![cfg(target_os = "windows")]

use std::sync::Mutex;

use jamodio_audio_core::plugin_host::MidiEvent;
use vst3::{
    Class,
    Steinberg::{
        int32, kResultFalse, kResultOk, tresult,
        Vst::{
            Event, Event__type0, Event_::EventTypes_, IEventList, IEventListTrait, NoteOffEvent,
            NoteOnEvent,
        },
    },
};

/// Liste d'events VST3 alimentée à chaque bloc audio, lue par le plugin via
/// `IEventList::getEvent`. Le plugin n'écrit jamais dedans (la spec autorise
/// `addEvent`, mais c'est pour les hôtes qui ré-injectent les events MIDI
/// sortants — pas notre cas en S2).
pub struct MidiEventList {
    events: Mutex<Vec<Event>>,
}

impl MidiEventList {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::with_capacity(64)),
        }
    }

    /// Remplace la liste interne par une nouvelle batch d'events. Pratique
    /// avant chaque appel `IAudioProcessor::process()` — on convertit nos
    /// `MidiEvent` en `Event` VST3 et on swap.
    pub fn set_batch(&self, midi: &[MidiEvent]) {
        let mut guard = match self.events.lock() {
            Ok(g) => g,
            Err(_) => return, // poisoned mutex, on n'expose pas d'events
        };
        guard.clear();
        for m in midi {
            if let Some(ev) = midi_to_vst3_event(m) {
                guard.push(ev);
            }
            // Si conversion échoue (= MIDI type non supporté) on skip
            // silencieusement. CC/pitch bend etc. viendront plus tard.
        }
    }

}

impl Default for MidiEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for MidiEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for MidiEventList {
    unsafe fn getEventCount(&self) -> int32 {
        match self.events.lock() {
            Ok(g) => g.len() as int32,
            Err(_) => 0,
        }
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        if e.is_null() || index < 0 {
            return kResultFalse;
        }
        let guard = match self.events.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        match guard.get(index as usize) {
            Some(ev) => {
                *e = *ev;
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kResultFalse;
        }
        let mut guard = match self.events.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        guard.push(*e);
        kResultOk
    }
}

/// Convertit un `MidiEvent` brut (3 bytes status/data1/data2) en `Event` VST3.
/// Retourne `None` pour les types non supportés au MVP (CC, pitch bend, etc.).
fn midi_to_vst3_event(midi: &MidiEvent) -> Option<Event> {
    let status = midi.data[0];
    let kind = status & 0xF0;
    let channel = (status & 0x0F) as i16;
    let mut event: Event = unsafe { std::mem::zeroed() };
    event.busIndex = 0;
    event.sampleOffset = midi.frame_offset as i32;
    event.ppqPosition = 0.0;
    event.flags = 0;

    match kind {
        // NoteOn (0x90) avec velocity > 0
        0x90 if midi.data[2] > 0 => {
            event.r#type = EventTypes_::kNoteOnEvent as u16;
            event.__field0 = Event__type0 {
                noteOn: NoteOnEvent {
                    channel,
                    pitch: midi.data[1] as i16,
                    tuning: 0.0,
                    velocity: midi.data[2] as f32 / 127.0,
                    length: 0,
                    noteId: -1, // -1 = auto-attribué par le plugin
                },
            };
            Some(event)
        }
        // NoteOff (0x80) OU NoteOn avec velocity=0 (= NoteOff implicite par convention MIDI)
        0x80 | 0x90 => {
            event.r#type = EventTypes_::kNoteOffEvent as u16;
            event.__field0 = Event__type0 {
                noteOff: NoteOffEvent {
                    channel,
                    pitch: midi.data[1] as i16,
                    velocity: midi.data[2] as f32 / 127.0,
                    noteId: -1,
                    tuning: 0.0,
                },
            };
            Some(event)
        }
        // CC, pitch bend, channel pressure, sysex… post-MVP.
        _ => None,
    }
}
