//! Talking to CLAP plug-ins: opening their library, starting an instance, and
//! running it. CLAP is an open plug-in interface under the MIT licence; the
//! declarations come from the `clap-sys` crate.
//!
//! CLAP gives each call a thread. Everything here belongs to the plug-in
//! thread of `plugins.rs`, except `Processor`, which belongs to the sound
//! thread and is the only part that runs while sound plays.

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::log::*;
use clap_sys::ext::params::*;
use clap_sys::ext::state::*;
use clap_sys::ext::thread_check::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::stream::*;
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// One plug-in found in a library, as the scan reports it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct PluginInfo {
    pub path: String,
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub features: Vec<String>,
}

impl PluginInfo {
    /// Instruments make sound from notes, which nothing here sends.
    pub fn treats_sound(&self) -> bool {
        let has = |f: &str| self.features.iter().any(|x| x == f);
        has("audio-effect") || has("analyzer") || !has("instrument")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Other,
    Main,
    Audio,
}

thread_local! {
    static ROLE: Cell<Role> = const { Cell::new(Role::Other) };
}

/// Plug-ins may ask which thread calls them, and some refuse to work when
/// the answer is wrong.
pub fn this_thread_is_main() {
    ROLE.with(|r| r.set(Role::Main));
}

pub fn this_thread_is_audio() {
    ROLE.with(|r| r.set(Role::Audio));
}

fn text(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn fixed_text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The file the system loads: a macOS plug-in is a folder around it.
fn binary_of(path: &Path) -> PathBuf {
    if path.is_dir() {
        let stem = path.file_stem().unwrap_or_default();
        return path.join("Contents").join("MacOS").join(stem);
    }
    path.to_path_buf()
}

/// An opened plug-in library. It stays open while anything made from it runs.
pub struct Library {
    entry: *const clap_plugin_entry,
    _lib: libloading::Library,
}

// The entry point is shared by every thread by design of CLAP.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    pub fn open(path: &Path) -> Result<Arc<Library>, String> {
        let lib = unsafe { libloading::Library::new(binary_of(path)) }
            .map_err(|e| format!("impossible de charger {} : {e}", path.display()))?;
        let entry = unsafe { lib.get::<*const clap_plugin_entry>(b"clap_entry\0") }
            .map_err(|_| "ce fichier n'est pas un plug-in CLAP".to_string())?;
        let entry: *const clap_plugin_entry = *entry;
        if entry.is_null() || !clap_version_is_compatible(unsafe { (*entry).clap_version }) {
            return Err("version de CLAP non prise en charge".into());
        }
        let c_path = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let init = unsafe { (*entry).init }.ok_or("plug-in incomplet")?;
        if !unsafe { init(c_path.as_ptr()) } {
            return Err(t!("clap.refused_start").into());
        }
        Ok(Arc::new(Library { entry, _lib: lib }))
    }

    fn factory(&self) -> Option<&clap_plugin_factory> {
        let get = unsafe { (*self.entry).get_factory }?;
        let f = unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        unsafe { f.as_ref() }
    }

    pub fn plugins(&self, path: &Path) -> Vec<PluginInfo> {
        let Some(f) = self.factory() else { return Vec::new() };
        let (Some(count), Some(get)) = (f.get_plugin_count, f.get_plugin_descriptor) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for n in 0..unsafe { count(f) } {
            let Some(d) = (unsafe { get(f, n).as_ref() }) else { continue };
            let mut features = Vec::new();
            let mut p = d.features;
            while !p.is_null() && !unsafe { *p }.is_null() {
                features.push(text(unsafe { *p }));
                p = unsafe { p.add(1) };
            }
            out.push(PluginInfo {
                path: path.to_string_lossy().into_owned(),
                id: text(d.id),
                name: text(d.name),
                vendor: text(d.vendor),
                version: text(d.version),
                features,
            });
        }
        out
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Some(deinit) = unsafe { (*self.entry).deinit } {
            unsafe { deinit() };
        }
    }
}

/// Lists the plug-ins of one file. Loading runs the plug-in's own code, so
/// the program does this in a separate process when it can.
pub fn describe(path: &Path) -> Result<Vec<PluginInfo>, String> {
    let lib = Library::open(path)?;
    Ok(lib.plugins(path))
}

/// What a plug-in asked of the host. Any of its threads may ask.
#[derive(Default)]
pub struct Requests {
    pub callback: AtomicBool,
    pub restart: AtomicBool,
    /// Its settings changed: the copy in the project needs refreshing.
    pub dirty: AtomicBool,
    pub rescan: AtomicBool,
    pub flush: AtomicBool,
    pub editor_closed: AtomicBool,
    /// Width and height wanted for the editor, packed; zero for none.
    pub resize: AtomicU64,
}

#[repr(C)]
struct Shell {
    host: clap_host,
    requests: Arc<Requests>,
}

unsafe fn requests<'a>(host: *const clap_host) -> &'a Requests {
    unsafe { &*((*host).host_data as *const Requests) }
}

unsafe extern "C" fn host_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_LOG {
        &HOST_LOG as *const _ as *const c_void
    } else if id == CLAP_EXT_THREAD_CHECK {
        &HOST_THREADS as *const _ as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &HOST_STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &HOST_LATENCY as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &HOST_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_GUI && cfg!(windows) {
        &HOST_GUI as *const _ as *const c_void
    } else {
        null()
    }
}

unsafe extern "C" fn host_restart(host: *const clap_host) {
    unsafe { requests(host) }.restart.store(true, Ordering::Relaxed);
}

/// The sound thread already calls every plug-in on every block.
unsafe extern "C" fn host_process(_: *const clap_host) {}

unsafe extern "C" fn host_callback(host: *const clap_host) {
    unsafe { requests(host) }.callback.store(true, Ordering::Relaxed);
}

static HOST_LOG: clap_host_log = clap_host_log { log: Some(host_log) };

unsafe extern "C" fn host_log(_: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    if severity >= CLAP_LOG_WARNING {
        eprintln!("plug-in : {}", text(msg));
    }
}

static HOST_THREADS: clap_host_thread_check =
    clap_host_thread_check { is_main_thread: Some(on_main), is_audio_thread: Some(on_audio) };

unsafe extern "C" fn on_main(_: *const clap_host) -> bool {
    ROLE.with(|r| r.get() == Role::Main)
}

unsafe extern "C" fn on_audio(_: *const clap_host) -> bool {
    ROLE.with(|r| r.get() == Role::Audio)
}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(params_rescan),
    clear: Some(params_clear),
    request_flush: Some(params_flush),
};

unsafe extern "C" fn params_rescan(host: *const clap_host, _: clap_param_rescan_flags) {
    unsafe { requests(host) }.rescan.store(true, Ordering::Relaxed);
}

/// Nothing automates parameters here, so there is nothing to forget.
unsafe extern "C" fn params_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {}

unsafe extern "C" fn params_flush(host: *const clap_host) {
    unsafe { requests(host) }.flush.store(true, Ordering::Relaxed);
}

static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(mark_dirty) };

unsafe extern "C" fn mark_dirty(host: *const clap_host) {
    unsafe { requests(host) }.dirty.store(true, Ordering::Relaxed);
}

static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(params_rescan_latency) };

unsafe extern "C" fn params_rescan_latency(host: *const clap_host) {
    unsafe { requests(host) }.rescan.store(true, Ordering::Relaxed);
}

static HOST_PORTS: clap_host_audio_ports =
    clap_host_audio_ports { is_rescan_flag_supported: Some(no_port_flag), rescan: Some(ports_rescan) };

unsafe extern "C" fn no_port_flag(_: *const clap_host, _: u32) -> bool {
    false
}

/// New ports mean starting the plug-in again with them.
unsafe extern "C" fn ports_rescan(host: *const clap_host, _: u32) {
    unsafe { requests(host) }.restart.store(true, Ordering::Relaxed);
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_hints),
    request_resize: Some(gui_resize),
    request_show: Some(gui_answer_no),
    request_hide: Some(gui_answer_no),
    closed: Some(gui_closed),
};

unsafe extern "C" fn gui_hints(_: *const clap_host) {}

unsafe extern "C" fn gui_resize(host: *const clap_host, w: u32, h: u32) -> bool {
    let packed = ((w as u64) << 32) | h as u64;
    unsafe { requests(host) }.resize.store(packed.max(1), Ordering::Relaxed);
    true
}

unsafe extern "C" fn gui_answer_no(_: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_closed(host: *const clap_host, _: bool) {
    unsafe { requests(host) }.editor_closed.store(true, Ordering::Relaxed);
}

const HOST_NAME: &CStr = c"LiveCraft";
const HOST_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// One parameter as the plug-in describes it, with its current value.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub value: f64,
    pub text: String,
    pub stepped: bool,
    pub read_only: bool,
}

/// Channels of each audio port, inputs then outputs.
#[derive(Clone, Default, Debug)]
pub struct Ports {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub main_in: Option<usize>,
    pub main_out: Option<usize>,
}

/// A running plug-in instance.
pub struct Plugin {
    raw: *const clap_plugin,
    shell: Box<Shell>,
    _lib: Arc<Library>,
    params: *const clap_plugin_params,
    state: *const clap_plugin_state,
    latency: *const clap_plugin_latency,
    gui: *const clap_plugin_gui,
    ports: Ports,
    active: bool,
}

// CLAP decides which thread may call what; the callers keep to it.
unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

impl Plugin {
    pub fn create(lib: Arc<Library>, id: &str, requests: Arc<Requests>) -> Result<Plugin, String> {
        let factory = lib.factory().ok_or(t!("clap.offers_nothing"))?;
        let create = factory.create_plugin.ok_or("plug-in incomplet")?;
        let mut shell = Box::new(Shell {
            host: clap_host {
                clap_version: CLAP_VERSION,
                host_data: null_mut(),
                name: HOST_NAME.as_ptr(),
                vendor: HOST_NAME.as_ptr(),
                url: c"".as_ptr(),
                version: HOST_VERSION.as_ptr() as *const c_char,
                get_extension: Some(host_extension),
                request_restart: Some(host_restart),
                request_process: Some(host_process),
                request_callback: Some(host_callback),
            },
            requests,
        });
        shell.host.host_data = Arc::as_ptr(&shell.requests) as *mut c_void;
        let c_id = CString::new(id).map_err(|e| e.to_string())?;
        let raw = unsafe { create(factory, &shell.host, c_id.as_ptr()) };
        if raw.is_null() {
            return Err(t!("clap.not_in_file", id = id));
        }
        let init = unsafe { (*raw).init };
        if !init.is_some_and(|f| unsafe { f(raw) }) {
            if let Some(destroy) = unsafe { (*raw).destroy } {
                unsafe { destroy(raw) };
            }
            return Err(t!("clap.init_failed").into());
        }
        let ext = |name: &CStr| match unsafe { (*raw).get_extension } {
            Some(get) => unsafe { get(raw, name.as_ptr()) },
            None => null(),
        };
        let mut plugin = Plugin {
            raw,
            params: ext(CLAP_EXT_PARAMS) as *const _,
            state: ext(CLAP_EXT_STATE) as *const _,
            latency: ext(CLAP_EXT_LATENCY) as *const _,
            gui: if cfg!(windows) { ext(CLAP_EXT_GUI) as *const _ } else { null() },
            ports: Ports::default(),
            shell,
            _lib: lib,
            active: false,
        };
        plugin.ports = plugin.read_ports(ext(CLAP_EXT_AUDIO_PORTS) as *const clap_plugin_audio_ports);
        Ok(plugin)
    }

    fn read_ports(&self, ext: *const clap_plugin_audio_ports) -> Ports {
        let Some(ext) = (unsafe { ext.as_ref() }) else { return Ports::default() };
        let (Some(count), Some(get)) = (ext.count, ext.get) else { return Ports::default() };
        let mut ports = Ports::default();
        for input in [true, false] {
            let mut list = Vec::new();
            let mut main = None;
            for n in 0..unsafe { count(self.raw, input) } {
                let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
                if !unsafe { get(self.raw, n, input, &mut info) } {
                    continue;
                }
                if info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 && main.is_none() {
                    main = Some(list.len());
                }
                list.push(info.channel_count);
            }
            // Without a port marked main, the first one is.
            let main = main.or((!list.is_empty()).then_some(0));
            if input {
                ports.inputs = list;
                ports.main_in = main;
            } else {
                ports.outputs = list;
                ports.main_out = main;
            }
        }
        ports
    }

    pub fn activate(&mut self, rate: f64, max_frames: u32) -> Result<(), String> {
        let activate = unsafe { (*self.raw).activate }.ok_or("plug-in incomplet")?;
        if !unsafe { activate(self.raw, rate, 1, max_frames) } {
            return Err(t!("clap.refused_activate").into());
        }
        self.active = true;
        Ok(())
    }

    pub fn load_state(&self, bytes: &[u8]) -> bool {
        let Some(ext) = (unsafe { self.state.as_ref() }) else { return false };
        let Some(load) = ext.load else { return false };
        let mut reader = Reader { bytes, at: 0 };
        let stream = clap_istream { ctx: &mut reader as *mut Reader as *mut c_void, read: Some(read_bytes) };
        unsafe { load(self.raw, &stream) }
    }

    pub fn save_state(&self) -> Option<Vec<u8>> {
        let ext = unsafe { self.state.as_ref() }?;
        let save = ext.save?;
        let mut out: Vec<u8> = Vec::new();
        let stream = clap_ostream { ctx: &mut out as *mut Vec<u8> as *mut c_void, write: Some(write_bytes) };
        unsafe { save(self.raw, &stream) }.then_some(out)
    }

    /// Samples the plug-in holds the sound back by.
    pub fn latency(&self) -> u32 {
        match unsafe { self.latency.as_ref() }.and_then(|l| l.get) {
            Some(get) if self.active => unsafe { get(self.raw) },
            _ => 0,
        }
    }

    pub fn params(&self) -> Vec<Param> {
        let Some(ext) = (unsafe { self.params.as_ref() }) else { return Vec::new() };
        let (Some(count), Some(get_info)) = (ext.count, ext.get_info) else { return Vec::new() };
        let mut out = Vec::new();
        for n in 0..unsafe { count(self.raw) } {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            if !unsafe { get_info(self.raw, n, &mut info) } || info.flags & CLAP_PARAM_IS_HIDDEN != 0 {
                continue;
            }
            let mut value = info.default_value;
            if let Some(get) = ext.get_value {
                unsafe { get(self.raw, info.id, &mut value) };
            }
            out.push(Param {
                id: info.id,
                name: fixed_text(&info.name),
                min: info.min_value,
                max: info.max_value,
                value,
                text: self.value_text(info.id, value),
                stepped: info.flags & CLAP_PARAM_IS_STEPPED != 0,
                read_only: info.flags & CLAP_PARAM_IS_READONLY != 0,
            });
        }
        out
    }

    pub fn value_text(&self, id: u32, value: f64) -> String {
        let to_text = unsafe { self.params.as_ref() }.and_then(|p| p.value_to_text);
        let mut buf = [0 as c_char; 128];
        match to_text {
            Some(f) if unsafe { f(self.raw, id, value, buf.as_mut_ptr(), buf.len() as u32) } => {
                fixed_text(&buf)
            }
            _ => format!("{value:.2}"),
        }
    }

    pub fn on_main_thread(&self) {
        if let Some(f) = unsafe { (*self.raw).on_main_thread } {
            unsafe { f(self.raw) };
        }
    }

    pub fn has_editor(&self) -> bool {
        editor::supported(self).is_some()
    }

    pub fn processor(&self, max_frames: usize) -> Processor {
        Processor::new(self, max_frames)
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            if self.active
                && let Some(f) = (*self.raw).deactivate
            {
                f(self.raw);
            }
            if let Some(f) = (*self.raw).destroy {
                f(self.raw);
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

unsafe extern "C" fn read_bytes(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    let reader = unsafe { &mut *((*stream).ctx as *mut Reader) };
    let n = (size as usize).min(reader.bytes.len() - reader.at);
    unsafe { std::ptr::copy_nonoverlapping(reader.bytes[reader.at..].as_ptr(), buffer as *mut u8, n) };
    reader.at += n;
    n as i64
}

unsafe extern "C" fn write_bytes(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    let out = unsafe { &mut *((*stream).ctx as *mut Vec<u8>) };
    out.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) });
    size as i64
}

/// Parameter changes handed to the plug-in with a block.
#[repr(C)]
struct InEvents {
    list: clap_input_events,
    events: Vec<clap_event_param_value>,
}

unsafe extern "C" fn events_size(list: *const clap_input_events) -> u32 {
    unsafe { &*((*list).ctx as *const InEvents) }.events.len() as u32
}

unsafe extern "C" fn events_get(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    let events = &unsafe { &*((*list).ctx as *const InEvents) }.events;
    events.get(index as usize).map_or(null(), |e| &e.header as *const _)
}

unsafe extern "C" fn events_push(list: *const clap_output_events, event: *const clap_event_header) -> bool {
    let requests = unsafe { &*((*list).ctx as *const Requests) };
    let h = unsafe { &*event };
    // A knob turned in the plug-in's own window: the project must learn it.
    if h.space_id == CLAP_CORE_EVENT_SPACE_ID
        && (h.type_ == CLAP_EVENT_PARAM_VALUE || h.type_ == CLAP_EVENT_PARAM_GESTURE_END)
    {
        requests.dirty.store(true, Ordering::Relaxed);
    }
    true
}

/// The sound-thread side of one plug-in: its buffers, made once, so that a
/// block costs no memory and waits on nothing.
pub struct Processor {
    raw: *const clap_plugin,
    params: *const clap_plugin_params,
    started: bool,
    steady: i64,
    max_frames: usize,
    ports: Ports,
    _channels: Vec<Vec<f32>>,
    _pointers: Vec<Vec<*mut f32>>,
    inputs: Vec<clap_audio_buffer>,
    outputs: Vec<clap_audio_buffer>,
    in_events: Box<InEvents>,
    out_events: Box<clap_output_events>,
    _requests: Arc<Requests>,
}

// Moved to the sound thread once, then only used there.
unsafe impl Send for Processor {}

impl Processor {
    fn new(plugin: &Plugin, max_frames: usize) -> Processor {
        let ports = plugin.ports.clone();
        let mut channels = Vec::new();
        let mut pointers = Vec::new();
        let mut buffers = |list: &[u32]| -> Vec<clap_audio_buffer> {
            list.iter()
                .map(|count| {
                    let mut ptrs = Vec::new();
                    for _ in 0..*count {
                        let mut ch = vec![0f32; max_frames];
                        ptrs.push(ch.as_mut_ptr());
                        channels.push(ch);
                    }
                    let buf = clap_audio_buffer {
                        data32: ptrs.as_mut_ptr(),
                        data64: null_mut(),
                        channel_count: *count,
                        latency: 0,
                        constant_mask: 0,
                    };
                    pointers.push(ptrs);
                    buf
                })
                .collect()
        };
        let inputs = buffers(&ports.inputs);
        let outputs = buffers(&ports.outputs);
        let mut in_events = Box::new(InEvents {
            list: clap_input_events { ctx: null_mut(), size: Some(events_size), get: Some(events_get) },
            events: Vec::with_capacity(256),
        });
        in_events.list.ctx = &mut *in_events as *mut InEvents as *mut c_void;
        let requests = plugin.shell.requests.clone();
        let out_events = Box::new(clap_output_events {
            ctx: Arc::as_ptr(&requests) as *mut c_void,
            try_push: Some(events_push),
        });
        Processor {
            raw: plugin.raw,
            params: plugin.params,
            started: false,
            steady: 0,
            max_frames,
            ports,
            _channels: channels,
            _pointers: pointers,
            inputs,
            outputs,
            in_events,
            out_events,
            _requests: requests,
        }
    }

    fn queue(&mut self, changes: &[(u32, f64)]) {
        self.in_events.events.clear();
        for (id, value) in changes.iter().take(self.in_events.events.capacity()) {
            self.in_events.events.push(clap_event_param_value {
                header: clap_event_header {
                    size: size_of::<clap_event_param_value>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: *id,
                cookie: null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: *value,
            });
        }
    }

    /// Treats a block of interleaved stereo in place. The sound stays as it
    /// was if the plug-in refuses or fails.
    pub fn run(&mut self, block: &mut [f32], changes: &[(u32, f64)]) {
        let frames = (block.len() / 2).min(self.max_frames);
        let (Some(main_in), Some(main_out)) = (self.ports.main_in, self.ports.main_out) else {
            self.flush(changes);
            return;
        };
        if !self.started {
            let start = unsafe { (*self.raw).start_processing };
            self.started = start.is_none_or(|f| unsafe { f(self.raw) });
            if !self.started {
                return;
            }
        }
        let Some(process) = (unsafe { (*self.raw).process }) else { return };
        let input = &self.inputs[main_in];
        unsafe {
            let ch = |n: usize| std::slice::from_raw_parts_mut(*input.data32.add(n), frames);
            match input.channel_count {
                0 => {}
                1 => {
                    for (d, p) in ch(0).iter_mut().zip(block.as_chunks::<2>().0) {
                        *d = (p[0] + p[1]) * 0.5;
                    }
                }
                _ => {
                    let (left, right) = (ch(0), ch(1));
                    for (n, p) in block.as_chunks::<2>().0.iter().take(frames).enumerate() {
                        left[n] = p[0];
                        right[n] = p[1];
                    }
                }
            }
        }
        self.queue(changes);
        let p = clap_process {
            steady_time: self.steady,
            frames_count: frames as u32,
            transport: null(),
            audio_inputs: self.inputs.as_ptr(),
            audio_outputs: self.outputs.as_mut_ptr(),
            audio_inputs_count: self.inputs.len() as u32,
            audio_outputs_count: self.outputs.len() as u32,
            in_events: &self.in_events.list,
            out_events: &*self.out_events,
        };
        let status = unsafe { process(self.raw, &p) };
        self.steady += frames as i64;
        if status == CLAP_PROCESS_ERROR {
            return;
        }
        let output = &self.outputs[main_out];
        unsafe {
            let ch = |n: usize| std::slice::from_raw_parts(*output.data32.add(n), frames);
            match output.channel_count {
                0 => {}
                1 => {
                    for (p, v) in block.as_chunks_mut::<2>().0.iter_mut().zip(ch(0)) {
                        *p = [*v, *v];
                    }
                }
                _ => {
                    let (left, right) = (ch(0), ch(1));
                    for (n, p) in block.as_chunks_mut::<2>().0.iter_mut().take(frames).enumerate() {
                        *p = [left[n], right[n]];
                    }
                }
            }
        }
    }

    /// Hands parameter changes over while no sound goes through.
    pub fn flush(&mut self, changes: &[(u32, f64)]) {
        let Some(flush) = (unsafe { self.params.as_ref() }).and_then(|p| p.flush) else { return };
        self.queue(changes);
        unsafe { flush(self.raw, &self.in_events.list, &*self.out_events) };
    }

    pub fn stop(&mut self) {
        if self.started {
            if let Some(f) = unsafe { (*self.raw).stop_processing } {
                unsafe { f(self.raw) };
            }
            self.started = false;
        }
    }
}

impl Drop for Processor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The plug-in's own window, on Windows: placed inside a window of ours
/// when it can be, or floating on its own otherwise.
pub mod editor {
    use super::*;

    pub(super) fn supported(plugin: &Plugin) -> Option<bool> {
        if !cfg!(windows) {
            return None;
        }
        let gui = unsafe { plugin.gui.as_ref() }?;
        let ask = gui.is_api_supported?;
        // Inside our own window first: it stays with LiveCraft's.
        [false, true]
            .into_iter()
            .find(|floating| unsafe { ask(plugin.raw, CLAP_WINDOW_API_WIN32.as_ptr(), *floating) })
    }

    pub struct Editor {
        /// Our window around it; none when the plug-in floats its own.
        pub window: Option<isize>,
    }

    pub fn open(plugin: &Plugin, title: &str) -> Result<Editor, String> {
        let floating = supported(plugin).ok_or(t!("clap.no_window"))?;
        let gui = unsafe { &*plugin.gui };
        let create = gui.create.ok_or("plug-in incomplet")?;
        if !unsafe { create(plugin.raw, CLAP_WINDOW_API_WIN32.as_ptr(), floating) } {
            return Err(t!("clap.window_not_opened").into());
        }
        let destroy = || {
            if let Some(f) = gui.destroy {
                unsafe { f(plugin.raw) };
            }
        };
        if floating {
            if let (Some(f), Ok(t)) = (gui.suggest_title, CString::new(title)) {
                unsafe { f(plugin.raw, t.as_ptr()) };
            }
            if !gui.show.is_some_and(|f| unsafe { f(plugin.raw) }) {
                destroy();
                return Err(t!("clap.window_not_shown").into());
            }
            return Ok(Editor { window: None });
        }
        let (mut w, mut h) = (400u32, 300u32);
        if let Some(f) = gui.get_size {
            unsafe { f(plugin.raw, &mut w, &mut h) };
        }
        let Some(hwnd) = win::create_window(title, w, h) else {
            destroy();
            return Err(t!("clap.cannot_open_window").into());
        };
        let parent = clap_window {
            api: CLAP_WINDOW_API_WIN32.as_ptr(),
            specific: clap_window_handle { win32: hwnd as _ },
        };
        if !gui.set_parent.is_some_and(|f| unsafe { f(plugin.raw, &parent) }) {
            destroy();
            win::destroy_window(hwnd);
            return Err(t!("clap.cannot_embed").into());
        }
        if let Some(f) = gui.show {
            unsafe { f(plugin.raw) };
        }
        win::show_window(hwnd);
        Ok(Editor { window: Some(hwnd) })
    }

    pub fn close(plugin: &Plugin, editor: Editor) {
        let gui = unsafe { &*plugin.gui };
        if let Some(f) = gui.hide {
            unsafe { f(plugin.raw) };
        }
        if let Some(f) = gui.destroy {
            unsafe { f(plugin.raw) };
        }
        if let Some(hwnd) = editor.window {
            win::destroy_window(hwnd);
        }
    }

    /// The plug-in asked for another size.
    pub fn resize(plugin: &Plugin, editor: &Editor, w: u32, h: u32) {
        if let Some(hwnd) = editor.window {
            win::resize_window(hwnd, w, h);
            if let Some(f) = unsafe { &*plugin.gui }.set_size {
                unsafe { f(plugin.raw, w, h) };
            }
        }
    }

    /// Windows of ours the user closed since the last call.
    pub fn closed() -> Vec<isize> {
        win::take_closed()
    }

    /// Plug-in windows live on the plug-in thread, which must hand them
    /// their messages.
    pub fn pump() {
        win::pump();
    }

    #[cfg(windows)]
    mod win {
        use std::cell::RefCell;
        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;

        thread_local! {
            static CLOSED: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
        }

        const CLASS: &str = "LiveCraftPluginEditor\0";

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if msg == WM_CLOSE {
                // The plug-in lets go of its part before the window goes.
                CLOSED.with(|c| c.borrow_mut().push(hwnd as isize));
                return 0;
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }

        fn outer_size(w: u32, h: u32) -> (i32, i32) {
            let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            unsafe { AdjustWindowRectEx(&mut r, WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, 0, 0) };
            (r.right - r.left, r.bottom - r.top)
        }

        pub fn create_window(title: &str, w: u32, h: u32) -> Option<isize> {
            let class: Vec<u16> = CLASS.encode_utf16().collect();
            let module = unsafe { GetModuleHandleW(std::ptr::null()) };
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: module,
                hIcon: std::ptr::null_mut(),
                hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            // Registering twice fails harmlessly.
            unsafe { RegisterClassW(&wc) };
            let (ow, oh) = outer_size(w, h);
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    wide(title).as_ptr(),
                    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    ow,
                    oh,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    module,
                    std::ptr::null(),
                )
            };
            (!hwnd.is_null()).then_some(hwnd as isize)
        }

        pub fn show_window(hwnd: isize) {
            unsafe { ShowWindow(hwnd as HWND, SW_SHOWNORMAL) };
        }

        pub fn destroy_window(hwnd: isize) {
            unsafe { DestroyWindow(hwnd as HWND) };
        }

        pub fn resize_window(hwnd: isize, w: u32, h: u32) {
            let (ow, oh) = outer_size(w, h);
            unsafe {
                SetWindowPos(hwnd as HWND, std::ptr::null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER)
            };
        }

        pub fn take_closed() -> Vec<isize> {
            CLOSED.with(|c| std::mem::take(&mut *c.borrow_mut()))
        }

        pub fn pump() {
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    #[cfg(not(windows))]
    mod win {
        pub fn create_window(_: &str, _: u32, _: u32) -> Option<isize> {
            None
        }
        pub fn show_window(_: isize) {}
        pub fn destroy_window(_: isize) {}
        pub fn resize_window(_: isize, _: u32, _: u32) {}
        pub fn take_closed() -> Vec<isize> {
            Vec::new()
        }
        pub fn pump() {}
    }
}
