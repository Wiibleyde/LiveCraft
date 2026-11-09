//! A tiny CLAP effect for LiveCraft's own tests: one gain parameter in
//! decibels, a fixed latency it really adds, a saved state, and on Windows a
//! small editor. The tests build it the first time they need it.
//!
//! Written by hand against the CLAP interface as `clap-sys` describes it.

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::params::*;
use clap_sys::ext::state::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::stream::*;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null;

/// Samples the sound is held back by, so a host can show and check it.
const LATENCY: usize = 32;
const GAIN: clap_id = 0;
const MIN_DB: f64 = -60.0;
const MAX_DB: f64 = 12.0;

struct Pointers<const N: usize>([*const c_char; N]);
// Only ever points at string literals.
unsafe impl<const N: usize> Sync for Pointers<N> {}

static FEATURES: Pointers<3> = Pointers([c"audio-effect".as_ptr(), c"utility".as_ptr(), null()]);

static DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.livecraft.test-gain".as_ptr(),
    name: c"LiveCraft Test Gain".as_ptr(),
    vendor: c"LiveCraft".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1.0.0".as_ptr(),
    description: c"Gain for tests".as_ptr(),
    features: FEATURES.0.as_ptr(),
};

#[repr(C)]
struct Gain {
    // First, so a pointer to the plug-in is a pointer to the whole.
    plugin: clap_plugin,
    gain_db: f64,
    delay: [[f32; LATENCY]; 2],
    pos: usize,
    editor: isize,
}

unsafe fn this<'a>(plugin: *const clap_plugin) -> &'a mut Gain {
    unsafe { &mut *((*plugin).plugin_data as *mut Gain) }
}

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    drop(unsafe { Box::from_raw((*plugin).plugin_data as *mut Gain) });
}

unsafe extern "C" fn activate(plugin: *const clap_plugin, _: f64, _: u32, _: u32) -> bool {
    let g = unsafe { this(plugin) };
    g.delay = [[0.0; LATENCY]; 2];
    g.pos = 0;
    true
}

unsafe extern "C" fn nothing(_: *const clap_plugin) {}

unsafe extern "C" fn start_processing(_: *const clap_plugin) -> bool {
    true
}

unsafe fn read_events(g: &mut Gain, events: *const clap_input_events) {
    if events.is_null() {
        return;
    }
    let list = unsafe { &*events };
    let (Some(size), Some(get)) = (list.size, list.get) else { return };
    for n in 0..unsafe { size(events) } {
        let header = unsafe { get(events, n) };
        if header.is_null() {
            continue;
        }
        let h = unsafe { &*header };
        if h.space_id == CLAP_CORE_EVENT_SPACE_ID && h.type_ == CLAP_EVENT_PARAM_VALUE {
            let e = unsafe { &*(header as *const clap_event_param_value) };
            if e.param_id == GAIN {
                g.gain_db = e.value.clamp(MIN_DB, MAX_DB);
            }
        }
    }
}

unsafe extern "C" fn process(plugin: *const clap_plugin, p: *const clap_process) -> clap_process_status {
    let g = unsafe { this(plugin) };
    let p = unsafe { &*p };
    unsafe { read_events(g, p.in_events) };
    if p.audio_inputs_count < 1 || p.audio_outputs_count < 1 {
        return CLAP_PROCESS_ERROR;
    }
    let (input, output) = unsafe { (&*p.audio_inputs, &*p.audio_outputs) };
    let gain = 10f64.powf(g.gain_db / 20.0) as f32;
    let frames = p.frames_count as usize;
    let start = g.pos;
    for ch in 0..2.min(input.channel_count as usize).min(output.channel_count as usize) {
        let (src, dst) = unsafe { (*input.data32.add(ch), *output.data32.add(ch)) };
        let mut pos = start;
        for n in 0..frames {
            let v = unsafe { *src.add(n) };
            let out = g.delay[ch][pos];
            g.delay[ch][pos] = v * gain;
            unsafe { *dst.add(n) = out };
            pos = (pos + 1) % LATENCY;
        }
    }
    g.pos = (start + frames) % LATENCY;
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn get_extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_PARAMS {
        &PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &LATENCY_EXT as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_GUI && cfg!(windows) {
        &GUI as *const _ as *const c_void
    } else {
        null()
    }
}

static PORTS: clap_plugin_audio_ports =
    clap_plugin_audio_ports { count: Some(port_count), get: Some(port_get) };

unsafe extern "C" fn port_count(_: *const clap_plugin, _: bool) -> u32 {
    1
}

unsafe extern "C" fn port_get(
    _: *const clap_plugin,
    index: u32,
    _: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if index != 0 {
        return false;
    }
    let info = unsafe { &mut *info };
    info.id = 0;
    write_name(&mut info.name, "Main");
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = 0;
    true
}

fn write_name(to: &mut [c_char], text: &str) {
    to.fill(0);
    let room = to.len() - 1;
    for (d, s) in to.iter_mut().zip(text.bytes().take(room)) {
        *d = s as c_char;
    }
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(param_count),
    get_info: Some(param_info),
    get_value: Some(param_value),
    value_to_text: Some(value_to_text),
    text_to_value: Some(text_to_value),
    flush: Some(flush),
};

unsafe extern "C" fn param_count(_: *const clap_plugin) -> u32 {
    1
}

unsafe extern "C" fn param_info(_: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    if index != 0 {
        return false;
    }
    let info = unsafe { &mut *info };
    info.id = GAIN;
    info.flags = CLAP_PARAM_IS_AUTOMATABLE;
    info.cookie = std::ptr::null_mut();
    write_name(&mut info.name, "Gain");
    write_name(&mut info.module, "");
    info.min_value = MIN_DB;
    info.max_value = MAX_DB;
    info.default_value = 0.0;
    true
}

unsafe extern "C" fn param_value(plugin: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
    if id != GAIN {
        return false;
    }
    unsafe { *out = this(plugin).gain_db };
    true
}

unsafe extern "C" fn value_to_text(
    _: *const clap_plugin,
    id: clap_id,
    value: f64,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    if id != GAIN || capacity == 0 {
        return false;
    }
    let text = format!("{value:.1} dB");
    let to = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
    write_name(to, &text);
    true
}

unsafe extern "C" fn text_to_value(
    _: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    match text.trim_end_matches("dB").trim().parse::<f64>() {
        Ok(v) if id == GAIN => {
            unsafe { *out = v };
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn flush(
    plugin: *const clap_plugin,
    input: *const clap_input_events,
    _: *const clap_output_events,
) {
    unsafe { read_events(this(plugin), input) };
}

static STATE: clap_plugin_state = clap_plugin_state { save: Some(save), load: Some(load) };

unsafe extern "C" fn save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let bytes = unsafe { this(plugin) }.gain_db.to_le_bytes();
    let mut done = 0;
    while done < bytes.len() {
        let Some(write) = (unsafe { (*stream).write }) else { return false };
        let n =
            unsafe { write(stream, bytes[done..].as_ptr() as *const c_void, (bytes.len() - done) as u64) };
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    true
}

unsafe extern "C" fn load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    let mut bytes = [0u8; 8];
    let mut done = 0;
    while done < bytes.len() {
        let Some(read) = (unsafe { (*stream).read }) else { return false };
        let n =
            unsafe { read(stream, bytes[done..].as_mut_ptr() as *mut c_void, (bytes.len() - done) as u64) };
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    unsafe { this(plugin) }.gain_db = f64::from_le_bytes(bytes).clamp(MIN_DB, MAX_DB);
    true
}

static LATENCY_EXT: clap_plugin_latency = clap_plugin_latency { get: Some(latency) };

unsafe extern "C" fn latency(_: *const clap_plugin) -> u32 {
    LATENCY as u32
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_supported),
    get_preferred_api: None,
    create: Some(gui_supported),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_scale),
    get_size: Some(gui_size),
    can_resize: Some(gui_fixed),
    get_resize_hints: None,
    adjust_size: Some(gui_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_parent),
    set_transient: None,
    suggest_title: None,
    show: Some(gui_done),
    hide: Some(gui_done),
};

/// Embedded in the host's window only.
unsafe extern "C" fn gui_supported(_: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
    !floating && unsafe { CStr::from_ptr(api) } == CLAP_WINDOW_API_WIN32
}

unsafe extern "C" fn gui_scale(_: *const clap_plugin, _: f64) -> bool {
    true
}

unsafe extern "C" fn gui_done(_: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn gui_fixed(_: *const clap_plugin) -> bool {
    false
}

unsafe extern "C" fn gui_size(_: *const clap_plugin, w: *mut u32, h: *mut u32) -> bool {
    unsafe {
        *w = 320;
        *h = 120;
    }
    true
}

unsafe extern "C" fn gui_set_size(_: *const clap_plugin, w: u32, h: u32) -> bool {
    w == 320 && h == 120
}

#[cfg(windows)]
unsafe extern "C" fn gui_parent(plugin: *const clap_plugin, window: *const clap_window) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WS_CHILD, WS_VISIBLE};
    let parent = unsafe { (*window).specific.win32 };
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let text: Vec<u16> = "LiveCraft Test Gain\0".encode_utf16().collect();
    let child = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            320,
            120,
            parent,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            null(),
        )
    };
    unsafe { this(plugin) }.editor = child as isize;
    !child.is_null()
}

#[cfg(not(windows))]
unsafe extern "C" fn gui_parent(_: *const clap_plugin, _: *const clap_window) -> bool {
    false
}

unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
    let g = unsafe { this(plugin) };
    #[cfg(windows)]
    if g.editor != 0 {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(g.editor as _) };
    }
    g.editor = 0;
}

unsafe extern "C" fn create_plugin(
    _: *const clap_plugin_factory,
    _: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != unsafe { CStr::from_ptr(DESC.id) } {
        return null();
    }
    let gain = Box::new(Gain {
        plugin: clap_plugin {
            desc: &DESC,
            plugin_data: std::ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(nothing),
            start_processing: Some(start_processing),
            stop_processing: Some(nothing),
            reset: Some(nothing),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(nothing),
        },
        gain_db: 0.0,
        delay: [[0.0; LATENCY]; 2],
        pos: 0,
        editor: 0,
    });
    let raw = Box::into_raw(gain);
    unsafe {
        (*raw).plugin.plugin_data = raw as *mut c_void;
        &(*raw).plugin
    }
}

unsafe extern "C" fn plugin_count(_: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn descriptor(_: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    if index == 0 { &DESC } else { null() }
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(plugin_count),
    get_plugin_descriptor: Some(descriptor),
    create_plugin: Some(create_plugin),
};

unsafe extern "C" fn entry_init(_: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn get_factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const _ as *const c_void
    } else {
        null()
    }
}

#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(get_factory),
};
