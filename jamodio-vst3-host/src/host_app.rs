//! `IHostApplication` minimal — passé en `hostContext` aux `IPluginBase::initialize`.
//!
//! Plusieurs plugins commerciaux (Valhalla, FabFilter, NI…) vérifient la
//! présence d'un host context valide à l'init du controller et refusent leur
//! `createView` en silence si le contexte est null. Cette impl répond aux
//! 2 méthodes obligatoires :
//! - `getName` → renvoie "Jamodio" en UTF-16
//! - `createInstance` → renvoie `kNotImplemented`, le plugin retombe sur ses
//!   propres allocations VST3 (IBStream interne, etc.)
//!
//! Aucun state — `MinimalHost` est zéro-sized. Tenu en vie par la struct qui
//! l'a injecté (généralement `EditorThreadData`) pour la durée de vie du
//! controller / component qui le référencent.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

use vst3::{
    Class,
    Steinberg::{
        kResultFalse, kResultOk, tresult, TUID,
        Vst::{IHostApplication, IHostApplicationTrait, String128},
    },
};

const K_NOT_IMPLEMENTED: tresult = 0x80004001_u32 as i32; // E_NOTIMPL

pub struct MinimalHost;

impl Class for MinimalHost {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for MinimalHost {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kResultFalse;
        }
        // String128 = [TChar; 128] = [i16; 128] (UTF-16 nul-terminated).
        let host_name = "Jamodio";
        let dst = name as *mut i16;
        let chars: Vec<u16> = host_name.encode_utf16().collect();
        let n = chars.len().min(127); // garde un emplacement pour le \0
        for i in 0..n {
            *dst.add(i) = chars[i] as i16;
        }
        *dst.add(n) = 0;
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        _obj: *mut *mut c_void,
    ) -> tresult {
        // On ne fournit aucun objet VST3 alloué par le host. Le plugin a ses
        // propres impls IBStream/IMessage en interne et fera fallback dessus.
        K_NOT_IMPLEMENTED
    }
}
