//! Composing on the graphics card. A source frame is uploaded once, each step
//! of `canvas` runs as a small shader pass doing the processor's own arithmetic
//! (`gpu.wgsl`), and only the pictures needed in memory come back: the output
//! for the encoders, the preview for the window.

use crate::canvas;
use crate::frame::{Frame, ROWS};
use crate::fx::{self, Placed, Turn};
use crate::model::{EffectKind, Input, KeyMode, Position};
use rayon::prelude::*;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once, OnceLock};
use std::time::Duration;
use wgpu::util::DeviceExt;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Room for one pass's settings: uniform offsets come in steps of 256.
const SLOT: usize = 256;
const SLOTS: usize = 2048;
/// Free pictures kept on the card for the next frames.
const SPARE_MOST: usize = 24;
/// Longest wait for the card before it counts as gone.
const PATIENCE: Duration = Duration::from_secs(3);

/// One pass's settings, laid out as `Params` in `gpu.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    rect: [f32; 4],
    map: [f32; 4],
    colour: [f32; 4],
    misc: [f32; 4],
    turn: [f32; 4],
    centre: [f32; 4],
    bounds: [f32; 4],
    shift: [i32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    key: [f32; 4],
    key_colour: [f32; 4],
    flags: [u32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Pass {
    Blit,
    Fill,
    Mix,
    Shift,
    Turn,
    Cube,
    Finish,
    Process,
    Lut,
    Mask,
}

impl Pass {
    const ALL: [Pass; 10] = [
        Pass::Blit,
        Pass::Fill,
        Pass::Mix,
        Pass::Shift,
        Pass::Turn,
        Pass::Cube,
        Pass::Finish,
        Pass::Process,
        Pass::Lut,
        Pass::Mask,
    ];

    fn entry(self) -> &'static str {
        match self {
            Pass::Blit => "fs_blit",
            Pass::Fill => "fs_fill",
            Pass::Mix => "fs_mix",
            Pass::Shift => "fs_shift",
            Pass::Turn => "fs_turn",
            Pass::Cube => "fs_cube",
            Pass::Finish => "fs_finish",
            Pass::Process => "fs_process",
            Pass::Lut => "fs_lut",
            Pass::Mask => "fs_mask",
        }
    }

    /// Drawn over what is there, the others replace it.
    fn over(self) -> bool {
        matches!(self, Pass::Blit | Pass::Fill | Pass::Turn)
    }
}

/// The graphics card, opened once and shared by every engine in the process.
struct Device {
    device: wgpu::Device,
    queue: wgpu::Queue,
    name: String,
    max_side: usize,
    params_layout: wgpu::BindGroupLayout,
    images_layout: wgpu::BindGroupLayout,
    pipes: HashMap<Pass, wgpu::RenderPipeline>,
    /// Why the card stopped working, from its own reports.
    broken: Arc<Mutex<Option<String>>>,
}

static HARDWARE: OnceLock<Result<Arc<Device>, String>> = OnceLock::new();
static ANY: LazyLock<Result<Arc<Device>, String>> = LazyLock::new(|| Device::open(true).map(Arc::new));

impl Device {
    fn hardware() -> &'static Result<Arc<Device>, String> {
        HARDWARE.get_or_init(|| Device::open(false).map(Arc::new))
    }

    fn shared(software: bool) -> Result<Arc<Device>, String> {
        let device = if software { &*ANY } else { Device::hardware() }.clone()?;
        let broken = device.broken.lock().unwrap().clone();
        broken.map_or(Ok(device), Err)
    }

    /// With `software`, an emulated card will do: enough to check pictures,
    /// far too slow for a show.
    fn open(software: bool) -> Result<Device, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let options = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        };
        let adapter = pollster::block_on(instance.request_adapter(&options))
            .map_err(|_| "aucune carte graphique adaptée".to_string())?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu && !software {
            return Err(format!("{} n'est qu'une émulation logicielle", info.name));
        }
        let limits = adapter.limits();
        let max_side = limits.max_texture_dimension_2d as usize;
        let desc = wgpu::DeviceDescriptor {
            label: Some("LiveCraft"),
            required_limits: limits,
            ..Default::default()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&desc))
            .map_err(|e| format!("carte graphique inutilisable : {e}"))?;
        let broken = Arc::new(Mutex::new(None));
        let said = broken.clone();
        device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
            said.lock().unwrap().get_or_insert_with(|| format!("erreur de la carte graphique : {e}"));
        }));
        let said = broken.clone();
        device.set_device_lost_callback(move |_, why| {
            said.lock().unwrap().get_or_insert_with(|| format!("carte graphique perdue : {why}"));
        });

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compositing"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu.wgsl").into()),
        });
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(size_of::<Params>() as u64),
                },
                count: None,
            }],
        });
        let image = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let table = wgpu::BindGroupLayoutEntry {
            binding: 3,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let images_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[image(0), image(1), image(2), table],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&params_layout), Some(&images_layout)],
            immediate_size: 0,
        });
        let pipe = |pass: Pass| {
            let blend = pass.over().then_some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(pass.entry()),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(pass.entry()),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipes = Pass::ALL.into_iter().map(|p| (p, pipe(p))).collect();
        if let Some(why) = broken.lock().unwrap().clone() {
            return Err(why);
        }
        let name = format!("{} ({:?})", info.name, info.backend);
        Ok(Device { device, queue, name, max_side, params_layout, images_layout, pipes, broken })
    }
}

/// A picture on the card.
struct Tex {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    w: usize,
    h: usize,
    format: wgpu::TextureFormat,
    /// The batch of commands that last touched it.
    used: Cell<u64>,
}

impl Tex {
    fn extent(&self) -> wgpu::Extent3d {
        wgpu::Extent3d { width: self.w as u32, height: self.h as u32, depth_or_array_layers: 1 }
    }
}

/// A finished picture, on the card, in memory, or both: each side is made
/// from the other only when someone asks for it.
#[derive(Clone)]
pub struct Pic(Rc<PicData>);

struct PicData {
    w: usize,
    h: usize,
    tex: OnceCell<Rc<Tex>>,
    frame: OnceCell<Arc<Frame>>,
}

impl Pic {
    fn of_frame(f: Arc<Frame>) -> Pic {
        Pic(Rc::new(PicData { w: f.w, h: f.h, tex: OnceCell::new(), frame: OnceCell::from(f) }))
    }

    fn of_tex(t: Rc<Tex>) -> Pic {
        Pic(Rc::new(PicData { w: t.w, h: t.h, tex: OnceCell::from(t), frame: OnceCell::new() }))
    }
}

struct Staging {
    buffer: wgpu::Buffer,
    busy: bool,
}

/// A picture on its way back from the card.
pub struct Readback {
    slot: usize,
    w: usize,
    h: usize,
    stride: usize,
    index: wgpu::SubmissionIndex,
    mapped: Arc<AtomicBool>,
}

impl Readback {
    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }
}

/// Commands being gathered for the card, and the pictures made for them.
struct Work {
    dev: Arc<Device>,
    encoder: RefCell<Option<wgpu::CommandEncoder>>,
    params: RefCell<Vec<u8>>,
    params_buf: wgpu::Buffer,
    params_group: wgpu::BindGroup,
    /// Counts submissions. A picture written by an upload must not have been
    /// used in the batch still being gathered, since uploads run first.
    batch: Cell<u64>,
    textures: RefCell<Vec<Rc<Tex>>>,
    staging: RefCell<Vec<Staging>>,
    /// Buffers source frames travel through, with the batch that last used each.
    uploads: RefCell<Vec<(wgpu::Buffer, Cell<u64>)>>,
    blank: Tex,
    no_table: wgpu::Buffer,
}

impl Work {
    fn new(dev: Arc<Device>) -> Work {
        let device = &dev.device;
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: (SLOT * SLOTS) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &dev.params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &params_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(size_of::<Params>() as u64),
                }),
            }],
        });
        let blank = new_tex(device, 1, 1, FORMAT, 0);
        dev.queue.write_texture(
            blank.texture.as_image_copy(),
            &[0; 4],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) },
            blank.extent(),
        );
        let no_table = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: &[0; 16],
            usage: wgpu::BufferUsages::STORAGE,
        });
        Work {
            encoder: RefCell::new(None),
            params: RefCell::new(Vec::new()),
            params_buf,
            params_group,
            batch: Cell::new(1),
            textures: RefCell::new(Vec::new()),
            staging: RefCell::new(Vec::new()),
            uploads: RefCell::new(Vec::new()),
            blank,
            no_table,
            dev,
        }
    }

    /// A picture nobody holds any more, else a new one.
    fn texture(&self, w: usize, h: usize, format: wgpu::TextureFormat) -> Rc<Tex> {
        let batch = self.batch.get();
        let mut list = self.textures.borrow_mut();
        let free = |t: &Rc<Tex>| Rc::strong_count(t) == 1 && t.used.get() != batch;
        if let Some(t) = list.iter().find(|t| free(t) && t.w == w && t.h == h && t.format == format) {
            t.used.set(batch);
            return t.clone();
        }
        let spare = list.iter().filter(|t| Rc::strong_count(t) == 1).count();
        if spare >= SPARE_MOST
            && let Some(old) = list.iter().position(free)
        {
            list.remove(old);
        }
        let t = Rc::new(new_tex(&self.dev.device, w, h, format, batch));
        list.push(t.clone());
        t
    }

    fn encode(&self, f: impl FnOnce(&mut wgpu::CommandEncoder)) {
        let mut slot = self.encoder.borrow_mut();
        f(slot.get_or_insert_with(|| self.dev.device.create_command_encoder(&Default::default())));
    }

    fn upload(&self, f: &Frame) -> Rc<Tex> {
        let max = self.dev.max_side;
        if f.w == 0 || f.h == 0 {
            let t = self.texture(1, 1, FORMAT);
            self.clear(&t);
            return t;
        }
        if f.w > max || f.h > max {
            // Bigger than the card takes: it is drawn scaled anyway.
            let k = max as f32 / f.w.max(f.h) as f32;
            let (w, h) = (((f.w as f32 * k) as usize).max(1), ((f.h as f32 * k) as usize).max(1));
            let mut small = Frame::new(w, h);
            small.blit_region(f, [0.0; 4], 0.0, 0.0, w as f32, h as f32, 255, 0.0);
            return self.upload(&small);
        }
        let t = self.texture(f.w, f.h, FORMAT);
        let row = f.w * 4;
        let stride = row.div_ceil(256) * 256;
        let size = (stride * f.h) as u64;
        let buffer = self.upload_buffer(size);
        if let Some(mut view) =
            wgpu::BufferSize::new(size).and_then(|n| self.dev.queue.write_buffer_with(&buffer, 0, n))
        {
            // A 4K frame is 33 MB: copied by several threads, it is on its
            // way in a fraction of the time.
            let band = ROWS * stride;
            let mut parts = Vec::new();
            let mut rest = view.slice(..);
            while rest.len() > band {
                let (part, after) = rest.split_at(band);
                parts.push(Band(part));
                rest = after;
            }
            parts.push(Band(rest));
            parts.into_par_iter().enumerate().for_each(|(n, Band(mut part))| {
                for (r, src) in f.data[n * ROWS * row..].chunks(row).take(ROWS).enumerate() {
                    part.slice(r * stride..r * stride + row).copy_from_slice(src);
                }
            });
        }
        t.used.set(self.batch.get());
        self.encode(|e| {
            let layout = wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(f.h as u32),
            };
            e.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo { buffer: &buffer, layout },
                t.texture.as_image_copy(),
                t.extent(),
            );
        });
        t
    }

    /// Its content is written when the batch is sent, before any of its
    /// commands: one buffer can only carry one frame per batch.
    fn upload_buffer(&self, size: u64) -> wgpu::Buffer {
        let batch = self.batch.get();
        let mut list = self.uploads.borrow_mut();
        if let Some((b, used)) = list.iter().find(|(b, used)| b.size() == size && used.get() != batch) {
            used.set(batch);
            return b.clone();
        }
        if list.len() >= SPARE_MOST
            && let Some(old) = list.iter().position(|(_, used)| used.get() != batch)
        {
            list.remove(old);
        }
        let b = self.dev.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upload"),
            size,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        list.push((b.clone(), Cell::new(batch)));
        b
    }

    fn tex(&self, p: &Pic) -> Rc<Tex> {
        p.0.tex.get_or_init(|| self.upload(p.0.frame.get().expect("a picture lives somewhere"))).clone()
    }

    fn clear(&self, target: &Tex) {
        target.used.set(self.batch.get());
        self.encode(|e| {
            let _ = e.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        });
    }

    fn copy(&self, from: &Tex, to: &Tex) {
        let batch = self.batch.get();
        from.used.set(batch);
        to.used.set(batch);
        self.encode(|e| {
            e.copy_texture_to_texture(from.texture.as_image_copy(), to.texture.as_image_copy(), to.extent())
        });
    }

    /// One pass over the pixels `area` (left, top, right, bottom) of `target`.
    fn draw(
        &self,
        target: &Tex,
        pass: Pass,
        area: [usize; 4],
        params: &Params,
        images: [Option<&Tex>; 3],
        table: Option<&wgpu::Buffer>,
    ) {
        let [x0, y0, x1, y1] = [area[0], area[1], area[2].min(target.w), area[3].min(target.h)];
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        if self.params.borrow().len() + SLOT > SLOT * SLOTS {
            self.flush();
        }
        let offset = {
            let mut list = self.params.borrow_mut();
            let at = list.len();
            list.extend_from_slice(bytemuck::bytes_of(params));
            list.resize(at + SLOT, 0);
            at as u32
        };
        let view = |n: usize| &images[n].unwrap_or(&self.blank).view;
        let group = self.dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.dev.images_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view(0)) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view(1)) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(view(2)) },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: table.unwrap_or(&self.no_table).as_entire_binding(),
                },
            ],
        });
        let batch = self.batch.get();
        target.used.set(batch);
        images.iter().flatten().for_each(|t| t.used.set(batch));
        self.encode(|e| {
            let mut rp = e.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.dev.pipes[&pass]);
            rp.set_bind_group(0, &self.params_group, &[offset]);
            rp.set_bind_group(1, &group, &[]);
            rp.set_scissor_rect(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
            rp.draw(0..3, 0..1);
        });
    }

    /// Sends everything gathered so far to the card.
    fn flush(&self) -> wgpu::SubmissionIndex {
        {
            let mut params = self.params.borrow_mut();
            if !params.is_empty() {
                self.dev.queue.write_buffer(&self.params_buf, 0, &params);
                params.clear();
            }
        }
        let commands = self.encoder.borrow_mut().take().map(|e| e.finish());
        let index = self.dev.queue.submit(commands);
        self.batch.set(self.batch.get() + 1);
        index
    }

    /// See `Frame::blit_region`.
    fn blit(
        &self,
        target: &Tex,
        src: &Tex,
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: u8,
        radius: f32,
    ) {
        if w < 1.0 || h < 1.0 || opacity == 0 {
            return;
        }
        let (tw, th) = (target.w as f32, target.h as f32);
        let (x0, x1) = (x.max(0.0) as usize, (x + w).min(tw).max(0.0) as usize);
        let (y0, y1) = (y.max(0.0) as usize, (y + h).min(th).max(0.0) as usize);
        let (sw, sh) = (src.w as f32, src.h as f32);
        let params = Params {
            rect: [x, y, w, h],
            map: [
                crop[0] * sw,
                crop[2] * sh,
                (1.0 - crop[0] - crop[1]) * sw / w,
                (1.0 - crop[2] - crop[3]) * sh / h,
            ],
            colour: [0.0, 0.0, 0.0, opacity as f32],
            misc: [radius, 0.0, 0.0, 0.0],
            ..Params::default()
        };
        self.draw(target, Pass::Blit, [x0, y0, x1, y1], &params, [Some(src), None, None], None);
    }

    /// See `Frame::transform` when it turns.
    fn turn(&self, target: &Tex, src: &Tex, t: &Turn, opacity: u8) {
        let params = Params {
            turn: [t.sin, t.cos, t.zoom, if t.mirror { 1.0 } else { 0.0 }],
            centre: [t.cx, t.cy, t.pan_x, t.pan_y],
            bounds: t.bounds,
            colour: [0.0, 0.0, 0.0, opacity as f32],
            ..Params::default()
        };
        self.draw(target, Pass::Turn, [0, 0, target.w, target.h], &params, [Some(src), None, None], None);
    }

    /// Copies a picture into a buffer the processor can read, and sends it.
    fn start_readback(&self, tex: &Tex) -> Readback {
        let (w, h) = (tex.w, tex.h);
        let stride = (w * 4).div_ceil(256) * 256;
        let size = (stride * h) as u64;
        let slot = {
            let mut list = self.staging.borrow_mut();
            let found = list.iter().position(|s| !s.busy && s.buffer.size() == size);
            let slot = found.unwrap_or_else(|| {
                // Buffers of a former size are of no more use.
                list.retain(|s| s.busy || s.buffer.size() == size);
                list.push(Staging {
                    buffer: self.dev.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("readback"),
                        size,
                        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
                    busy: false,
                });
                list.len() - 1
            });
            list[slot].busy = true;
            let buffer = list[slot].buffer.clone();
            tex.used.set(self.batch.get());
            self.encode(|e| {
                let layout = wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h as u32),
                };
                e.copy_texture_to_buffer(
                    tex.texture.as_image_copy(),
                    wgpu::TexelCopyBufferInfo { buffer: &buffer, layout },
                    tex.extent(),
                );
            });
            slot
        };
        let index = self.flush();
        let mapped = Arc::new(AtomicBool::new(false));
        let done = mapped.clone();
        self.staging.borrow()[slot]
            .buffer
            .map_async(wgpu::MapMode::Read, .., move |r| done.store(r.is_ok(), Ordering::Release));
        Readback { slot, w, h, stride, index, mapped }
    }

    /// Waits for a picture coming back and copies it into `out`. On failure
    /// `out` keeps what it held, and the card is reported broken.
    fn finish_readback(&self, back: Readback, out: &mut Frame) {
        let wait = wgpu::PollType::Wait { submission_index: Some(back.index), timeout: Some(PATIENCE) };
        if let Err(e) = self.dev.device.poll(wait) {
            self.dev
                .broken
                .lock()
                .unwrap()
                .get_or_insert_with(|| format!("la carte graphique ne répond plus : {e}"));
        }
        let mut list = self.staging.borrow_mut();
        let staging = &mut list[back.slot];
        let copied = back.mapped.load(Ordering::Acquire) && {
            if (out.w, out.h) != (back.w, back.h) {
                *out = Frame::for_overwrite(back.w, back.h);
            }
            let row = back.w * 4;
            let copied = staging.buffer.get_mapped_range(..).map(|view| {
                out.data
                    .par_chunks_mut(row)
                    .zip(view.par_chunks(back.stride))
                    .with_min_len(ROWS)
                    .for_each(|(d, s)| d.copy_from_slice(&s[..row]));
            });
            staging.buffer.unmap();
            copied.is_ok()
        };
        if !copied {
            self.dev
                .broken
                .lock()
                .unwrap()
                .get_or_insert_with(|| "image perdue en revenant de la carte graphique".into());
        }
        staging.busy = false;
    }
}

/// A band of an upload buffer. `WriteOnly` is only `Send` for sized contents,
/// not slices, though a slice is the same pointer and length.
struct Band<'a>(wgpu::WriteOnly<'a, [u8]>);

// SAFETY: bands are disjoint parts of one write mapping, each written by a
// single thread, all done before the mapping is released; nothing is read.
unsafe impl Send for Band<'_> {}

fn new_tex(device: &wgpu::Device, w: usize, h: usize, format: wgpu::TextureFormat, batch: u64) -> Tex {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Tex { texture, view, w, h, format, used: Cell::new(batch) }
}

/// A picture being composed on the card.
pub struct Canvas {
    work: Rc<Work>,
    tex: Rc<Tex>,
}

impl Canvas {
    fn full(&self) -> [usize; 4] {
        [0, 0, self.tex.w, self.tex.h]
    }

    /// Copies rows or columns of `from`, each pixel reading `shift` away.
    fn shifted(&self, from: &Pic, area: [usize; 4], shift: [i32; 2]) {
        let src = self.work.tex(from);
        let params = Params { shift: [shift[0], shift[1], 0, 0], ..Params::default() };
        self.work.draw(&self.tex, Pass::Shift, area, &params, [Some(&src), None, None], None);
    }
}

impl canvas::Canvas for Canvas {
    type Pic = Pic;

    fn size(&self) -> (usize, usize) {
        (self.tex.w, self.tex.h)
    }

    fn pic_size(p: &Pic) -> (usize, usize) {
        (p.0.w, p.0.h)
    }

    fn clear(&mut self) {
        self.work.clear(&self.tex);
    }

    fn copy_from(&mut self, p: &Pic) {
        let src = self.work.tex(p);
        self.work.copy(&src, &self.tex);
    }

    fn blend_over(&mut self, p: &Pic, opacity: u8) {
        if opacity == 0 {
            return;
        }
        let src = self.work.tex(p);
        let (w, h) = (self.tex.w as f32, self.tex.h as f32);
        let params = Params {
            rect: [0.0, 0.0, w, h],
            map: [0.0, 0.0, src.w as f32 / w, src.h as f32 / h],
            colour: [0.0, 0.0, 0.0, opacity as f32],
            // Pixel for pixel, a clear one leaves what is under it.
            misc: [0.0, 0.0, 1.0, 0.0],
            ..Params::default()
        };
        self.work.draw(&self.tex, Pass::Blit, self.full(), &params, [Some(&src), None, None], None);
    }

    fn blit_region(
        &mut self,
        p: &Pic,
        crop: [f32; 4],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: u8,
        radius: f32,
    ) {
        let src = self.work.tex(p);
        self.work.blit(&self.tex, &src, crop, x, y, w, h, opacity, radius);
    }

    fn fill_round(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: [u8; 3], opacity: u8) {
        let (tw, th) = (self.tex.w as f32, self.tex.h as f32);
        let (x0, x1) = (x.max(0.0) as usize, (x + w).min(tw).max(0.0) as usize);
        let (y0, y1) = (y.max(0.0) as usize, (y + h).min(th).max(0.0) as usize);
        let params = Params {
            rect: [x, y, w, h],
            colour: [rgb[0] as f32, rgb[1] as f32, rgb[2] as f32, opacity as f32],
            misc: [radius, 0.0, 0.0, 0.0],
            ..Params::default()
        };
        self.work.draw(&self.tex, Pass::Fill, [x0, y0, x1, y1], &params, [None; 3], None);
    }

    fn mix(&mut self, a: &Pic, b: &Pic, t: f32) {
        let (a, b) = (self.work.tex(a), self.work.tex(b));
        let weight = (t.clamp(0.0, 1.0) * 256.0) as u32;
        let params = Params { misc: [0.0, weight as f32, 0.0, 0.0], ..Params::default() };
        self.work.draw(&self.tex, Pass::Mix, self.full(), &params, [Some(&a), Some(&b), None], None);
    }

    fn wipe(&mut self, a: &Pic, b: &Pic, t: f32) {
        let (w, h) = self.size();
        let edge = (t.clamp(0.0, 1.0) * w as f32) as usize;
        self.shifted(b, [0, 0, edge, h], [0, 0]);
        self.shifted(a, [edge, 0, w, h], [0, 0]);
    }

    fn slide(&mut self, a: &Pic, b: &Pic, t: f32) {
        let (w, h) = self.size();
        let off = (t.clamp(0.0, 1.0) * w as f32) as usize;
        let keep = w - off;
        self.shifted(a, [0, 0, keep, h], [off as i32, 0]);
        self.shifted(b, [keep, 0, w, h], [-(keep as i32), 0]);
    }

    fn roller(&mut self, a: &Pic, b: &Pic, t: f32) {
        let (w, h) = self.size();
        let edge = (t.clamp(0.0, 1.0) * h as f32) as usize;
        self.shifted(b, [0, 0, w, edge], [0, 0]);
        self.shifted(a, [0, edge, w, h], [0, 0]);
    }

    fn vertical_slide(&mut self, a: &Pic, b: &Pic, t: f32) {
        let (w, h) = self.size();
        let rows = (t * h as f32) as usize;
        let keep = h - rows;
        self.shifted(a, [0, 0, w, keep], [0, rows as i32]);
        self.shifted(b, [0, keep, w, h], [0, -(keep as i32)]);
    }

    fn blend_transformed(&mut self, p: &Pic, pos: &Position, opacity: u8) {
        if opacity == 0 {
            return;
        }
        let (w, h) = Self::pic_size(p);
        match fx::placement(w, h, pos, false) {
            Placed::Box { crop, x, y, w, h } => self.blit_region(p, crop, x, y, w, h, opacity, 0.0),
            Placed::Turned(t) => {
                let src = self.work.tex(p);
                self.work.turn(&self.tex, &src, &t, opacity);
            }
        }
    }

    fn cube(&mut self, a: &Pic, b: &Pic, t: f32, zoom: f32) {
        let (a, b) = (self.work.tex(a), self.work.tex(b));
        let (sin, cos) = crate::transitions::cube_angle(t).sin_cos();
        let params = Params {
            rect: [0.0, 0.0, self.tex.w as f32, self.tex.h as f32],
            turn: [sin, cos, zoom, 0.0],
            ..Params::default()
        };
        self.work.draw(&self.tex, Pass::Cube, self.full(), &params, [Some(&a), Some(&b), None], None);
    }

    fn finish(&mut self, level: f32) {
        let k = if level < 1.0 { (level.clamp(0.0, 1.0) * 256.0) as u32 } else { 256 };
        let out = self.work.texture(self.tex.w, self.tex.h, FORMAT);
        let params = Params { misc: [0.0, 0.0, 0.0, k as f32], ..Params::default() };
        self.work.draw(&out, Pass::Finish, self.full(), &params, [Some(&self.tex), None, None], None);
        self.tex = out;
    }
}

/// Composes on the graphics card for one mixer.
pub struct Painter {
    work: Rc<Work>,
    /// Source frames already on the card, by where they sit in memory.
    sources: HashMap<usize, Pic>,
    black: Option<Pic>,
    tables: HashMap<Vec<u8>, Rc<Tex>>,
    luts: HashMap<usize, (Arc<fx::Lut>, wgpu::Buffer)>,
    masks: HashMap<usize, (Arc<Vec<u8>>, Rc<Tex>)>,
}

impl Painter {
    /// The card if there is a suitable one, else why not.
    pub fn open(software: bool) -> Result<Painter, String> {
        let dev = Device::shared(software)?;
        Ok(Painter {
            work: Rc::new(Work::new(dev)),
            sources: HashMap::new(),
            black: None,
            tables: HashMap::new(),
            luts: HashMap::new(),
            masks: HashMap::new(),
        })
    }

    /// `open` for a real card, without waiting: opening one takes up to a
    /// second (its programs are built for it), so it happens on a thread of
    /// its own. `None` until it is done.
    pub fn open_soon() -> Option<Result<Painter, String>> {
        static STARTED: Once = Once::new();
        STARTED.call_once(|| {
            std::thread::spawn(Device::hardware);
        });
        HARDWARE.get()?;
        Some(Painter::open(false))
    }

    pub fn adapter_name(&self) -> &str {
        &self.work.dev.name
    }

    /// Set once the card has reported an error or gone away.
    pub fn failed(&self) -> Option<String> {
        self.work.dev.broken.lock().unwrap().clone()
    }

    /// The picture in memory, brought back from the card if it was made there.
    pub fn frame(&mut self, p: &Pic) -> Arc<Frame> {
        p.0.frame
            .get_or_init(|| {
                let tex = self.work.tex(p);
                let back = self.work.start_readback(&tex);
                let mut f = Frame::for_overwrite(tex.w, tex.h);
                self.work.finish_readback(back, &mut f);
                Arc::new(f)
            })
            .clone()
    }

    /// Sends a finished canvas on its way back, to be collected with
    /// `finish_readback` once something else has been done meanwhile.
    pub fn start_readback(&mut self, c: Canvas) -> Readback {
        // Frames the sources have moved on from, and nobody draws any more.
        self.sources.retain(|_, p| {
            Rc::strong_count(&p.0) > 1 || p.0.frame.get().is_some_and(|f| Arc::strong_count(f) > 1)
        });
        self.work.start_readback(&c.tex)
    }

    pub fn finish_readback(&mut self, back: Readback, out: &mut Frame) {
        self.work.finish_readback(back, out);
    }

    fn table(&mut self, bytes: Vec<u8>) -> Rc<Tex> {
        if let Some(t) = self.tables.get(&bytes) {
            return t.clone();
        }
        if self.tables.len() >= 16 {
            self.tables.clear();
        }
        let t = Rc::new(new_tex(&self.work.dev.device, 256, 1, FORMAT, 0));
        let layout =
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256 * 4), rows_per_image: Some(1) };
        self.work.dev.queue.write_texture(t.texture.as_image_copy(), &bytes, layout, t.extent());
        self.tables.insert(bytes, t.clone());
        t
    }

    fn lut(&mut self, lut: &Arc<fx::Lut>) -> wgpu::Buffer {
        let key = Arc::as_ptr(lut) as usize;
        if let Some((_, b)) = self.luts.get(&key) {
            return b.clone();
        }
        if self.luts.len() >= 8 {
            self.luts.clear();
        }
        let packed = lut.packed();
        let buffer = self.work.dev.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("lut"),
            contents: bytemuck::cast_slice(&packed),
            usage: wgpu::BufferUsages::STORAGE,
        });
        self.luts.insert(key, (lut.clone(), buffer.clone()));
        buffer
    }

    fn mask(&mut self, mask: &Arc<Vec<u8>>, w: usize, h: usize) -> Rc<Tex> {
        let key = Arc::as_ptr(mask) as usize;
        if let Some((_, t)) = self.masks.get(&key) {
            return t.clone();
        }
        if self.masks.len() >= 8 {
            self.masks.clear();
        }
        let t = Rc::new(new_tex(&self.work.dev.device, w, h, wgpu::TextureFormat::R8Unorm, 0));
        let layout = wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w as u32),
            rows_per_image: Some(h as u32),
        };
        self.work.dev.queue.write_texture(t.texture.as_image_copy(), mask, layout, t.extent());
        self.masks.insert(key, (mask.clone(), t.clone()));
        t
    }

    /// An effect over the whole picture, or only inside its crop.
    fn effect(
        &self,
        cur: &Rc<Tex>,
        crop: [f32; 4],
        pass: Pass,
        params: &Params,
        extra: Option<&Tex>,
        table: Option<&wgpu::Buffer>,
    ) -> Rc<Tex> {
        let (w, h) = (cur.w, cur.h);
        let out = self.work.texture(w, h, FORMAT);
        let area = if crop == [0.0; 4] {
            [0, 0, w, h]
        } else {
            self.work.copy(cur, &out);
            let (x0, x1, y0, y1) = fx::crop_bounds(w, h, crop);
            [x0, y0, x1, y1]
        };
        self.work.draw(&out, pass, area, params, [Some(cur), None, extra], table);
        out
    }

    /// Colour correction and key in one pass.
    fn colour_and_key(&mut self, cur: &Rc<Tex>, input: &Input, fill: Option<&Pic>) -> Rc<Tex> {
        let (w, h) = (cur.w, cur.h);
        let mut params = Params::default();
        let mut table = None;
        let c = &input.colour;
        if !c.is_identity() {
            let t = [fx::colour_table(c, 0), fx::colour_table(c, 1), fx::colour_table(c, 2)];
            table = Some(self.table((0..256).flat_map(|i| [t[0][i], t[1][i], t[2][i], 255]).collect()));
            params.flags[0] = 1;
            if c.saturation != 1.0 || c.hue != 0.0 {
                let m = fx::colour_matrix(c.saturation, c.hue);
                params.m0 = [m[0][0], m[0][1], m[0][2], 1.0];
                params.m1 = [m[1][0], m[1][1], m[1][2], 0.0];
                params.m2 = [m[2][0], m[2][1], m[2][2], 0.0];
            }
            params.key[3] = c.alpha.clamp(0.0, 1.0);
        }
        let key = &input.key;
        let colour = key.color.map(|v| v as f32);
        let mut fill_tex = None;
        match key.mode {
            KeyMode::Off => {}
            KeyMode::Colour => {
                params.flags[1] = 1;
                params.key[0] = key.tolerance * 441.0;
                params.key[1] = key.softness.max(0.002) * 441.0;
                params.key_colour = [colour[0], colour[1], colour[2], 0.0];
            }
            KeyMode::Chroma => {
                let (kb, kr) = crate::frame::chroma(colour[0], colour[1], colour[2]);
                params.flags[1] = 2;
                params.flags[2] = fx::spill_channel(key) as u32;
                params.key[0] = key.tolerance.max(0.0) * 180.0;
                params.key[1] = key.softness.max(0.01) * 180.0;
                params.key[2] = key.spill.clamp(0.0, 1.0);
                params.key_colour = [kb, kr, 0.0, 0.0];
            }
            KeyMode::Luma => {
                params.flags[1] = 3;
                params.key[0] = key.tolerance * 255.0;
                params.key[1] = key.softness.max(0.002) * 255.0;
            }
            KeyMode::Fill => {
                params.flags[1] = 4;
                fill_tex = fill.filter(|f| (f.0.w, f.0.h) == (w, h)).map(|f| self.work.tex(f));
                params.flags[3] = fill_tex.is_some() as u32;
            }
        }
        let out = self.work.texture(w, h, FORMAT);
        let images = [Some(&**cur), fill_tex.as_deref(), table.as_deref()];
        self.work.draw(&out, Pass::Process, [0, 0, w, h], &params, images, None);
        out
    }
}

impl canvas::Painter for Painter {
    type Canvas = Canvas;

    fn source(&mut self, f: Arc<Frame>) -> Pic {
        let key = Arc::as_ptr(&f) as usize;
        self.sources.entry(key).or_insert_with(|| Pic::of_frame(f)).clone()
    }

    fn canvas(&mut self, w: usize, h: usize) -> Canvas {
        let tex = self.work.texture(w, h, FORMAT);
        self.work.clear(&tex);
        Canvas { work: self.work.clone(), tex }
    }

    fn canvas_from(&mut self, p: &Pic) -> Canvas {
        let src = self.work.tex(p);
        let tex = self.work.texture(src.w, src.h, FORMAT);
        self.work.copy(&src, &tex);
        Canvas { work: self.work.clone(), tex }
    }

    fn done(&mut self, c: Canvas) -> Pic {
        Pic::of_tex(c.tex)
    }

    fn black(&mut self, w: usize, h: usize) -> Pic {
        match &self.black {
            Some(b) if (b.0.w, b.0.h) == (w, h) => b.clone(),
            _ => {
                let c = self.canvas(w, h);
                let b = self.done(c);
                self.black = Some(b.clone());
                b
            }
        }
    }

    fn process(&mut self, src: &Pic, input: &Input, fill: Option<&Pic>) -> Pic {
        let effects = || input.effects.iter().filter(|e| e.enabled);
        // Blurs work across many pixels at once: they stay on the processor.
        if effects().any(|e| matches!(e.kind, EffectKind::Blur | EffectKind::Sharpen)) {
            let f = self.frame(src);
            let fill = fill.map(|p| self.frame(p));
            return Pic::of_frame(Arc::new(canvas::processed(&f, input, fill.as_deref())));
        }
        let mut cur = self.work.tex(src);
        let (w, h) = (cur.w, cur.h);
        if !input.position.is_identity() || input.mirror {
            let out = self.work.texture(w, h, FORMAT);
            self.work.clear(&out);
            match fx::placement(w, h, &input.position, input.mirror) {
                Placed::Box { crop, x, y, w, h } => self.work.blit(&out, &cur, crop, x, y, w, h, 255, 0.0),
                Placed::Turned(t) => self.work.turn(&out, &cur, &t, 255),
            }
            cur = out;
        }
        if !input.colour.is_identity() || input.key.mode != KeyMode::Off {
            cur = self.colour_and_key(&cur, input, fill);
        }
        for e in effects() {
            match e.kind {
                EffectKind::Lut => {
                    let Some(lut) = fx::load_lut(&e.path) else { continue };
                    let table = self.lut(&lut);
                    let params = Params {
                        flags: [(e.strength.clamp(0.0, 1.0) * 256.0) as u32, 0, 0, 0],
                        ..Params::default()
                    };
                    cur = self.effect(&cur, e.crop, Pass::Lut, &params, None, Some(&table));
                }
                EffectKind::AlphaMask => {
                    let Some(mask) = fx::load_mask(&e.path, w, h) else { continue };
                    let mask = self.mask(&mask, w, h);
                    cur = self.effect(&cur, e.crop, Pass::Mask, &Params::default(), Some(&mask), None);
                }
                EffectKind::Blur | EffectKind::Sharpen => {}
            }
        }
        Pic::of_tex(cur)
    }
}
