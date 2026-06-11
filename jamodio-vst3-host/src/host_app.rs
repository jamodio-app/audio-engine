//! `IHostApplication` — passé en `hostContext` aux `IPluginBase::initialize`.
//!
//! Implémente les 2 méthodes obligatoires :
//! - `getName` → "Jamodio" en UTF-16
//! - `createInstance` → fournit `IMessage` et `IAttributeList` alloués par le
//!   host (mêmes classes que `hostclasses.cpp` du SDK Steinberg).
//!
//! # Pourquoi createInstance est OBLIGATOIRE (pas optionnel)
//!
//! Quand component et controller sont reliés via un `ConnectionProxy` (notre
//! cas, cf. `conn_proxy.rs`), les plugins JUCE (Surge XT, Valhalla…) ne
//! peuvent plus faire leur cast direct privé controller↔component. Leur
//! fallback (`JuceVST3EditController::connect` → `sendIntMessage`) alloue un
//! message via `hostContext->createInstance(IMessage)` pour transmettre le
//! pointeur du controller au component. Sans cette impl, le handshake échoue
//! en silence, le controller n'a jamais son AudioProcessor, et
//! `createView('editor')` retourne null (symptôme v0.4.25/v0.4.26).

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::Mutex;

use vst3::{
    Class, ComPtr, ComWrapper,
    Steinberg::{
        char16, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
        uint32, TUID,
        Vst::{
            IAttributeList, IAttributeListTrait, IAttributeList_iid, IHostApplication,
            IHostApplicationTrait, IMessage, IMessageTrait, IMessage_iid, String128,
        },
        FIDString,
    },
};

// ---------- HostAttributeList ----------

enum Attr {
    Int(i64),
    Float(f64),
    /// UTF-16 SANS terminateur (longueur = .len()).
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

/// `IAttributeList` alloué par le host — équivalent `HostAttributeList` du SDK.
/// Mutex (et pas RefCell) parce que le plugin peut théoriquement y accéder
/// depuis plusieurs threads ; en pratique tout se passe sur vst3-main.
pub struct HostAttributeList {
    attrs: Mutex<HashMap<String, Attr>>,
}

impl HostAttributeList {
    fn new() -> Self {
        Self {
            attrs: Mutex::new(HashMap::new()),
        }
    }
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

/// Copie la clé `AttrID` (cstring UTF-8/ASCII) vers une String owned.
/// `None` si pointeur null.
unsafe fn attr_key(id: *const std::ffi::c_char) -> Option<String> {
    if id.is_null() {
        return None;
    }
    Some(CStr::from_ptr(id).to_string_lossy().into_owned())
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: *const std::ffi::c_char, value: i64) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        self.attrs.lock().unwrap().insert(key, Attr::Int(value));
        kResultTrue
    }

    unsafe fn getInt(&self, id: *const std::ffi::c_char, value: *mut i64) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&key) {
            Some(Attr::Int(v)) => {
                *value = *v;
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: *const std::ffi::c_char, value: f64) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        self.attrs.lock().unwrap().insert(key, Attr::Float(value));
        kResultTrue
    }

    unsafe fn getFloat(&self, id: *const std::ffi::c_char, value: *mut f64) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&key) {
            Some(Attr::Float(v)) => {
                *value = *v;
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: *const std::ffi::c_char, string: *const char16) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if string.is_null() {
            return kInvalidArgument;
        }
        // Longueur = jusqu'au premier 0 UTF-16.
        let mut len = 0usize;
        while *string.add(len) != 0 {
            len += 1;
        }
        let v: Vec<u16> = (0..len).map(|i| *string.add(i)).collect();
        self.attrs.lock().unwrap().insert(key, Attr::Str(v));
        kResultTrue
    }

    unsafe fn getString(
        &self,
        id: *const std::ffi::c_char,
        string: *mut char16,
        size_in_bytes: uint32,
    ) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if string.is_null() || size_in_bytes < 2 {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&key) {
            Some(Attr::Str(v)) => {
                // Copie tronquée à la taille du buffer, toujours nul-terminée
                // (même sémantique que HostAttributeList::getString du SDK).
                let max_chars = (size_in_bytes as usize / 2).saturating_sub(1);
                let n = v.len().min(max_chars);
                for (i, ch) in v[..n].iter().enumerate() {
                    *string.add(i) = *ch;
                }
                *string.add(n) = 0;
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(
        &self,
        id: *const std::ffi::c_char,
        data: *const c_void,
        size_in_bytes: uint32,
    ) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if data.is_null() && size_in_bytes > 0 {
            return kInvalidArgument;
        }
        let v = if size_in_bytes == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize).to_vec()
        };
        self.attrs.lock().unwrap().insert(key, Attr::Bin(v));
        kResultTrue
    }

    unsafe fn getBinary(
        &self,
        id: *const std::ffi::c_char,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        let Some(key) = attr_key(id) else {
            return kInvalidArgument;
        };
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        match self.attrs.lock().unwrap().get(&key) {
            Some(Attr::Bin(v)) => {
                // Pointeur vers le storage interne — valide tant que l'attribut
                // n'est pas réécrit (même contrat que le SDK). Le plugin copie
                // immédiatement.
                *data = v.as_ptr() as *const c_void;
                *size_in_bytes = v.len() as uint32;
                kResultTrue
            }
            _ => {
                *data = std::ptr::null();
                *size_in_bytes = 0;
                kResultFalse
            }
        }
    }
}

// ---------- HostMessage ----------

/// `IMessage` alloué par le host — équivalent `HostMessage` du SDK.
pub struct HostMessage {
    /// Message ID, cstring nul-terminée. Le pointeur retourné par
    /// `getMessageID` reste valide jusqu'au prochain `setMessageID`
    /// (contrat SDK — usage transient par le plugin).
    id: Mutex<Vec<u8>>,
    attrs: ComWrapper<HostAttributeList>,
    attrs_ptr: ComPtr<IAttributeList>,
}

impl HostMessage {
    fn new() -> Option<Self> {
        let attrs = ComWrapper::new(HostAttributeList::new());
        let attrs_ptr = attrs.to_com_ptr::<IAttributeList>()?;
        Some(Self {
            id: Mutex::new(vec![0]),
            attrs,
            attrs_ptr,
        })
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.lock().unwrap().as_ptr() as FIDString
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let mut g = self.id.lock().unwrap();
        if id.is_null() {
            *g = vec![0];
            return;
        }
        let mut v = CStr::from_ptr(id).to_bytes().to_vec();
        v.push(0);
        *g = v;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Sans addref — même contrat que HostMessage::getAttributes du SDK.
        // L'objet reste détenu par le message (attrs/attrs_ptr keep-alive).
        let _ = &self.attrs;
        self.attrs_ptr.as_ptr()
    }
}

// ---------- MinimalHost ----------

pub struct MinimalHost;

impl Class for MinimalHost {
    type Interfaces = (IHostApplication,);
}

fn tuid_eq(a: &TUID, b: &TUID) -> bool {
    a == b
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
        for (i, ch) in chars.iter().take(n).enumerate() {
            *dst.add(i) = *ch as i16;
        }
        *dst.add(n) = 0;
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        *obj = std::ptr::null_mut();
        let cid = &*cid;
        let iid = &*iid;

        // IMessage — requis par le handshake ConnectionProxy des plugins JUCE.
        if tuid_eq(cid, &IMessage_iid) && tuid_eq(iid, &IMessage_iid) {
            let Some(msg) = HostMessage::new() else {
                return kResultFalse;
            };
            let wrapper = ComWrapper::new(msg);
            let Some(ptr) = wrapper.to_com_ptr::<IMessage>() else {
                return kResultFalse;
            };
            *obj = ptr.into_raw() as *mut c_void; // ref transférée au plugin
            return kResultTrue;
        }

        // IAttributeList standalone — certains plugins en demandent pour leur
        // propre usage (SDK HostApplication le fournit aussi).
        if tuid_eq(cid, &IAttributeList_iid) && tuid_eq(iid, &IAttributeList_iid) {
            let wrapper = ComWrapper::new(HostAttributeList::new());
            let Some(ptr) = wrapper.to_com_ptr::<IAttributeList>() else {
                return kResultFalse;
            };
            *obj = ptr.into_raw() as *mut c_void;
            return kResultTrue;
        }

        kNotImplemented
    }
}
