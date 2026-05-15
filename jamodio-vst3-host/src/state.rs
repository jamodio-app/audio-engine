//! `MemoryStream` minimal implémentant `IBStream` Steinberg.
//!
//! Utilisé pour le state sync component → controller en VST3 :
//! 1. `IComponent::getState(stream)` écrit l'état dans le stream
//! 2. seek(0)
//! 3. `IEditController::setComponentState(stream)` charge l'état dans le controller
//!
//! Sans ça, les plugins en architecture "separate component+controller"
//! (Valhalla, FabFilter, NI…) refusent leur `createView` car le controller
//! ne sait pas quels params le composant audio a actuellement.

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::sync::Mutex;

use vst3::{
    Class,
    Steinberg::{
        int32, int64, kResultFalse, kResultOk, tresult, IBStream, IBStreamTrait,
        IBStream_::IStreamSeekMode_,
    },
};

pub struct MemoryStream {
    inner: Mutex<MemStreamInner>,
}

struct MemStreamInner {
    buf: Vec<u8>,
    pos: usize,
}

impl MemoryStream {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MemStreamInner {
                buf: Vec::new(),
                pos: 0,
            }),
        }
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_read: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes <= 0 {
            if !num_bytes_read.is_null() {
                *num_bytes_read = 0;
            }
            return kResultOk;
        }
        let mut s = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        let available = s.buf.len().saturating_sub(s.pos);
        let to_read = (num_bytes as usize).min(available);
        if to_read > 0 {
            std::ptr::copy_nonoverlapping(s.buf.as_ptr().add(s.pos), buffer as *mut u8, to_read);
            s.pos += to_read;
        }
        if !num_bytes_read.is_null() {
            *num_bytes_read = to_read as int32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes <= 0 {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kResultOk;
        }
        let mut s = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        let required = s.pos + num_bytes as usize;
        if s.buf.len() < required {
            s.buf.resize(required, 0);
        }
        std::ptr::copy_nonoverlapping(
            buffer as *const u8,
            s.buf.as_mut_ptr().add(s.pos),
            num_bytes as usize,
        );
        s.pos += num_bytes as usize;
        if !num_bytes_written.is_null() {
            *num_bytes_written = num_bytes;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut s = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        let buf_len = s.buf.len() as int64;
        let new_pos = match mode as u32 {
            x if x == IStreamSeekMode_::kIBSeekSet as u32 => pos,
            x if x == IStreamSeekMode_::kIBSeekCur as u32 => s.pos as int64 + pos,
            x if x == IStreamSeekMode_::kIBSeekEnd as u32 => buf_len + pos,
            _ => return kResultFalse,
        };
        if new_pos < 0 {
            return kResultFalse;
        }
        // Seek au-delà du buffer = autorisé (= les writes suivants étendront).
        s.pos = new_pos as usize;
        if !result.is_null() {
            *result = new_pos;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kResultFalse;
        }
        let s = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        *pos = s.pos as int64;
        kResultOk
    }
}
