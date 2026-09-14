//! Conversion `CFString` → `String` (macOS), partagée par les lectures CoreAudio
//! (`audio::declared_latency`) et SystemConfiguration (`net_interface`).

use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringGetCString, CFStringGetCStringPtr, CFStringRef,
};
use std::ffi::CStr;
use std::os::raw::c_char;

/// Texte d'une `CFString` (nulle → `None`). Ne libère pas la chaîne : la règle
/// Get/Copy de CoreFoundation reste à la charge de l'appelant.
pub(crate) fn to_string(cf: CFStringRef) -> Option<String> {
    if cf.is_null() {
        return None;
    }
    // SAFETY : `cf` est une CFString non nulle ; le pointeur rapide, s'il existe,
    // vit aussi longtemps qu'elle ; sinon copie dans un tampon local terminé par 0.
    unsafe {
        let fast = CFStringGetCStringPtr(cf, kCFStringEncodingUTF8);
        if !fast.is_null() {
            return CStr::from_ptr(fast).to_str().ok().map(str::to_owned);
        }
        let mut buf = [0 as c_char; 512];
        if CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as _, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        CStr::from_ptr(buf.as_ptr())
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}
