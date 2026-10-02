//! Adresses brutes de la pile d'appels, pour les traces de plantage.
//!
//! Les binaires publiés ne contiennent pas les noms de fonctions : retirés à la
//! compilation (macOS) ou jamais embarqués (Windows), ils vivent dans un fichier
//! de symboles PRIVÉ archivé avec chaque release (`.dSYM` macOS, `.pdb` Windows).
//! Une trace Rust ne peut donc pas les afficher chez le musicien. On journalise
//! à la place les adresses brutes et l'adresse de chargement du programme :
//! avec le fichier privé, elles suffisent à retrouver chaque fonction.
//!
//! Appelé UNIQUEMENT depuis le crochet de panique : jamais dans le chemin audio.

use std::ffi::c_void;
use std::fmt::Write as _;

const MAX_FRAMES: usize = 64;

/// Pile capturée : adresse de chargement de l'exécutable + adresses de retour.
pub struct RawTrace {
    pub image_base: usize,
    pub frames: Vec<usize>,
}

impl RawTrace {
    /// `None` sur une plateforme non supportée (l'agent ne vise que macOS et Windows).
    pub fn capture() -> Option<Self> {
        let mut buf = [std::ptr::null_mut::<c_void>(); MAX_FRAMES];
        let n = imp::capture(&mut buf)?;
        Some(Self {
            image_base: imp::image_base(),
            frames: buf[..n].iter().map(|p| *p as usize).collect(),
        })
    }

    /// `0x…,0x…` — à symboliser avec le fichier privé de la même version.
    pub fn frames_hex(&self) -> String {
        let mut s = String::with_capacity(self.frames.len() * 19);
        for (i, f) in self.frames.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "{f:#x}");
        }
        s
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_int, c_void};

    extern "C" {
        // libSystem (execinfo.h, mach-o/dyld.h) — toujours lié.
        fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
        fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    }

    // Adresse de __TEXT d'un exécutable arm64 macOS avant décalage ASLR.
    const DEFAULT_TEXT_BASE: usize = 0x1_0000_0000;

    pub fn capture(buf: &mut [*mut c_void]) -> Option<usize> {
        // SAFETY : `buf` est valide pour `buf.len()` pointeurs ; backtrace(3)
        // n'écrit jamais au-delà de `size`.
        let n = unsafe { backtrace(buf.as_mut_ptr(), buf.len() as c_int) };
        Some(n.max(0) as usize)
    }

    pub fn image_base() -> usize {
        // SAFETY : image 0 = l'exécutable principal, toujours chargé.
        let slide = unsafe { _dyld_get_image_vmaddr_slide(0) };
        DEFAULT_TEXT_BASE.wrapping_add(slide as usize)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    pub fn capture(buf: &mut [*mut c_void]) -> Option<usize> {
        // SAFETY : `buf` est valide pour `buf.len()` pointeurs, que la fonction
        // ne dépasse pas ; le hachage est facultatif (pointeur nul accepté).
        let n = unsafe {
            RtlCaptureStackBackTrace(0, buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut())
        };
        Some(n as usize)
    }

    pub fn image_base() -> usize {
        // SAFETY : nom nul = module de l'exécutable courant, toujours chargé.
        unsafe { GetModuleHandleW(std::ptr::null()) as usize }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::ffi::c_void;
    pub fn capture(_: &mut [*mut c_void]) -> Option<usize> {
        None
    }
    pub fn image_base() -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_rend_des_adresses_dans_l_executable() {
        let t = RawTrace::capture().expect("macOS et Windows sont supportés");
        assert!(t.image_base != 0, "adresse de chargement inconnue");
        assert!(!t.frames.is_empty(), "pile vide");
        // La première adresse est dans ce code, donc au-delà du début de l'image.
        assert!(t.frames.iter().any(|&f| f > t.image_base));
        let hex = t.frames_hex();
        assert!(hex.starts_with("0x") && hex.split(',').count() == t.frames.len());
    }
}
