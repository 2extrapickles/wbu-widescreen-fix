//! Wakeboarding Unleashed Widescreen Fix: a d3d8.dll proxy for Wakeboarding Unleashed (PC, Aspyr 2003).
//!
//! Native resolution, borderless window with instant alt-tab, Hor+ field of view and a frame cap.
//! Addresses come from reverse engineering WBU.EXE. Every patch is skipped if the
//! exe does not match the expected bytes, so a different build just runs unmodified.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE, HWND};
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExA, GetSystemMetrics, HMENU, SM_CXSCREEN, SM_CYSCREEN, SetProcessDPIAware, WS_POPUP, WS_VISIBLE,
};

// ------------------------------------------------------------------ WBU.EXE addresses (image base 0x400000)

/// The compiler duplicated the res= lookup table into each function that reads config.cfg:
/// (widths u32[4] {640, 800, 1024, 1280}, heights u32[4] {480, 600, 768, 1024}).
const RES_TABLES: [(usize, usize); 4] = [(0x59ead8, 0x59eae8), (0x59eb2c, 0x59eb3c), (0x59eb4c, 0x59eb5c), (0x59eb6c, 0x59eb7c)];
const IAT_CREATE_WINDOW: usize = 0x59e1d0; // import slot for USER32!CreateWindowExA
/// f32 0.99 multiplying tan(hfov/2) in both camera routines (0x52729e main camera, 0x54a0ec view
/// frustum). The game keeps hfov fixed and derives the vertical FOV from W/H (Vert-):
/// tan(vfov/2) = tan(hfov/2) * 0.99 * k * H / (W * 0.9 * 7/6).
const HFOV_FACTOR: usize = 0x5c5a2c;
/// Shrinks a loaded model file in place and aborts with "realloc relocated memory block!" when the
/// heap moves it, which modern Windows may do (crash on level change / quit). Skipping the shrink is
/// safe: the block is freed whole later. The trainer helper applies the same one-byte fix.
const SHRINK_LOADED_FILE: usize = 0x4eb260;
const SHRINK_PROLOGUE: [u8; 8] = [0x8b, 0x44, 0x24, 0x04, 0x8b, 0x4c, 0x24, 0x08];
const GAME_CLASS: &[u8] = b"Wakeboarding Unleashed\0";

// IDirect3D8 / IDirect3DDevice8 vtable slots.
const D3D_CREATE_DEVICE: usize = 15;
const DEV_RESET: usize = 14;
const DEV_PRESENT: usize = 15;
const DEV_SET_TRANSFORM: usize = 37;
const DEV_DRAW_PRIMITIVE: usize = 70;
const DEV_DRAW_INDEXED: usize = 71;
const DEV_DRAW_UP: usize = 72;
const DEV_DRAW_INDEXED_UP: usize = 73;
const DEV_SET_VERTEX_SHADER: usize = 76;
const DEV_CREATE_TEXTURE: usize = 20;
const DEV_CLEAR: usize = 36;
const DEV_SET_VIEWPORT: usize = 40;
const DEV_SET_RENDER_STATE: usize = 50;
const DEV_SET_TEXTURE: usize = 61;
const DEV_SET_STREAM_SOURCE: usize = 83;
const D3DFVF_XYZRHW: u32 = 0x004;
const D3DFVF_POSITION_MASK: u32 = 0x00E;
const D3DTS_PROJECTION: u32 = 3;
const D3DFMT_X8R8G8B8: u32 = 22;

// ------------------------------------------------------------------ config + log

struct Config {
    width: u32,
    height: u32,
    borderless: bool,
    fov_fix: bool,
    hud_fix: bool,
    /// HUD placement on wide screens: true = anchored to the screen edges, false = 4:3 column in the middle.
    hud_edges: bool,
    fps_limit: u32,
    log_projection: bool,
    /// Frame numbers to save as BMP files next to the exe (debugging; only the game's own image).
    debug_shots: Vec<u32>,
    /// Frame whose draw calls are summarised in the log (debugging); 0 = off.
    log_draws: u32,
    /// F12 saves the current frame and logs every screen-space draw in it (debugging).
    debug_keys: bool,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG: Mutex<()> = Mutex::new(());

fn cfg() -> &'static Config {
    CONFIG.get().expect("config loaded in DllMain")
}

fn log(msg: impl AsRef<str>) {
    let _g = LOG.lock();
    if let Some(dir) = DIR.get()
        && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join("wbu-fix.log"))
    {
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}

const DEFAULT_INI: &str = "\
; wbu-fix settings. Delete this file to restore defaults.
[wbu-fix]
; 0 = use the desktop resolution
width=0
height=0
; 1 = borderless window covering the screen (instant alt-tab), 0 = normal window
borderless=1
; 1 = widen the 3D view for widescreen instead of cropping it (Hor+)
fov_fix=1
; 1 = keep the HUD, menus, loading screens and movies at their 4:3 proportions instead of stretched
hud_fix=1
; in-game HUD placement on wide screens: edges = side elements move out to the screen edges,
; center = the whole HUD stays in the 4:3 area in the middle (like the original screen)
hud_layout=edges
; frames per second cap (the original game ran vsynced); 0 = uncapped
fps_limit=60
; 1 = write projection matrices to wbu-fix.log (debugging)
log_projection=0
; comma-separated frame numbers to save as wbu-shot-<frame>.bmp (debugging), e.g. 300,900
debug_shots=
; 1 = F12 saves the current frame and logs how it was drawn to wbu-fix.log (debugging)
debug_keys=0
";

fn load_config(dir: &PathBuf) -> Config {
    let path = dir.join("wbu-fix.ini");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        let _ = std::fs::write(&path, DEFAULT_INI);
        DEFAULT_INI.to_string()
    });
    let get = |key: &str, default: u32| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(default)
    };
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) };
    let (w, h) = (get("width", 0), get("height", 0));
    Config {
        width: if w == 0 { sw } else { w },
        height: if h == 0 { sh } else { h },
        borderless: get("borderless", 1) != 0,
        fov_fix: get("fov_fix", 1) != 0,
        hud_fix: get("hud_fix", 1) != 0,
        hud_edges: !text
            .lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("hud_layout"))
            .is_some_and(|(_, v)| v.trim().eq_ignore_ascii_case("center") || v.trim().eq_ignore_ascii_case("centre")),
        fps_limit: get("fps_limit", 60),
        log_projection: get("log_projection", 0) != 0,
        debug_shots: text
            .lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("debug_shots"))
            .map(|(_, v)| v.split(',').filter_map(|n| n.trim().parse().ok()).collect())
            .unwrap_or_default(),
        log_draws: get("log_draws", 0),
        debug_keys: get("debug_keys", 0) != 0,
    }
}

// ------------------------------------------------------------------ memory helpers

unsafe fn write_protected<T: Copy>(addr: usize, value: T) {
    unsafe {
        let mut old = 0;
        VirtualProtect(addr as *const c_void, size_of::<T>(), PAGE_EXECUTE_READWRITE, &mut old);
        (addr as *mut T).write_unaligned(value);
        VirtualProtect(addr as *const c_void, size_of::<T>(), old, &mut old);
    }
}

/// Replace slot `index` of a COM object's vtable, returning the original function pointer.
unsafe fn hook_vtable(obj: *mut c_void, index: usize, hook: usize) -> usize {
    unsafe {
        let slot = (*(obj as *const *mut usize)).add(index);
        let original = *slot;
        if original != hook {
            write_protected(slot as usize, hook);
        }
        original
    }
}

fn exe_matches() -> bool {
    let read = |a: usize| unsafe { std::ptr::read_unaligned(a as *const [u32; 4]) };
    RES_TABLES.iter().all(|&(w, h)| read(w) == [640, 800, 1024, 1280] && read(h) == [480, 600, 768, 1024])
}

// ------------------------------------------------------------------ window

unsafe extern "system" fn hook_create_window(
    ex_style: u32,
    class: *const u8,
    title: *const u8,
    mut style: u32,
    mut x: i32,
    mut y: i32,
    mut w: i32,
    mut h: i32,
    parent: HWND,
    menu: HMENU,
    inst: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let c = cfg();
    let is_game = (class as usize) > 0xFFFF && unsafe { std::ffi::CStr::from_ptr(class.cast()) }.to_bytes_with_nul() == GAME_CLASS;
    let mut ex = ex_style;
    if is_game {
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if c.borderless {
            style = WS_POPUP | WS_VISIBLE;
            ex = 0;
            (w, h) = (c.width as i32, c.height as i32);
            // Fill the screen, or centre a smaller custom size.
            (x, y) = ((sw - w).max(0) / 2, (sh - h).max(0) / 2);
        }
        let (gw, gh) = unsafe { (*(0x5c0040 as *const u32), *(0x5c0044 as *const u32)) };
        log(format!("window: {w}x{h} at {x},{y} style {style:#x}; game resolution globals {gw}x{gh}"));
    }
    unsafe { CreateWindowExA(ex, class, title, style, x, y, w, h, parent, menu, inst, param) }
}

// ------------------------------------------------------------------ Direct3D 8

static REAL_CREATE: AtomicUsize = AtomicUsize::new(0);
static ORIG_CREATE_DEVICE: AtomicUsize = AtomicUsize::new(0);
static ORIG_RESET: AtomicUsize = AtomicUsize::new(0);
static ORIG_PRESENT: AtomicUsize = AtomicUsize::new(0);
static ORIG_SET_TRANSFORM: AtomicUsize = AtomicUsize::new(0);
static ORIG_DRAW: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
static ORIG_SET_VS: AtomicUsize = AtomicUsize::new(0);
static ORIG_CREATE_TEXTURE: AtomicUsize = AtomicUsize::new(0);
static ORIG_CLEAR: AtomicUsize = AtomicUsize::new(0);
static ORIG_SET_VIEWPORT: AtomicUsize = AtomicUsize::new(0);
/// True while the game's HUD camera is the active projection (see `is_hud_projection`).
static HUD_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// D3DVIEWPORT8: x, y, width, height, min z, max z.
#[repr(C)]
struct Viewport {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    min_z: f32,
    max_z: f32,
}

fn full_viewport() -> Viewport {
    let c = cfg();
    Viewport { x: 0, y: 0, w: c.width, h: c.height, min_z: 0.0, max_z: 1.0 }
}

/// The 4:3 area in the middle of the screen, where the original HUD layout lives.
fn hud_viewport() -> Viewport {
    let c = cfg();
    let w = (c.height * 4).div_ceil(3).min(c.width);
    Viewport { x: (c.width - w) / 2, y: 0, w, h: c.height, min_z: 0.0, max_z: 1.0 }
}

unsafe fn set_viewport_raw(dev: *mut c_void, vp: &Viewport) {
    let orig: unsafe extern "system" fn(*mut c_void, *const Viewport) -> i32 = unsafe { std::mem::transmute(ORIG_SET_VIEWPORT.load(Relaxed)) };
    let hr = unsafe { orig(dev, vp) };
    if dumping() {
        log(format!("  [wbu-fix] SetViewport {} {} {} {} -> {hr:#x}", vp.x, vp.y, vp.w, vp.h));
    }
}

/// Current viewport as the device reports it (debug dumps only).
unsafe fn current_viewport(dev: *mut c_void) -> [u32; 6] {
    let mut v = [0u32; 6];
    unsafe {
        let get: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(*(*(dev as *const *const usize)).add(41));
        get(dev, v.as_mut_ptr());
    }
    v
}

/// The in-game HUD (panels, meters, tips) is 3D geometry drawn through its own camera, recognisable
/// by its depth terms (near plane 4: _33 = 1.020, _43 = -4.082). Measured from an in-run frame dump.
fn is_hud_projection(p: &[f32; 16]) -> bool {
    (p[11] - 1.0).abs() < 1e-4 && (p[10] - 1.0204).abs() < 0.002 && (p[14] + 4.0816).abs() < 0.01
}
static ORIG_SET_RENDER_STATE: AtomicUsize = AtomicUsize::new(0);
const D3DRS_CLIPPING: u32 = 136;
/// The game's own D3DRS_CLIPPING value (it may turn clipping off; the HUD viewport needs it on).
static GAME_CLIPPING: AtomicU32 = AtomicU32::new(1);

unsafe fn set_render_state_raw(dev: *mut c_void, state: u32, value: u32) {
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32) -> i32 = unsafe { std::mem::transmute(ORIG_SET_RENDER_STATE.load(Relaxed)) };
    unsafe { orig(dev, state, value) };
}

/// The HUD camera's projection as the game set it (Hor+ after our FOV patch) and its 4:3 version.
static HUD_PROJ: Mutex<([f32; 16], [f32; 16])> = Mutex::new(([0.0; 16], [0.0; 16]));
/// X and Z translation of the current view matrix (f32 bits): where a HUD element sits.
static VIEW_X: AtomicU32 = AtomicU32::new(0);
static VIEW_Z: AtomicU32 = AtomicU32::new(0);
/// Frame number in which the HUD camera was last used (gameplay frames).
static HUD_FRAME: AtomicU32 = AtomicU32::new(u32::MAX);

/// Which side of the original 4:3 layout something belongs to, from its 4:3 screen position
/// (-1 .. 1): -1 = left, 1 = right, 0 = centre.
fn layout_side(ndc_43: f32) -> f32 {
    if ndc_43 < -0.3 {
        -1.0
    } else if ndc_43 > 0.3 {
        1.0
    } else {
        0.0
    }
}

/// How far (in NDC units, 2 = screen width) edge anchoring moves side elements outwards.
fn edge_shift_ndc() -> f32 {
    let c = cfg();
    let w43 = c.height as f32 * 4.0 / 3.0;
    ((c.width as f32 - w43) / c.width as f32).max(0.0)
}

/// Called right before every draw while the HUD camera is active (the game restores the viewport
/// through state blocks without calling SetViewport, so setting it once is not enough).
/// - Off-centre elements (tip panels, meters, slide-ins) are laid out partly beyond the 4:3 edges,
///   so they get the 4:3 projection clipped to the central 4:3 area, exactly as the original.
/// - Centred elements (letterbox bars, fades, centred text) keep the full screen with the game's
///   Hor+ projection: same size and position, but full-width pieces reach the screen edges.
unsafe fn enforce_hud_state(dev: *mut c_void) {
    if !HUD_MODE.load(Relaxed) {
        return;
    }
    let (view_x, view_z) = (f32::from_bits(VIEW_X.load(Relaxed)), f32::from_bits(VIEW_Z.load(Relaxed)));
    let centred = view_x.abs() < 0.05;
    let (natural, clipped) = *HUD_PROJ.lock().unwrap();
    unsafe {
        let set_vp: unsafe extern "system" fn(*mut c_void, *const Viewport) -> i32 = std::mem::transmute(ORIG_SET_VIEWPORT.load(Relaxed));
        let set_tf: unsafe extern "system" fn(*mut c_void, u32, *const [f32; 16]) -> i32 = std::mem::transmute(ORIG_SET_TRANSFORM.load(Relaxed));
        if cfg().hud_edges {
            // Correct proportions (the game's Hor+ projection, which matches the 4:3 layout in the
            // middle), then move left/right elements out to the screen edges. Pieces designed to hang
            // past the 4:3 screen edge now hang past the real screen edge, so they clip the same way.
            let side = if view_z.abs() > 1e-3 { layout_side(view_x / view_z * clipped[0]) } else { 0.0 };
            let mut m = natural;
            m[8] += side * edge_shift_ndc(); // _31: shifts x_ndc by a constant for perspective matrices
            set_vp(dev, &full_viewport());
            set_tf(dev, D3DTS_PROJECTION, &m);
            set_render_state_raw(dev, D3DRS_CLIPPING, 1);
            return;
        }
        if centred {
            set_vp(dev, &full_viewport());
            set_tf(dev, D3DTS_PROJECTION, &natural);
        } else {
            set_vp(dev, &hud_viewport());
            set_tf(dev, D3DTS_PROJECTION, &clipped);
        }
        set_render_state_raw(dev, D3DRS_CLIPPING, 1);
    }
}

/// Enter or leave HUD drawing: 4:3 viewport in the middle with clipping forced on.
unsafe fn set_hud_mode(dev: *mut c_void, on: bool) {
    if on {
        HUD_FRAME.store(FRAMES.load(Relaxed), Relaxed);
    }
    if HUD_MODE.swap(on, Relaxed) == on {
        return;
    }
    if dumping() {
        log(format!("  [wbu-fix] HUD mode {}", if on { "ON (4:3 viewport, clipping on)" } else { "off" }));
    }
    unsafe {
        if on {
            set_viewport_raw(dev, &hud_viewport());
            set_render_state_raw(dev, D3DRS_CLIPPING, 1);
        } else {
            set_viewport_raw(dev, &full_viewport());
            set_render_state_raw(dev, D3DRS_CLIPPING, GAME_CLIPPING.load(Relaxed));
        }
    }
}

unsafe extern "system" fn hook_set_viewport(this: *mut c_void, vp: *const [u32; 6]) -> i32 {
    if dumping() && !vp.is_null() {
        let v = unsafe { *vp };
        log(format!("  SetViewport x {} y {} w {} h {}", v[0], v[1], v[2], v[3]));
    }
    if HUD_MODE.load(Relaxed) {
        unsafe { set_viewport_raw(this, &hud_viewport()) };
        return 0;
    }
    let orig: unsafe extern "system" fn(*mut c_void, *const [u32; 6]) -> i32 = unsafe { std::mem::transmute(ORIG_SET_VIEWPORT.load(Relaxed)) };
    unsafe { orig(this, vp) }
}

unsafe extern "system" fn hook_set_render_state(this: *mut c_void, state: u32, value: u32) -> i32 {
    if dumping() {
        let name = match state {
            7 => Some("ZENABLE"),
            14 => Some("ZWRITEENABLE"),
            27 => Some("ALPHABLENDENABLE"),
            _ => None,
        };
        if let Some(n) = name {
            log(format!("  SetRenderState {n} = {value}"));
        }
    }
    if state == D3DRS_CLIPPING {
        GAME_CLIPPING.store(value, Relaxed);
        if HUD_MODE.load(Relaxed) {
            return 0; // keep clipping on while the HUD is clipped to 4:3
        }
    }
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32) -> i32 = unsafe { std::mem::transmute(ORIG_SET_RENDER_STATE.load(Relaxed)) };
    unsafe { orig(this, state, value) }
}

/// The game clears the back buffer to pure red, which was never visible while every 2D screen
/// filled the display. With pillarboxing the bars would show it, so clear to black instead.
unsafe extern "system" fn hook_clear(this: *mut c_void, count: u32, rects: *const c_void, flags: u32, color: u32, z: f32, stencil: u32) -> i32 {
    let color = if cfg().hud_fix && color & 0x00FF_FFFF == 0x00FF_0000 { color & 0xFF00_0000 } else { color };
    let orig: unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32, u32, f32, u32) -> i32 =
        unsafe { std::mem::transmute(ORIG_CLEAR.load(Relaxed)) };
    unsafe { orig(this, count, rects, flags, color, z, stencil) }
}
static ORIG_SET_TEXTURE: AtomicUsize = AtomicUsize::new(0);
static ORIG_SET_STREAM: AtomicUsize = AtomicUsize::new(0);
static CURRENT_TEX: AtomicUsize = AtomicUsize::new(0);
static CURRENT_VB: AtomicUsize = AtomicUsize::new(0);
static CURRENT_STRIDE: AtomicU32 = AtomicU32::new(0);
/// Render-target textures (full-screen effects): never pillarboxed.
static RENDER_TARGETS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe extern "system" fn hook_create_texture(
    this: *mut c_void, w: u32, h: u32, levels: u32, usage: u32, fmt: u32, pool: u32, out: *mut *mut c_void,
) -> i32 {
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32, u32, *mut *mut c_void) -> i32 =
        unsafe { std::mem::transmute(ORIG_CREATE_TEXTURE.load(Relaxed)) };
    let hr = unsafe { orig(this, w, h, levels, usage, fmt, pool, out) };
    if hr >= 0 && usage & 1 != 0 && !out.is_null() {
        RENDER_TARGETS.lock().unwrap().push(unsafe { *out } as usize); // D3DUSAGE_RENDERTARGET
    }
    hr
}

unsafe extern "system" fn hook_set_texture(this: *mut c_void, stage: u32, tex: *mut c_void) -> i32 {
    if stage == 0 {
        CURRENT_TEX.store(tex as usize, Relaxed);
    }
    let orig: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32 = unsafe { std::mem::transmute(ORIG_SET_TEXTURE.load(Relaxed)) };
    unsafe { orig(this, stage, tex) }
}

unsafe extern "system" fn hook_set_stream(this: *mut c_void, stream: u32, vb: *mut c_void, stride: u32) -> i32 {
    if stream == 0 {
        CURRENT_VB.store(vb as usize, Relaxed);
        CURRENT_STRIDE.store(stride, Relaxed);
    }
    let orig: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> i32 = unsafe { std::mem::transmute(ORIG_SET_STREAM.load(Relaxed)) };
    unsafe { orig(this, stream, vb, stride) }
}

/// Frame number being dumped after an F12 press (u32::MAX = none).
static DUMP_FRAME: AtomicU32 = AtomicU32::new(u32::MAX);

fn dumping() -> bool {
    DUMP_FRAME.load(Relaxed) == FRAMES.load(Relaxed)
}

/// Describe one draw call for the F12 dump: texture, screen rectangle, first vertex colour.
fn dump_draw(api: &str, prim: u32, count: u32, verts: Option<&[u8]>, stride: usize, squeezed: bool) {
    let fvf = CURRENT_FVF.load(Relaxed);
    let tex = CURRENT_TEX.load(Relaxed);
    let rt = tex != 0 && RENDER_TARGETS.lock().unwrap().contains(&tex);
    let mut tex_size = String::from("-");
    if tex != 0 {
        unsafe {
            let vt = *(tex as *const *const usize);
            let get_level_desc: unsafe extern "system" fn(usize, u32, *mut u32) -> i32 = std::mem::transmute(*vt.add(14));
            let mut desc = [0u32; 8];
            if get_level_desc(tex, 0, desc.as_mut_ptr()) >= 0 {
                tex_size = format!("{}x{} fmt {:#x}", desc[6], desc[7], desc[0]);
            }
        }
    }
    let mut geo = String::new();
    if let Some(v) = verts.filter(|_| fvf & D3DFVF_POSITION_MASK == D3DFVF_XYZRHW && stride >= 8) {
        let f = |c: &[u8], o: usize| f32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]]);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for c in v.chunks_exact(stride) {
            x0 = x0.min(f(c, 0));
            x1 = x1.max(f(c, 0));
            y0 = y0.min(f(c, 4));
            y1 = y1.max(f(c, 4));
        }
        geo = format!(" rect x {x0:.0}..{x1:.0} y {y0:.0}..{y1:.0}");
        if fvf & 0x40 != 0 && stride >= 20 {
            geo += &format!(" colour {:#010x}", u32::from_le_bytes([v[16], v[17], v[18], v[19]]));
        }
    }
    log(format!(
        "  {api} prim {prim} n {count} fvf {fvf:#x} tex {tex:#x} ({tex_size}){} {}{geo}",
        if rt { " RENDER-TARGET" } else { "" },
        if squeezed { "SQUEEZED" } else { "kept" }
    ));
}

/// Read `n` vertices starting at `start` from a vertex buffer (read-only lock).
unsafe fn read_vb(vb: *mut c_void, start: u32, n: usize, stride: usize) -> Option<Vec<u8>> {
    unsafe {
        let vt = *(vb as *const *const usize);
        let lock: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut u8, u32) -> i32 = std::mem::transmute(*vt.add(11));
        let unlock: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(*vt.add(12));
        let mut ptr: *mut u8 = std::ptr::null_mut();
        if lock(vb, start * stride as u32, (n * stride) as u32, &mut ptr, 0x10) < 0 || ptr.is_null() {
            return None;
        }
        let copy = std::slice::from_raw_parts(ptr, n * stride).to_vec();
        unlock(vb);
        Some(copy)
    }
}

/// Width of texture level 0, cached per texture pointer.
static TEX_WIDTHS: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

fn texture_width(tex: usize) -> u32 {
    if let Some(&(_, w)) = TEX_WIDTHS.lock().unwrap().iter().find(|e| e.0 == tex) {
        return w;
    }
    let mut desc = [0u32; 8];
    let w = unsafe {
        let vt = *(tex as *const *const usize);
        let get_level_desc: unsafe extern "system" fn(usize, u32, *mut u32) -> i32 = std::mem::transmute(*vt.add(14));
        if get_level_desc(tex, 0, desc.as_mut_ptr()) >= 0 { desc[6] } else { 0 }
    };
    let mut cache = TEX_WIDTHS.lock().unwrap();
    if cache.len() > 4096 {
        cache.clear(); // textures are recreated per level; keep the cache small
    }
    cache.push((tex, w));
    w
}

/// Could this draw be a screen-space (XYZRHW) 2D draw that needs squeezing? Cheap pre-check.
fn screen_space_candidate() -> bool {
    let c = cfg();
    c.hud_fix && c.width * 3 > c.height * 4 && CURRENT_FVF.load(Relaxed) & D3DFVF_POSITION_MASK == D3DFVF_XYZRHW
}

/// Decide from the actual vertices whether a screen-space draw is squeezed to 4:3 in the middle.
/// - Render targets and screen-sized textures are copies of the (already corrected) screen: keep.
/// - Untextured quads spanning the whole width are fades/overlays: keep. Others (bars, frames): squeeze.
/// - Everything else (artwork, text, movie frames): squeeze.
fn should_squeeze(verts: &[u8], stride: usize) -> bool {
    let c = cfg();
    let tex = CURRENT_TEX.load(Relaxed);
    if tex != 0 {
        return !RENDER_TARGETS.lock().unwrap().contains(&tex) && texture_width(tex) < c.width;
    }
    let (mut x0, mut x1) = (f32::MAX, f32::MIN);
    for v in verts.chunks_exact(stride) {
        let x = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
        x0 = x0.min(x);
        x1 = x1.max(x);
    }
    !(x0 <= 1.0 && x1 >= c.width as f32 - 1.0)
}

/// Squeeze the screen-space x coordinate (first float of each vertex) towards the centre.
/// In gameplay frames with `hud_layout=edges`, 2D HUD text (scores, tip text) is then moved out to
/// the screen edge together with the 3D panel it sits on.
fn pillarbox_vertices(data: &mut [u8], stride: usize) {
    let c = cfg();
    let (cx, scale) = (c.width as f32 / 2.0, (4.0 / 3.0) / (c.width as f32 / c.height as f32));
    let mut shift = 0.0;
    if c.hud_edges && HUD_FRAME.load(Relaxed) == FRAMES.load(Relaxed) {
        let (mut x0, mut x1) = (f32::MAX, f32::MIN);
        for v in data.chunks_exact(stride) {
            let x = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
            x0 = x0.min(x);
            x1 = x1.max(x);
        }
        // Pre-squeeze coordinates are the 4:3 layout spread over the full width.
        let ndc_43 = ((x0 + x1) / 2.0) / c.width as f32 * 2.0 - 1.0;
        shift = layout_side(ndc_43) * edge_shift_ndc() * c.width as f32 / 2.0;
    }
    for v in data.chunks_exact_mut(stride) {
        let x = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
        v[..4].copy_from_slice(&(cx + (x - cx) * scale + shift).to_le_bytes());
    }
}

fn vertex_count(prim: u32, count: u32) -> usize {
    (match prim {
        1 => count,     // point list
        2 => count * 2, // line list
        3 => count + 1, // line strip
        4 => count * 3, // triangle list
        _ => count + 2, // triangle strip / fan
    }) as usize
}
static CURRENT_FVF: AtomicU32 = AtomicU32::new(0);
static DRAW_STATS: Mutex<Vec<(&'static str, u32, u32)>> = Mutex::new(Vec::new());

fn note_draw(api: &'static str) {
    if cfg().log_draws != 0 && FRAMES.load(Relaxed) == cfg().log_draws {
        let fvf = CURRENT_FVF.load(Relaxed);
        let mut st = DRAW_STATS.lock().unwrap();
        match st.iter_mut().find(|e| e.0 == api && e.1 == fvf) {
            Some(e) => e.2 += 1,
            None => st.push((api, fvf, 1)),
        }
    }
}

unsafe extern "system" fn hook_set_vs(this: *mut c_void, handle: u32) -> i32 {
    CURRENT_FVF.store(handle, Relaxed);
    let orig: unsafe extern "system" fn(*mut c_void, u32) -> i32 = unsafe { std::mem::transmute(ORIG_SET_VS.load(Relaxed)) };
    unsafe { orig(this, handle) }
}

unsafe extern "system" fn hook_draw_primitive(this: *mut c_void, prim: u32, start: u32, count: u32) -> i32 {
    note_draw("DrawPrimitive");
    unsafe { enforce_hud_state(this) };
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, u32) -> i32 = unsafe { std::mem::transmute(ORIG_DRAW[0].load(Relaxed)) };
    let (vb, stride) = (CURRENT_VB.load(Relaxed) as *mut c_void, CURRENT_STRIDE.load(Relaxed) as usize);
    let verts = if (screen_space_candidate() || dumping()) && !vb.is_null() && stride >= 4 {
        unsafe { read_vb(vb, start, vertex_count(prim, count), stride) }
    } else {
        None
    };
    let squeeze = screen_space_candidate() && verts.as_deref().is_some_and(|v| should_squeeze(v, stride));
    if dumping() {
        dump_draw("DrawPrimitive", prim, count, verts.as_deref(), stride, squeeze);
    }
    if squeeze {
        unsafe {
            // Draw an adjusted copy of the vertices instead of the vertex buffer.
            if let Some(mut copy) = verts {
                pillarbox_vertices(&mut copy, stride);
                let draw_up: unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void, u32) -> i32 =
                    std::mem::transmute(ORIG_DRAW[2].load(Relaxed));
                let hr = draw_up(this, prim, count, copy.as_ptr().cast(), stride as u32);
                // DrawPrimitiveUP unbinds stream 0, so rebind the vertex buffer the game had set.
                let set_stream: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> i32 =
                    std::mem::transmute(ORIG_SET_STREAM.load(Relaxed));
                set_stream(this, 0, vb, stride as u32);
                return hr;
            }
        }
    }
    unsafe { orig(this, prim, start, count) }
}

unsafe extern "system" fn hook_draw_indexed(this: *mut c_void, a: u32, b: u32, c: u32, d: u32, e: u32) -> i32 {
    note_draw("DrawIndexedPrimitive");
    unsafe { enforce_hud_state(this) };
    if dumping() {
        dump_draw("DrawIndexedPrimitive", a, d, None, 0, false);
        if HUD_MODE.load(Relaxed) {
            let v = unsafe { current_viewport(this) };
            log(format!("    live viewport {} {} {} {}", v[0], v[1], v[2], v[3]));
        }
    }
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32) -> i32 = unsafe { std::mem::transmute(ORIG_DRAW[1].load(Relaxed)) };
    unsafe { orig(this, a, b, c, d, e) }
}

unsafe extern "system" fn hook_draw_up(this: *mut c_void, ty: u32, count: u32, data: *const c_void, stride: u32) -> i32 {
    note_draw("DrawPrimitiveUP");
    unsafe { enforce_hud_state(this) };
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void, u32) -> i32 = unsafe { std::mem::transmute(ORIG_DRAW[2].load(Relaxed)) };
    let verts = (!data.is_null() && stride >= 4)
        .then(|| unsafe { std::slice::from_raw_parts(data.cast::<u8>(), vertex_count(ty, count) * stride as usize) });
    let squeeze = screen_space_candidate() && verts.is_some_and(|v| should_squeeze(v, stride as usize));
    if dumping() && let Some(v) = verts {
        dump_draw("DrawPrimitiveUP", ty, count, Some(v), stride as usize, squeeze);
    }
    if squeeze {
        // Movies and other user-pointer 2D quads (e.g. the 640x480 Bink frame texture).
        let mut copy = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), vertex_count(ty, count) * stride as usize) }.to_vec();
        pillarbox_vertices(&mut copy, stride as usize);
        return unsafe { orig(this, ty, count, copy.as_ptr().cast(), stride) };
    }
    unsafe { orig(this, ty, count, data, stride) }
}

unsafe extern "system" fn hook_draw_indexed_up(
    this: *mut c_void, ty: u32, min: u32, nv: u32, count: u32, idx: *const c_void, fmt: u32, data: *const c_void, stride: u32,
) -> i32 {
    note_draw("DrawIndexedPrimitiveUP");
    unsafe { enforce_hud_state(this) };
    if dumping() && CURRENT_FVF.load(Relaxed) & D3DFVF_POSITION_MASK == D3DFVF_XYZRHW && !data.is_null() {
        let v = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add((min * stride) as usize), (nv * stride) as usize) };
        dump_draw("DrawIndexedPrimitiveUP", ty, count, Some(v), stride as usize, false);
    }
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, *const c_void, u32, *const c_void, u32) -> i32 =
        unsafe { std::mem::transmute(ORIG_DRAW[3].load(Relaxed)) };
    unsafe { orig(this, ty, min, nv, count, idx, fmt, data, stride) }
}

/// D3DPRESENT_PARAMETERS (Direct3D 8), as 13 u32 fields.
type PresentParams = [u32; 13];
const PP_FORMAT: usize = 2;
const PP_BACKBUFFERS: usize = 3;
const PP_WINDOWED: usize = 7;
const PP_FLAGS: usize = 10;
const PP_REFRESH: usize = 11;
const PP_INTERVAL: usize = 12;

fn make_windowed(pp: &mut PresentParams) {
    let before = *pp;
    pp[PP_WINDOWED] = 1;
    pp[PP_BACKBUFFERS] = 1;
    pp[PP_FORMAT] = D3DFMT_X8R8G8B8; // windowed back buffers use the desktop format
    pp[PP_REFRESH] = 0; // must be 0 in windowed mode
    pp[PP_INTERVAL] = 0; // D3DPRESENT_INTERVAL_DEFAULT; frame pacing is done in Present
    if !cfg().debug_shots.is_empty() || cfg().debug_keys {
        pp[PP_FLAGS] |= 1; // D3DPRESENTFLAG_LOCKABLE_BACKBUFFER, so frames can be saved
    }
    log(format!("present params: {before:x?} -> {pp:x?}"));
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Direct3DCreate8(sdk_version: u32) -> *mut c_void {
    unsafe {
        if REAL_CREATE.load(Relaxed) == 0 {
            let mut buf = [0u16; 260];
            let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
            let path: Vec<u16> = buf[..n].iter().copied().chain("\\d3d8.dll\0".encode_utf16()).collect();
            let module = LoadLibraryW(path.as_ptr());
            match GetProcAddress(module, c"Direct3DCreate8".as_ptr().cast()) {
                Some(f) => REAL_CREATE.store(f as usize, Relaxed),
                None => {
                    log("FATAL: could not load the system d3d8.dll");
                    return std::ptr::null_mut();
                }
            }
        }
        let real: unsafe extern "system" fn(u32) -> *mut c_void = std::mem::transmute(REAL_CREATE.load(Relaxed));
        let d3d = real(sdk_version);
        if !d3d.is_null() && CONFIG.get().is_some() {
            let orig = hook_vtable(d3d, D3D_CREATE_DEVICE, hook_create_device as *const () as usize);
            if orig != hook_create_device as *const () as usize {
                ORIG_CREATE_DEVICE.store(orig, Relaxed);
            }
        }
        d3d
    }
}

unsafe extern "system" fn hook_create_device(
    this: *mut c_void,
    adapter: u32,
    device_type: u32,
    hwnd: HWND,
    flags: u32,
    pp: *mut PresentParams,
    out: *mut *mut c_void,
) -> i32 {
    unsafe {
        make_windowed(&mut *pp);
        let orig: unsafe extern "system" fn(*mut c_void, u32, u32, HWND, u32, *mut PresentParams, *mut *mut c_void) -> i32 =
            std::mem::transmute(ORIG_CREATE_DEVICE.load(Relaxed));
        let hr = orig(this, adapter, device_type, hwnd, flags, pp, out);
        log(format!("CreateDevice -> {hr:#x}"));
        if hr >= 0 && !(*out).is_null() {
            let dev = *out;
            for (slot, hook, store) in [
                (DEV_RESET, hook_reset as *const () as usize, &ORIG_RESET),
                (DEV_PRESENT, hook_present as *const () as usize, &ORIG_PRESENT),
                (DEV_SET_TRANSFORM, hook_set_transform as *const () as usize, &ORIG_SET_TRANSFORM),
                (DEV_SET_VERTEX_SHADER, hook_set_vs as *const () as usize, &ORIG_SET_VS),
                (DEV_DRAW_PRIMITIVE, hook_draw_primitive as *const () as usize, &ORIG_DRAW[0]),
                (DEV_DRAW_INDEXED, hook_draw_indexed as *const () as usize, &ORIG_DRAW[1]),
                (DEV_DRAW_UP, hook_draw_up as *const () as usize, &ORIG_DRAW[2]),
                (DEV_DRAW_INDEXED_UP, hook_draw_indexed_up as *const () as usize, &ORIG_DRAW[3]),
                (DEV_CREATE_TEXTURE, hook_create_texture as *const () as usize, &ORIG_CREATE_TEXTURE),
                (DEV_CLEAR, hook_clear as *const () as usize, &ORIG_CLEAR),
                (DEV_SET_VIEWPORT, hook_set_viewport as *const () as usize, &ORIG_SET_VIEWPORT),
                (DEV_SET_RENDER_STATE, hook_set_render_state as *const () as usize, &ORIG_SET_RENDER_STATE),
                (DEV_SET_TEXTURE, hook_set_texture as *const () as usize, &ORIG_SET_TEXTURE),
                (DEV_SET_STREAM_SOURCE, hook_set_stream as *const () as usize, &ORIG_SET_STREAM),
            ] {
                let orig = hook_vtable(dev, slot, hook);
                if orig != hook {
                    store.store(orig, Relaxed);
                }
            }
        }
        hr
    }
}

unsafe extern "system" fn hook_reset(this: *mut c_void, pp: *mut PresentParams) -> i32 {
    unsafe {
        make_windowed(&mut *pp);
        let orig: unsafe extern "system" fn(*mut c_void, *mut PresentParams) -> i32 = std::mem::transmute(ORIG_RESET.load(Relaxed));
        let hr = orig(this, pp);
        log(format!("Reset -> {hr:#x}"));
        hr
    }
}

static FRAME_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);
static FRAMES: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn hook_present(this: *mut c_void, src: *const c_void, dst: *const c_void, wnd: HWND, dirty: *const c_void) -> i32 {
    let orig: unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, HWND, *const c_void) -> i32 =
        unsafe { std::mem::transmute(ORIG_PRESENT.load(Relaxed)) };
    unsafe { set_hud_mode(this, false) }; // never carry the HUD viewport into the next frame
    let frame_no = FRAMES.fetch_add(1, Relaxed);
    if DUMP_FRAME.load(Relaxed) == frame_no {
        unsafe { save_back_buffer(this, frame_no) };
        log(format!("--- end of dump for frame {frame_no}"));
    }
    if cfg().debug_keys && unsafe { GetAsyncKeyState(0x7B) } & 1 != 0 {
        DUMP_FRAME.store(frame_no + 1, Relaxed); // VK_F12: dump the next full frame
        log(format!("--- F12: dumping screen-space draws of frame {}", frame_no + 1));
    }
    if cfg().log_draws != 0 && frame_no == cfg().log_draws {
        for (api, fvf, n) in DRAW_STATS.lock().unwrap().iter() {
            log(format!("draws frame {frame_no}: {api} fvf {fvf:#x} rhw={} x{n}", fvf & 0x4 != 0));
        }
    }
    if cfg().debug_shots.contains(&frame_no) {
        unsafe { save_back_buffer(this, frame_no) };
    }
    let hr = unsafe { orig(this, src, dst, wnd, dirty) };
    if frame_no == 0 {
        log(format!("first Present -> {hr:#x}"));
    }
    let fps = cfg().fps_limit;
    if fps > 0 {
        // Sleep most of the remaining frame time, then spin for precision.
        let frame = Duration::from_secs_f64(1.0 / fps as f64);
        let mut deadline = FRAME_DEADLINE.lock().unwrap();
        let now = Instant::now();
        let target = deadline.map_or(now + frame, |d| if d + frame < now { now } else { d + frame });
        if let Some(rest) = target.checked_duration_since(now) {
            if rest > Duration::from_millis(2) {
                std::thread::sleep(rest - Duration::from_millis(2));
            }
            while Instant::now() < target {
                std::hint::spin_loop();
            }
        }
        *deadline = Some(target);
    }
    hr
}

/// Save the back buffer (X8R8G8B8) as a 32-bit BMP. Debug aid: needs a lockable back buffer and no FSAA.
unsafe fn save_back_buffer(dev: *mut c_void, frame_no: u32) {
    unsafe {
        let vt = |obj: *mut c_void, i: usize| *(*(obj as *const *const usize)).add(i);
        let mut surf: *mut c_void = std::ptr::null_mut();
        let get_bb: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32 = std::mem::transmute(vt(dev, 16));
        if get_bb(dev, 0, 0, &mut surf) < 0 || surf.is_null() {
            return log("shot: GetBackBuffer failed");
        }
        let mut desc = [0u32; 8];
        let get_desc: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(vt(surf, 8));
        get_desc(surf, desc.as_mut_ptr());
        let (w, h) = (desc[6] as usize, desc[7] as usize);
        let mut locked: (i32, *const u8) = (0, std::ptr::null());
        let lock: unsafe extern "system" fn(*mut c_void, *mut (i32, *const u8), *const c_void, u32) -> i32 = std::mem::transmute(vt(surf, 9));
        if lock(surf, &mut locked, std::ptr::null(), 0x10) >= 0 {
            let mut bmp = Vec::with_capacity(54 + w * h * 4);
            let file_size = (54 + w * h * 4) as u32;
            bmp.extend(b"BM");
            bmp.extend(file_size.to_le_bytes());
            bmp.extend([0u8; 4]);
            bmp.extend(54u32.to_le_bytes());
            bmp.extend(40u32.to_le_bytes());
            bmp.extend((w as i32).to_le_bytes());
            bmp.extend((-(h as i32)).to_le_bytes()); // top-down rows
            bmp.extend(1u16.to_le_bytes());
            bmp.extend(32u16.to_le_bytes());
            bmp.extend([0u8; 24]);
            for y in 0..h {
                bmp.extend_from_slice(std::slice::from_raw_parts(locked.1.add(y * locked.0 as usize), w * 4));
            }
            let unlock: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(vt(surf, 10));
            unlock(surf);
            if let Some(dir) = DIR.get() {
                let _ = std::fs::write(dir.join(format!("wbu-shot-{frame_no}.bmp")), bmp);
            }
            log(format!("shot: saved frame {frame_no} ({w}x{h}, format {:#x})", desc[0]));
        } else {
            log("shot: LockRect failed");
        }
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(vt(surf, 2));
        release(surf);
    }
}

static PROJ_LOGGED: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn hook_set_transform(this: *mut c_void, state: u32, m: *const [f32; 16]) -> i32 {
    let orig: unsafe extern "system" fn(*mut c_void, u32, *const [f32; 16]) -> i32 =
        unsafe { std::mem::transmute(ORIG_SET_TRANSFORM.load(Relaxed)) };
    if dumping() && !m.is_null() {
        let name = match state {
            2 => "VIEW".to_string(),
            3 => "PROJECTION".to_string(),
            256 => "WORLD".to_string(),
            n => format!("state {n}"),
        };
        let v: Vec<String> = unsafe { *m }.iter().map(|x| format!("{x:.3}")).collect();
        log(format!("  SetTransform {name} [{}]", v.join(" ")));
    }
    if state == 2 && !m.is_null() {
        VIEW_X.store(unsafe { (*m)[12] }.to_bits(), Relaxed); // D3DTS_VIEW translation x
        VIEW_Z.store(unsafe { (*m)[14] }.to_bits(), Relaxed); // and z
    }
    if state != D3DTS_PROJECTION || m.is_null() {
        if cfg().log_projection && (16..24).contains(&state) && !m.is_null() && PROJ_LOGGED.fetch_add(1, Relaxed) < 400 {
            log(format!("tex{} {:?}", state - 16, unsafe { *m }));
        }
        return unsafe { orig(this, state, m) };
    }
    let c = cfg();
    let mut p = unsafe { *m };
    let perspective = (p[11] - 1.0).abs() < 1e-4 && p[15].abs() < 1e-4;
    if c.log_projection && PROJ_LOGGED.fetch_add(1, Relaxed) < 400 {
        log(format!("proj persp={perspective} _11={:.5} _22={:.5} ratio={:.4}", p[0], p[5], p[5] / p[0]));
    }
    let aspect = c.width as f32 / c.height as f32;
    if c.hud_fix && aspect > 4.0 / 3.0 + 0.01 && is_hud_projection(&p) {
        // Draw the HUD exactly as the 4:3 game did, inside the central 4:3 area. Its elements are
        // laid out partly beyond the 4:3 edges (slide-ins, hidden widgets) and must stay clipped.
        let to_43 = aspect / (4.0 / 3.0);
        let natural = p;
        p[0] *= to_43;
        p[8] *= to_43;
        *HUD_PROJ.lock().unwrap() = (natural, p);
        unsafe { set_hud_mode(this, true) };
        return unsafe { orig(this, state, &p) };
    }
    unsafe { set_hud_mode(this, false) };
    unsafe { orig(this, state, m) }
}

// ------------------------------------------------------------------ entry point

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe {
        let mut buf = [0u16; 520];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        let dll = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        let dir = dll.parent().map(PathBuf::from).unwrap_or_default();
        let _ = DIR.set(dir.clone());
        let _ = std::fs::remove_file(dir.join("wbu-fix.log"));
        SetProcessDPIAware(); // real pixels, not DPI-scaled ones
        timeBeginPeriod(1); // 1 ms sleep granularity for the frame cap
        let c = CONFIG.get_or_init(|| load_config(&dir));
        log(format!(
            "wbu-fix loaded: {}x{} borderless={} fov_fix={} hud_fix={} hud_layout={} fps_limit={}",
            c.width,
            c.height,
            c.borderless,
            c.fov_fix,
            c.hud_fix,
            if c.hud_edges { "edges" } else { "center" },
            c.fps_limit
        ));

        if !exe_matches() {
            log("WBU.EXE does not match the expected build; resolution and window patches skipped");
            return 1;
        }
        // Every res= choice in config.cfg now means the configured resolution.
        for (w, h) in RES_TABLES {
            write_protected(w, [c.width; 4]);
            write_protected(h, [c.height; 4]);
        }
        // Hor+: widen the horizontal FOV by aspect / (4/3). The game then derives exactly the
        // vertical FOV it has at 4:3, and culling, water and reflections all use the wider view.
        let aspect = c.width as f32 / c.height as f32;
        let original = std::ptr::read_unaligned(HFOV_FACTOR as *const f32);
        if c.fov_fix && aspect > 4.0 / 3.0 && (original - 0.99).abs() < 1e-5 {
            let widened = original * aspect / (4.0 / 3.0);
            write_protected(HFOV_FACTOR, widened);
            log(format!("fov: horizontal factor {original} -> {widened}"));
        }
        let prologue = std::slice::from_raw_parts(SHRINK_LOADED_FILE as *const u8, SHRINK_PROLOGUE.len());
        if prologue == SHRINK_PROLOGUE {
            write_protected(SHRINK_LOADED_FILE, 0xC3u8); // ret (cdecl: the caller pops the arguments)
            log("fixed: 'realloc relocated memory block' crash on level change / quit");
        }
        write_protected(IAT_CREATE_WINDOW, hook_create_window as *const () as usize);
        log("patched resolution tables and window creation");
    }
    1
}
