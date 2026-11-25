use crate::dv::{Frame, Standard};
use anyhow::{Context, Result, ensure};
use crossbeam_channel::{Receiver, Sender};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub can_record: bool,
}
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[repr(i32)]
pub enum Transport {
    Stop = 0,
    Play = 1,
    Pause = 2,
    Rewind = 3,
    Forward = 4,
    Record = 5,
    RecordPause = 6,
}
unsafe extern "C" {
    fn udvt_list(
        cb: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, i32),
        user: *mut c_void,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn udvt_open(
        id: *const c_char,
        output: i32,
        pal: i32,
        bitrate: u32,
        cb: unsafe extern "C" fn(*mut c_void, *const u8, i32),
        user: *mut c_void,
        error: *mut c_char,
        capacity: i32,
    ) -> *mut c_void;
    fn udvt_poll(h: *mut c_void, ms: i32, error: *mut c_char, capacity: i32) -> i32;
    fn udvt_transport(h: *mut c_void, cmd: i32, error: *mut c_char, capacity: i32) -> i32;
    fn udvt_write(
        h: *mut c_void,
        data: *const u8,
        len: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn udvt_finish(h: *mut c_void, error: *mut c_char, capacity: i32) -> i32;
    fn udvt_close(h: *mut c_void);
}
const CAP: i32 = 2048;
fn error(b: &[c_char]) -> String {
    unsafe { CStr::from_ptr(b.as_ptr()).to_string_lossy().into_owned() }
}
pub fn devices() -> Result<Vec<Device>> {
    unsafe extern "C" fn visit(
        u: *mut c_void,
        id: *const c_char,
        name: *const c_char,
        record: i32,
    ) {
        unsafe {
            let v = &mut *(u as *mut Vec<Device>);
            v.push(Device {
                id: CStr::from_ptr(id).to_string_lossy().into_owned(),
                name: CStr::from_ptr(name).to_string_lossy().into_owned(),
                can_record: record != 0,
            });
        }
    }
    let mut result = Vec::new();
    let mut e = [0; CAP as usize];
    let status = unsafe {
        udvt_list(
            visit,
            &mut result as *mut _ as *mut c_void,
            e.as_mut_ptr(),
            CAP,
        )
    };
    ensure!(status == 0, "{}", error(&e));
    Ok(result)
}
struct Callback {
    tx: Sender<Frame>,
    raw: Option<Sender<Vec<u8>>>,
    dropped: Arc<AtomicU64>,
}
unsafe extern "C" fn receive(u: *mut c_void, data: *const u8, len: i32) {
    let _ = std::panic::catch_unwind(|| unsafe {
        let ctx = &*(u as *const Callback);
        if len == i32::MIN && !data.is_null() {
            let message = CStr::from_ptr(data.cast()).to_string_lossy();
            crate::diagnostics::log("native", message.as_ref());
            return;
        }
        if len <= 0 || data.is_null() {
            ctx.dropped.fetch_add(
                if len < 0 {
                    u64::from(len.unsigned_abs())
                } else {
                    1
                },
                Ordering::Relaxed,
            );
            return;
        }
        let bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
        if let Some(raw) = &ctx.raw {
            if raw.try_send(bytes).is_err() {
                ctx.dropped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        let f = Frame::parse(bytes);
        match f {
            Ok(f) => {
                if ctx.tx.try_send(f).is_err() {
                    ctx.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                ctx.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}
/// Sessions are thread-affine: the worker owns their COM/CFRunLoop resources.
pub struct Session {
    h: *mut c_void,
    _callback: Box<Callback>,
    pub frames: Receiver<Frame>,
    pub chunks: Receiver<Vec<u8>>,
    pub dropped: Arc<AtomicU64>,
    _affinity: PhantomData<Rc<()>>,
}
impl Session {
    pub fn open(id: &str, output: Option<Standard>, capacity: usize) -> Result<Self> {
        Self::open_mode(
            id,
            output.is_some(),
            (output == Some(Standard::Pal)) as i32,
            0,
            capacity,
        )
    }
    pub fn open_hdv(id: &str, output: bool, bitrate: u32, capacity: usize) -> Result<Self> {
        Self::open_mode(id, output, 2, bitrate, capacity)
    }
    pub fn open_auto(id: &str, capacity: usize) -> Result<Self> {
        // Universal receivers deliver small isochronous packets rather than DV
        // frames. Budget approximately the same bytes, with room for an IRQ batch.
        let packets = if cfg!(windows) {
            capacity
        } else {
            capacity.saturating_mul(256)
        };
        Self::open_mode(id, false, 3, 0, packets)
    }
    fn open_mode(id: &str, output: bool, mode: i32, bitrate: u32, capacity: usize) -> Result<Self> {
        crate::diagnostics::log(
            "backend",
            format!(
                "Opening device={id:?} output={output} mode={mode} bitrate={bitrate} buffer={capacity}"
            ),
        );
        let (tx, frames) = crossbeam_channel::bounded(capacity);
        let (raw, chunks) = crossbeam_channel::bounded(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut callback = Box::new(Callback {
            tx,
            raw: (mode >= 2).then_some(raw),
            dropped: dropped.clone(),
        });
        let mut e = [0; CAP as usize];
        let id = CString::new(id).context("Device ID contains NUL")?;
        let h = unsafe {
            udvt_open(
                id.as_ptr(),
                output as i32,
                mode,
                bitrate,
                receive,
                &mut *callback as *mut _ as *mut c_void,
                e.as_mut_ptr(),
                CAP,
            )
        };
        if h.is_null() {
            crate::diagnostics::log("backend.error", error(&e));
        }
        ensure!(!h.is_null(), "{}", error(&e));
        crate::diagnostics::log("backend", "Native input/output session opened");
        Ok(Self {
            h,
            _callback: callback,
            frames,
            chunks,
            dropped,
            _affinity: PhantomData,
        })
    }
    pub fn poll(&self) -> Result<()> {
        let mut e = [0; CAP as usize];
        ensure!(
            unsafe { udvt_poll(self.h, 10, e.as_mut_ptr(), CAP) } == 0,
            "{}",
            error(&e)
        );
        Ok(())
    }
    pub fn transport(&self, t: Transport) -> Result<()> {
        crate::diagnostics::log("deck.command", format!("Sending {t:?}"));
        let mut e = [0; CAP as usize];
        let result = unsafe { udvt_transport(self.h, t as i32, e.as_mut_ptr(), CAP) };
        if result != 0 {
            let message = format!("Deck {t:?} command failed: {}", error(&e));
            crate::diagnostics::log("deck.error", &message);
            anyhow::bail!(message);
        }
        crate::diagnostics::log("deck.command", format!("{t:?} command accepted"));
        Ok(())
    }
    pub fn finish(&self) -> Result<()> {
        let mut e = [0; CAP as usize];
        ensure!(
            unsafe { udvt_finish(self.h, e.as_mut_ptr(), CAP) } == 0,
            "{}",
            error(&e)
        );
        Ok(())
    }
    pub fn write(&self, f: &Frame) -> Result<()> {
        self.write_bytes(&f.data)
    }
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<()> {
        let mut e = [0; CAP as usize];
        ensure!(
            unsafe {
                udvt_write(
                    self.h,
                    bytes.as_ptr(),
                    bytes
                        .len()
                        .try_into()
                        .context("Native output block is too large")?,
                    e.as_mut_ptr(),
                    CAP,
                )
            } == 0,
            "{}",
            error(&e)
        );
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe { udvt_close(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_callbacks_are_not_counted_as_media_or_losses() {
        let (tx, frames) = crossbeam_channel::bounded(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut callback = Callback {
            tx,
            raw: None,
            dropped: dropped.clone(),
        };
        let message = CString::new("Output pin: DV25").unwrap();
        unsafe {
            receive(
                &mut callback as *mut _ as *mut c_void,
                message.as_ptr().cast(),
                i32::MIN,
            );
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert!(frames.is_empty());
    }
    #[test]
    fn native_drop_counts_are_accumulated() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut callback = Callback {
            tx,
            raw: None,
            dropped: dropped.clone(),
        };
        unsafe {
            receive(&mut callback as *mut _ as *mut c_void, std::ptr::null(), -7);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 7);
    }
}
