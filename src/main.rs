use {
    clap::{Args, Parser},
    drm::node::NodeType,
    gbm::{BufferObjectFlags, Format::Xrgb8888},
    memfile::Seal,
    std::{collections::HashMap, fs::File, os::fd::AsFd, process},
    wayland_backend::client::ObjectId,
    wayland_client::{
        delegate_noop, event_created_child,
        protocol::{
            wl_buffer,
            wl_callback::{self, WlCallback},
            wl_compositor,
            wl_display::WlDisplay,
            wl_output::{self, WlOutput},
            wl_registry,
            wl_shm::{Format, WlShm},
            wl_shm_pool::WlShmPool,
            wl_subcompositor,
            wl_subsurface::WlSubsurface,
            wl_surface,
        },
        Connection, Dispatch, Proxy, QueueHandle,
    },
    wayland_protocols::{
        ext::{
            foreign_toplevel_list::v1::client::{
                ext_foreign_toplevel_handle_v1,
                ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
                ext_foreign_toplevel_list_v1::{
                    self, ExtForeignToplevelListV1, EVT_TOPLEVEL_OPCODE,
                },
            },
            image_capture_source::v1::client::{
                ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
                ext_image_capture_source_v1::ExtImageCaptureSourceV1,
                ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
            },
            image_copy_capture::v1::client::{
                ext_image_copy_capture_frame_v1,
                ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
                ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
                ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
            },
        },
        wp::{
            linux_dmabuf::zv1::client::{
                zwp_linux_buffer_params_v1::{Flags, ZwpLinuxBufferParamsV1},
                zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            },
            single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
            viewporter::client::{wp_viewport::WpViewport, wp_viewporter},
        },
        xdg::{
            decoration::zv1::client::{
                zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
                zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
            },
            shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
        },
    },
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_subcompositor::WlSubcompositor,
    wl_surface::WlSurface,
    wp_viewporter::WpViewporter,
    xdg_surface::XdgSurface,
    xdg_toplevel::XdgToplevel,
    xdg_wm_base::XdgWmBase,
};

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long)]
    stretch: bool,
    #[clap(flatten)]
    target: CliTarget,
    #[clap(long)]
    dmabuf: bool,
}

#[derive(Args, Debug)]
#[group(multiple = false)]
struct CliTarget {
    #[clap(long)]
    output: Option<String>,
    #[clap(long)]
    toplevel: Option<String>,
}

enum Target {
    None,
    Output(String),
    Toplevel(String),
}

fn main() {
    let cli = Cli::parse();
    let target = match cli.target.output {
        Some(s) => Target::Output(s),
        None => match cli.target.toplevel {
            Some(s) => Target::Toplevel(s),
            None => Target::None,
        },
    };

    let conn = Connection::connect_to_env().unwrap();

    let mut event_queue = conn.new_event_queue();
    let qhandle = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qhandle, ());

    display.sync(&qhandle, InitialRoundtrip);

    let mut state = State {
        display,
        target,
        fullscreen: cli.stretch,
        running: true,
        wm_base: None,
        wl_compositor: None,
        wl_shm: None,
        wp_viewporter: None,
        wl_subcompositor: None,
        wp_single_pixel_buffer_manager: None,
        zxdg_decoration_manager_v1: None,
        ext_output_image_capture_source_manager_v1: None,
        ext_foreign_toplevel_image_capture_source_manager_v1: None,
        ext_foreign_toplevel_list_v1: None,
        ext_image_copy_capture_manager_v1: None,
        zwp_linux_dmabuf_v1: None,
        objects: None,
        outputs: Default::default(),
        capture_size: (0, 0),
        foreign_toplevels: Default::default(),
        next_buffer_id: 0,
        dmabuf: cli.dmabuf,
        dmabuf_device: 0,
        gbm: None,
        dmabuf_modifiers: vec![],
        size: (1, 1),
        buffers: vec![],
    };

    while state.running {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }
}

struct State {
    display: WlDisplay,
    target: Target,
    running: bool,
    fullscreen: bool,
    wm_base: Option<XdgWmBase>,
    wl_compositor: Option<WlCompositor>,
    wl_shm: Option<WlShm>,
    wp_viewporter: Option<WpViewporter>,
    wl_subcompositor: Option<WlSubcompositor>,
    wp_single_pixel_buffer_manager: Option<WpSinglePixelBufferManagerV1>,
    zxdg_decoration_manager_v1: Option<ZxdgDecorationManagerV1>,
    ext_output_image_capture_source_manager_v1: Option<ExtOutputImageCaptureSourceManagerV1>,
    ext_foreign_toplevel_image_capture_source_manager_v1:
        Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    ext_foreign_toplevel_list_v1: Option<ExtForeignToplevelListV1>,
    ext_image_copy_capture_manager_v1: Option<ExtImageCopyCaptureManagerV1>,
    zwp_linux_dmabuf_v1: Option<ZwpLinuxDmabufV1>,
    objects: Option<Objects>,
    outputs: HashMap<ObjectId, Output>,
    capture_size: (i32, i32),
    foreign_toplevels: HashMap<ObjectId, ForeignToplevel>,
    next_buffer_id: u64,
    dmabuf: bool,
    dmabuf_device: libc::dev_t,
    gbm: Option<gbm::Device<File>>,
    dmabuf_modifiers: Vec<u64>,
    size: (i32, i32),
    buffers: Vec<Buffer>,
}

struct Output {
    output: WlOutput,
    name: String,
}

struct ForeignToplevel {
    handle: ExtForeignToplevelHandleV1,
    id: String,
    title: String,
    app_id: String,
}

struct Buffer {
    id: u64,
    buffer: WlBuffer,
    free: bool,
    ready: bool,
    size: (i32, i32),
    _bo_opt: Option<gbm::BufferObject<()>>,
}

struct Objects {
    root_surface: WlSurface,
    root_buffer: WlBuffer,
    root_viewport: WpViewport,
    _xdg_surface: XdgSurface,
    _xdg_toplevel: XdgToplevel,
    video_surface: WlSurface,
    video_subsurface: WlSubsurface,
    video_viewport: WpViewport,
    video_buffer_size: (i32, i32),
    session: ExtImageCopyCaptureSessionV1,
    frame: Option<ExtImageCopyCaptureFrameV1>,
}

impl State {
    fn render_frame(&mut self) {
        let obj = self.objects.as_mut().unwrap();
        if let Some(buffer) = self.buffers.iter_mut().find(|b| b.ready && b.free) {
            buffer.ready = false;
            buffer.free = false;
            obj.video_surface.attach(Some(&buffer.buffer), 0, 0);
            obj.video_surface
                .damage_buffer(0, 0, buffer.size.0, buffer.size.1);
            obj.video_buffer_size = buffer.size;
        }
        if self.fullscreen {
            let mut video_size = obj.video_buffer_size;
            if video_size.0 != self.size.0 {
                video_size.1 = video_size.1 * self.size.0 / video_size.0;
                video_size.0 = self.size.0;
            }
            if video_size.1 > self.size.1 {
                video_size.0 = video_size.0 * self.size.1 / video_size.1;
                video_size.1 = self.size.1;
            }
            obj.video_subsurface.set_position(
                (self.size.0 - video_size.0) / 2,
                (self.size.1 - video_size.1) / 2,
            );
            if video_size.0 > 0 && video_size.1 > 0 {
                obj.video_viewport
                    .set_destination(video_size.0, video_size.1);
            }
        }
        obj.video_surface.commit();
        obj.root_surface.attach(Some(&obj.root_buffer), 0, 0);
        if self.size.0 > 0 && self.size.1 > 0 {
            obj.root_viewport.set_destination(self.size.0, self.size.1);
        }
        obj.root_surface.commit();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match &interface[..] {
                "wl_compositor" => {
                    state.wl_compositor =
                        Some(registry.bind::<WlCompositor, _, _>(name, 4, qh, ()));
                }
                "wl_subcompositor" => {
                    state.wl_subcompositor =
                        Some(registry.bind::<WlSubcompositor, _, _>(name, 1, qh, ()));
                }
                "zxdg_decoration_manager_v1" => {
                    state.zxdg_decoration_manager_v1 =
                        Some(registry.bind::<ZxdgDecorationManagerV1, _, _>(name, 1, qh, ()));
                }
                "wl_shm" => {
                    state.wl_shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ()));
                }
                "wp_viewporter" => {
                    state.wp_viewporter =
                        Some(registry.bind::<WpViewporter, _, _>(name, 1, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind::<XdgWmBase, _, _>(name, 1, qh, ()));
                }
                "wl_output" => {
                    let o = registry.bind::<WlOutput, _, _>(name, 4, qh, ());
                    state.outputs.insert(
                        o.id(),
                        Output {
                            output: o,
                            name: "".to_string(),
                        },
                    );
                }
                "wp_single_pixel_buffer_manager_v1" => {
                    state.wp_single_pixel_buffer_manager =
                        Some(registry.bind::<WpSinglePixelBufferManagerV1, _, _>(name, 1, qh, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.ext_image_copy_capture_manager_v1 =
                        Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(name, 1, qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    state.zwp_linux_dmabuf_v1 =
                        Some(registry.bind::<ZwpLinuxDmabufV1, _, _>(name, 2, qh, ()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.ext_output_image_capture_source_manager_v1 =
                        Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                            name,
                            1,
                            qh,
                            (),
                        ));
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    state.ext_foreign_toplevel_image_capture_source_manager_v1 = Some(
                        registry.bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(
                            name,
                            1,
                            qh,
                            (),
                        ),
                    );
                }
                "ext_foreign_toplevel_list_v1" => {
                    state.ext_foreign_toplevel_list_v1 =
                        Some(registry.bind::<ExtForeignToplevelListV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

struct InitialRoundtrip;

impl Dispatch<WlCallback, InitialRoundtrip> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        _event: wl_callback::Event,
        _data: &InitialRoundtrip,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        state.display.sync(qhandle, SecondaryRoundtrip);
    }
}

impl State {
    fn print_toplevels(&self) {
        let mut handles: Vec<_> = self.foreign_toplevels.values().collect();
        handles.sort_by_cached_key(|t| &t.app_id);

        eprintln!("Available toplevels:");
        for handle in handles {
            println!("  {} - {} - {}", handle.id, handle.app_id, handle.title);
        }
    }

    fn print_outputs(&self) {
        let mut outputs: Vec<_> = self.outputs.values().collect();
        outputs.sort_by_cached_key(|t| &t.name);

        eprintln!("Available outputs:");
        for output in outputs {
            println!("  {}", output.name);
        }
    }
}

struct SecondaryRoundtrip;

impl Dispatch<WlCallback, SecondaryRoundtrip> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        _event: wl_callback::Event,
        _data: &SecondaryRoundtrip,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        let source = match &state.target {
            Target::None => {
                state.print_outputs();
                state.print_toplevels();
                process::exit(0);
            }
            Target::Output(n) => {
                let Some(o) = state.outputs.values().find(|o| &o.name == n) else {
                    eprintln!("Unknown output {n}");
                    state.print_outputs();
                    process::exit(1);
                };
                let oicsm = state
                    .ext_output_image_capture_source_manager_v1
                    .as_ref()
                    .expect("ext_output_image_capture_source_manager_v1");
                oicsm.create_source(&o.output, qhandle, ())
            }
            Target::Toplevel(id) => {
                let Some(o) = state.foreign_toplevels.values().find(|o| &o.id == id) else {
                    eprintln!("Unknown toplevel {id}");
                    state.print_toplevels();
                    process::exit(1);
                };
                let fticsm = state
                    .ext_foreign_toplevel_image_capture_source_manager_v1
                    .as_ref()
                    .expect("ext_foreign_toplevel_image_capture_source_manager_v1");
                fticsm.create_source(&o.handle, qhandle, ())
            }
        };
        let comp = state.wl_compositor.as_ref().expect("wl_compositor");
        let wm_base = state.wm_base.as_ref().expect("wm_base");
        let sub = state.wl_subcompositor.as_ref().expect("wl_subcompositor");
        let viewporter = state.wp_viewporter.as_ref().expect("wp_viewporter");
        let spbm = state
            .wp_single_pixel_buffer_manager
            .as_ref()
            .expect("wp_single_pixel_buffer_manager");
        let iccm = state
            .ext_image_copy_capture_manager_v1
            .as_ref()
            .expect("ext_image_copy_capture_manager_v1");
        let root_surface = comp.create_surface(qhandle, ());
        let root_viewport = viewporter.get_viewport(&root_surface, qhandle, ());
        let root_buffer = spbm.create_u32_rgba_buffer(0, 0, 0, !0, qhandle, None);
        let video_surface = comp.create_surface(qhandle, ());
        let video_subsurface = sub.get_subsurface(&video_surface, &root_surface, qhandle, ());
        let video_viewport = viewporter.get_viewport(&video_surface, qhandle, ());
        let xdg_surface = wm_base.get_xdg_surface(&root_surface, qhandle, ());
        let xdg_toplevel = xdg_surface.get_toplevel(qhandle, ());
        if let Some(decoman) = state.zxdg_decoration_manager_v1.as_ref() {
            let decorations = decoman.get_toplevel_decoration(&xdg_toplevel, qhandle, ());
            decorations.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
        root_surface.commit();
        let session = iccm.create_session(&source, Options::all(), qhandle, ());
        source.destroy();
        state.objects = Some(Objects {
            root_surface,
            root_buffer,
            root_viewport,
            _xdg_surface: xdg_surface,
            _xdg_toplevel: xdg_toplevel,
            video_surface,
            video_subsurface,
            video_viewport,
            video_buffer_size: (1, 1),
            session,
            frame: None,
        });
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WlSubsurface);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WlSubcompositor);
delegate_noop!(State: ignore ZxdgDecorationManagerV1);
delegate_noop!(State: ignore ZxdgToplevelDecorationV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore WpSinglePixelBufferManagerV1);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

impl Dispatch<WlBuffer, Option<u64>> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlBuffer,
        _event: wl_buffer::Event,
        data: &Option<u64>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let Some(idx) = *data {
            for buffer in &mut state.buffers {
                if buffer.id == idx {
                    buffer.free = true;
                }
            }
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial, .. } = event {
            xdg_surface.ack_configure(serial);
            state.render_frame();
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use xdg_toplevel::Event;

        match event {
            Event::Configure { width, height, .. } => {
                state.size = (width, height);
            }
            Event::Close => state.running = false,
            Event::ConfigureBounds { .. } => {}
            Event::WmCapabilities { .. } => {}
            _ => {}
        }
    }
}

impl State {
    fn capture_frame(&mut self, qh: &QueueHandle<Self>) {
        let Some(obj) = &mut self.objects else {
            return;
        };
        if obj.frame.is_some() {
            return;
        }
        if self.capture_size.0 == 0 || self.capture_size.1 == 0 {
            return;
        }
        self.buffers.retain(|b| {
            let retain = b.size == self.capture_size;
            if !retain {
                b.buffer.destroy();
            }
            retain
        });
        let mut bo_opt = None;
        let b = self.buffers.iter_mut().find(|b| b.free);
        let b = match b {
            Some(b) => b,
            _ => {
                let buffer = if self.dmabuf {
                    let bo = self
                        .gbm
                        .as_ref()
                        .unwrap()
                        .create_buffer_object_with_modifiers2::<()>(
                            self.capture_size.0 as _,
                            self.capture_size.1 as _,
                            Xrgb8888,
                            self.dmabuf_modifiers.iter().map(|&m| m.into()),
                            BufferObjectFlags::RENDERING,
                        )
                        .expect("allocate dmabuf");
                    let dmabuf = self
                        .zwp_linux_dmabuf_v1
                        .as_ref()
                        .expect("zwp_linux_dmabuf_v1");
                    let params = dmabuf.create_params(qh, ());
                    for i in 0..bo.plane_count().expect("plane_count") {
                        let modifier: u64 = bo.modifier().unwrap().into();
                        params.add(
                            bo.fd_for_plane(i as _).unwrap().as_fd(),
                            i,
                            bo.offset(i as _).unwrap(),
                            bo.stride_for_plane(i as _).unwrap(),
                            (modifier >> 32) as _,
                            modifier as _,
                        );
                    }
                    let buffer = params.create_immed(
                        self.capture_size.0,
                        self.capture_size.1,
                        Xrgb8888 as _,
                        Flags::empty(),
                        qh,
                        Some(self.next_buffer_id),
                    );
                    params.destroy();
                    bo_opt = Some(bo);
                    buffer
                } else {
                    let memfile = memfile::MemFile::create_sealable("wl_shm").unwrap();
                    let size = self.capture_size.0 * self.capture_size.1 * 4;
                    memfile.set_len(size as _).unwrap();
                    memfile.add_seal(Seal::Shrink).unwrap();
                    let shm = self.wl_shm.as_ref().expect("wl_shm");
                    let pool = shm.create_pool(memfile.as_fd(), size, qh, ());
                    let buffer = pool.create_buffer(
                        0,
                        self.capture_size.0,
                        self.capture_size.1,
                        self.capture_size.0 * 4,
                        Format::Argb8888,
                        qh,
                        Some(self.next_buffer_id),
                    );
                    pool.destroy();
                    buffer
                };
                let b = Buffer {
                    id: self.next_buffer_id,
                    buffer,
                    free: true,
                    ready: false,
                    size: self.capture_size,
                    _bo_opt: bo_opt,
                };
                self.next_buffer_id += 1;
                self.buffers.push(b);
                self.buffers.last_mut().unwrap()
            }
        };
        let frame = obj.session.create_frame(qh, b.id);
        frame.attach_buffer(&b.buffer);
        frame.damage_buffer(0, 0, b.size.0, b.size.1);
        frame.capture();
        obj.frame = Some(frame);
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;

        match event {
            Event::BufferSize { width, height } => {
                state.capture_size = (width as _, height as _);
            }
            Event::DmabufDevice { device } => {
                state.dmabuf_device = bytemuck::pod_read_unaligned(&device);
                if state.dmabuf {
                    let dev = drm::node::dev_path(state.dmabuf_device, NodeType::Render)
                        .expect("dmabuf device");
                    let fd = File::options()
                        .read(true)
                        .write(true)
                        .open(dev)
                        .expect("dmabuf device");
                    let gbm = gbm::Device::new(fd).expect("gbm");
                    state.gbm = Some(gbm);
                }
            }
            Event::DmabufFormat { format, modifiers } => {
                if format == Xrgb8888 as u32 {
                    state.dmabuf_modifiers = bytemuck::pod_collect_to_vec(&modifiers);
                }
            }
            Event::Stopped => {
                state.running = false;
            }
            Event::Done => {
                state.capture_frame(qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        id: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;

        let Some(obj) = &mut state.objects else {
            return;
        };
        let Some(buffer) = state.buffers.iter_mut().find(|b| b.id == *id) else {
            return;
        };

        match event {
            Event::Transform { .. } => {}
            Event::Damage { .. } => {}
            Event::PresentationTime { .. } => {}
            Event::Ready => {
                buffer.ready = true;
                obj.frame.take();
                state.render_frame();
                frame.destroy();
                state.capture_frame(qh);
            }
            Event::Failed { reason } => {
                eprintln!("failed: {:?}", reason);
                obj.frame.take();
                frame.destroy();
                state.capture_frame(qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtForeignToplevelListV1,
        _: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;

        let tl = state
            .foreign_toplevels
            .entry(handle.id())
            .or_insert_with(|| ForeignToplevel {
                handle: handle.clone(),
                id: "".to_string(),
                title: "".to_string(),
                app_id: "".to_string(),
            });

        match event {
            Event::Closed => {
                state.foreign_toplevels.remove(&handle.id());
                handle.destroy();
            }
            Event::Title { title } => {
                tl.title = title;
            }
            Event::AppId { app_id } => {
                tl.app_id = app_id;
            }
            Event::Identifier { identifier } => {
                tl.id = identifier;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_output::Event;

        let o = state.outputs.get_mut(&output.id()).unwrap();

        match event {
            Event::Name { name } => {
                o.name = name;
            }
            _ => {}
        }
    }
}
