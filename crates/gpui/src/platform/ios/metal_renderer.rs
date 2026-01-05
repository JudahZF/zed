//! iOS-specific Metal renderer.
//!
//! This is a copy of the macOS Metal renderer with iOS-specific adaptations:
//! - Cocoa types (NSSize, NSUInteger, etc.) are defined locally instead of imported
//! - core_video is disabled (no video texture rendering on iOS for Phase 1)
//! - draw_surfaces is stubbed as a no-op

use super::metal_atlas::MetalAtlas;
use crate::{
    AtlasTextureId, Background, Bounds, ContentMask, DevicePixels, MonochromeSprite, PaintSurface,
    Path, PathVertex, PolychromeSprite, PrimitiveBatch, Quad, ScaledPixels, Scene, Shadow, Size,
    Surface, Underline, point, size,
};
use anyhow::Result;
use block::ConcreteBlock;

// iOS compatibility - provide Cocoa-equivalent types without cocoa crate
#[allow(non_camel_case_types, dead_code)]
mod cocoa_compat {
    pub use objc::runtime::{NO, YES};
    pub type NSUInteger = usize;

    #[repr(C)]
    #[derive(Copy, Clone, Debug, Default)]
    pub struct NSSize {
        pub width: f64,
        pub height: f64,
    }

    impl NSSize {
        /// Creates an NSSize with the specified width and height.
        ///
        /// # Examples
        ///
        /// ```
        /// let s = NSSize::new(10.0, 20.0);
        /// assert_eq!(s.width, 10.0);
        /// assert_eq!(s.height, 20.0);
        /// ```
        pub fn new(width: f64, height: f64) -> Self {
            NSSize { width, height }
        }
    }

    bitflags::bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct AutoresizingMask: NSUInteger {
            const NOT_SIZABLE = 0;
            const MIN_X_MARGIN = 1 << 0;
            const WIDTH_SIZABLE = 1 << 1;
            const MAX_X_MARGIN = 1 << 2;
            const MIN_Y_MARGIN = 1 << 3;
            const HEIGHT_SIZABLE = 1 << 4;
            const MAX_Y_MARGIN = 1 << 5;
        }
    }
}

use cocoa_compat::{AutoresizingMask, NSSize, NSUInteger, NO, YES};

use core_foundation::base::TCFType;

use foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    CAMetalLayer, CommandQueue, MTLDrawPrimitivesIndirectArguments, MTLPixelFormat,
    MTLResourceOptions, NSRange,
};
use objc::{self, msg_send, sel, sel_impl};
use parking_lot::Mutex;
use std::{cell::Cell, ffi::c_void, mem, ptr, sync::Arc};

// Exported to metal
pub(crate) type PointF = crate::Point<f32>;

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));

pub type Context = Arc<Mutex<InstanceBufferPool>>;
pub type Renderer = MetalRenderer;

/// Create a new MetalRenderer configured for iOS.
///
/// The native window/view, bounds, and transparency parameters are accepted for
/// API compatibility but are not used by this platform implementation; only
/// `context` is required to initialize the renderer.
///
/// # Parameters
///
/// - `context`: shared instance buffer pool used by the renderer.
///
/// # Returns
///
/// A newly constructed `MetalRenderer`.
///
/// # Safety
///
/// The function is `unsafe` because it accepts raw native pointers; callers must
/// uphold any platform pointer invariants when providing them, even though the
/// current implementation does not use those pointers.
///
/// # Examples
///
/// ```
/// use std::ptr;
/// // `ctx` would be created elsewhere as `Arc<Mutex<InstanceBufferPool>>`
/// # let ctx = crate::Context::default();
/// let renderer = unsafe {
///     new_renderer(ctx, ptr::null_mut(), ptr::null_mut(), crate::Size { width: 0.0, height: 0.0 }, false)
/// };
/// ```
pub unsafe fn new_renderer(
    context: self::Context,
    _native_window: *mut c_void,
    _native_view: *mut c_void,
    _bounds: crate::Size<f32>,
    _transparent: bool,
) -> Renderer {
    MetalRenderer::new(context)
}

pub(crate) struct InstanceBufferPool {
    buffer_size: usize,
    buffers: Vec<metal::Buffer>,
}

impl Default for InstanceBufferPool {
    /// Creates an InstanceBufferPool initialized with a 2 MiB buffer size and an empty buffer cache.
    ///
    /// The default pool uses a buffer_size of 2 * 1024 * 1024 bytes and no cached buffers.
    ///
    /// # Examples
    ///
    /// ```
    /// let pool = crate::platform::ios::metal_renderer::InstanceBufferPool::default();
    /// assert_eq!(pool.buffer_size, 2 * 1024 * 1024);
    /// assert!(pool.buffers.is_empty());
    /// ```
    fn default() -> Self {
        Self {
            buffer_size: 2 * 1024 * 1024,
            buffers: Vec::new(),
        }
    }
}

pub(crate) struct InstanceBuffer {
    metal_buffer: metal::Buffer,
    size: usize,
}

impl InstanceBufferPool {
    /// Resets the instance buffer pool to use a new per-buffer size and discards any cached buffers.
    ///
    /// This updates the pool's `buffer_size` to `buffer_size` and clears the internal buffer cache,
    /// dropping any stored `metal::Buffer` instances.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut pool = InstanceBufferPool::default();
    /// pool.reset(4096);
    /// assert_eq!(pool.buffer_size, 4096);
    /// assert!(pool.buffers.is_empty());
    /// ```
    pub(crate) fn reset(&mut self, buffer_size: usize) {
        self.buffer_size = buffer_size;
        self.buffers.clear();
    }

    /// Acquire an `InstanceBuffer` from the pool, allocating a new Metal buffer if none are cached.
    ///
    /// If a cached buffer is available the pool will return it; otherwise a new buffer of the
    /// pool's configured `buffer_size` is created with `StorageModeShared`.
    ///
    /// # Parameters
    ///
    /// - `device`: Metal device used to allocate a new buffer when the pool is empty.
    ///
    /// # Returns
    ///
    /// An `InstanceBuffer` containing a Metal buffer whose `size` equals the pool's `buffer_size`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use gpui::platform::ios::metal_renderer::InstanceBufferPool;
    /// # use metal::Device;
    /// # let device: Device = /* obtain device */ unimplemented!();
    /// let mut pool = InstanceBufferPool::default();
    /// let buf = pool.acquire(&device);
    /// assert_eq!(buf.size, pool.buffer_size);
    /// ```
    pub(crate) fn acquire(&mut self, device: &metal::Device) -> InstanceBuffer {
        let buffer = self.buffers.pop().unwrap_or_else(|| {
            device.new_buffer(
                self.buffer_size as u64,
                MTLResourceOptions::StorageModeShared,
            )
        });
        InstanceBuffer {
            metal_buffer: buffer,
            size: self.buffer_size,
        }
    }

    /// Returns an instance buffer to the pool if its size matches the pool's configured buffer size.
    ///
    /// If `buffer.size` equals the pool's `buffer_size`, the underlying Metal buffer is pushed
    /// onto the internal free list for reuse; otherwise the buffer is dropped.
    ///
    /// # Parameters
    ///
    /// - `buffer`: The instance buffer being returned to the pool.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// let mut pool = InstanceBufferPool::default();
    /// // Create a buffer with the same size as the pool (metal::Buffer is created unsafely for example)
    /// let buf = InstanceBuffer {
    ///     size: pool.buffer_size,
    ///     metal_buffer: unsafe { std::mem::zeroed() },
    /// };
    /// pool.release(buf);
    /// assert_eq!(pool.buffers.len(), 1);
    /// ```
    pub(crate) fn release(&mut self, buffer: InstanceBuffer) {
        if buffer.size == self.buffer_size {
            self.buffers.push(buffer.metal_buffer)
        }
    }
}

pub(crate) struct MetalRenderer {
    device: metal::Device,
    layer: metal::MetalLayer,
    presents_with_transaction: bool,
    command_queue: CommandQueue,
    path_pipeline_state: metal::RenderPipelineState,
    shadows_pipeline_state: metal::RenderPipelineState,
    quads_pipeline_state: metal::RenderPipelineState,
    underlines_pipeline_state: metal::RenderPipelineState,
    monochrome_sprites_pipeline_state: metal::RenderPipelineState,
    polychrome_sprites_pipeline_state: metal::RenderPipelineState,
    surfaces_pipeline_state: metal::RenderPipelineState,
    unit_vertices: metal::Buffer,
    #[allow(clippy::arc_with_non_send_sync)]
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    sprite_atlas: Arc<MetalAtlas>,
    sample_count: u64,
    msaa_texture: Option<metal::Texture>,
}

impl MetalRenderer {
    /// Creates a new MetalRenderer configured for iOS using the provided instance buffer pool.
    ///
    /// The returned renderer is initialized with a system Metal device, a configured CAMetalLayer,
    /// compiled shader library, pipeline states for all supported primitive types, a unit vertex
    /// buffer, a sprite atlas, and an optional MSAA texture chosen based on device capabilities.
    ///
    /// # Parameters
    ///
    /// - `instance_buffer_pool` — shared pool used to allocate per-frame instance buffers.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    /// // assume InstanceBufferPool and MetalRenderer are in scope
    /// let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
    /// let renderer = MetalRenderer::new(pool);
    /// ```
    pub fn new(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>) -> Self {
        // On iOS, there is only one GPU
        let Some(device) = metal::Device::system_default() else {
            log::error!("unable to access a compatible graphics device");
            std::process::exit(1);
        };

        let layer = metal::MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_opaque(false);
        layer.set_maximum_drawable_count(3);
        unsafe {
            let _: () = msg_send![&*layer, setAllowsNextDrawableTimeout: NO];
            let _: () = msg_send![&*layer, setNeedsDisplayOnBoundsChange: YES];
            let _: () = msg_send![
                &*layer,
                setAutoresizingMask: AutoresizingMask::WIDTH_SIZABLE
                    | AutoresizingMask::HEIGHT_SIZABLE
            ];
        }
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: PointF) -> u64 {
            let mut output = point.y.to_bits() as u64;
            output <<= 32;
            output |= point.x.to_bits() as u64;
            output
        }

        let unit_vertices = [
            to_float2_bits(point(0., 0.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(1., 1.)),
        ];
        let unit_vertices = device.new_buffer_with_data(
            unit_vertices.as_ptr() as *const c_void,
            mem::size_of_val(&unit_vertices) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let sample_count = [4, 2, 1]
            .into_iter()
            .find(|count| device.supports_texture_sample_count(*count))
            .unwrap_or(1);

        let path_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "paths",
            "path_vertex",
            "path_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let shadows_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "shadows",
            "shadow_vertex",
            "shadow_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let quads_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "quads",
            "quad_vertex",
            "quad_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let underlines_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "underlines",
            "underline_vertex",
            "underline_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let monochrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "monochrome_sprites",
            "monochrome_sprite_vertex",
            "monochrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let polychrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "polychrome_sprites",
            "polychrome_sprite_vertex",
            "polychrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );
        let surfaces_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "surfaces",
            "surface_vertex",
            "surface_fragment",
            MTLPixelFormat::BGRA8Unorm,
            sample_count,
        );

        let command_queue = device.new_command_queue();
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone()));
        let msaa_texture = create_msaa_texture(&device, &layer, sample_count);

        Self {
            device,
            layer,
            presents_with_transaction: false,
            command_queue,
            path_pipeline_state,
            shadows_pipeline_state,
            quads_pipeline_state,
            underlines_pipeline_state,
            monochrome_sprites_pipeline_state,
            polychrome_sprites_pipeline_state,
            surfaces_pipeline_state,
            unit_vertices,
            instance_buffer_pool,
            sprite_atlas,
            sample_count,
            msaa_texture,
        }
    }

    /// Returns a reference to the renderer's Metal layer.
    ///
    /// # Examples
    ///
    /// ```
    /// fn inspect_layer(r: &MetalRenderer) {
    ///     let _layer_ref = r.layer();
    /// }
    /// ```
    pub fn layer(&self) -> &metal::MetalLayerRef {
        &self.layer
    }

    /// Accesses the underlying CAMetalLayer as a raw pointer.
    ///
    /// The pointer refers to the CAMetalLayer owned by this renderer.
    /// The caller must ensure correct usage and safety when dereferencing.
    ///
    /// # Examples
    ///
    /// ```
    /// let renderer = unsafe { new_renderer(/* args omitted for example */) };
    /// let ptr = renderer.layer_ptr();
    /// assert!(!ptr.is_null());
    /// ```
    pub fn layer_ptr(&self) -> *mut CAMetalLayer {
        self.layer.as_ptr()
    }

    /// Returns a reference to the renderer's shared sprite atlas.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain a reference to the atlas
    /// let atlas_ref: &std::sync::Arc<MetalAtlas> = renderer.sprite_atlas();
    /// // Use `atlas_ref` for read-only access or clone the Arc for shared ownership:
    /// let _cloned = std::sync::Arc::clone(atlas_ref);
    /// ```
    pub fn sprite_atlas(&self) -> &Arc<MetalAtlas> {
        &self.sprite_atlas
    }

    /// Toggle whether the renderer presents drawables using Core Animation transactions.
    ///
    /// This updates the renderer's `presents_with_transaction` flag and applies the same setting
    /// to the underlying `CAMetalLayer`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Enable presentation via transactions
    /// renderer.set_presents_with_transaction(true);
    /// ```
    pub fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.presents_with_transaction = presents_with_transaction;
        self.layer
            .set_presents_with_transaction(presents_with_transaction);
    }

    /// Update the renderer's drawable size and recreate the multisample (MSAA) texture accordingly.
    ///
    /// Updates the underlying CAMetalLayer drawable size to `size` and recreates the MSAA texture
    /// to match the new dimensions and the renderer's current sample count.
    ///
    /// # Examples
    ///
    /// ```
    /// // `renderer` is a mutable MetalRenderer; `size` is a Size<DevicePixels>.
    /// // This adjusts the layer and MSAA texture to the new size.
    /// renderer.update_drawable_size(size);
    /// ```
    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        let size = NSSize {
            width: size.width.0 as f64,
            height: size.height.0 as f64,
        };
        unsafe {
            let _: () = msg_send![
                self.layer(),
                setDrawableSize: size
            ];
        }

        self.msaa_texture = create_msaa_texture(&self.device, &self.layer, self.sample_count);
    }

    /// Update the renderer's transparency state.
    ///
    /// On iOS this is currently a no-op and does not affect rendering.
    ///
    /// - `transparent`: whether the renderer's background should be treated as transparent.
    ///
    /// # Examples
    ///
    /// ```
    /// // Enable transparency (no-op on iOS)
    /// renderer.update_transparency(true);
    /// ```
    pub fn update_transparency(&self, _transparent: bool) {
        // todo(ios)?
    }

    /// Performs platform-specific teardown for the renderer.
    ///
    /// On iOS this is currently a no-op; calling this method has no effect.
    ///
    /// # Examples
    ///
    /// ```
    /// // assume `renderer` is a MetalRenderer instance
    /// renderer.destroy();
    /// ```
    pub fn destroy(&self) {
        // nothing to do
    }

    /// Renders the provided Scene into the renderer's current CAMetalLayer drawable.
    ///
    /// This obtains the layer's next drawable, encodes drawing commands for the scene,
    /// commits the command buffer, and presents the drawable. If encoding fails due to
    /// insufficient instance buffer capacity, the renderer will enlarge its instance buffer
    /// and retry rendering until a configured maximum size is reached.
    ///
    /// # Parameters
    ///
    /// - `scene` — The scene to render.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given a configured `renderer: MetalRenderer` and a prepared `scene: Scene`:
    /// renderer.draw(&scene);
    /// ```
    pub fn draw(&mut self, scene: &Scene) {
        let layer = self.layer.clone();
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = if let Some(drawable) = layer.next_drawable() {
            drawable
        } else {
            log::error!(
                "failed to retrieve next drawable, drawable size: {:?}",
                viewport_size
            );
            return;
        };

        loop {
            let mut instance_buffer = self.instance_buffer_pool.lock().acquire(&self.device);

            let command_buffer =
                self.draw_primitives(scene, &mut instance_buffer, drawable, viewport_size);

            match command_buffer {
                Ok(command_buffer) => {
                    let instance_buffer_pool = self.instance_buffer_pool.clone();
                    let instance_buffer = Cell::new(Some(instance_buffer));
                    let block = ConcreteBlock::new(move |_| {
                        if let Some(instance_buffer) = instance_buffer.take() {
                            instance_buffer_pool.lock().release(instance_buffer);
                        }
                    });
                    let block = block.copy();
                    command_buffer.add_completed_handler(&block);

                    if self.presents_with_transaction {
                        command_buffer.commit();
                        command_buffer.wait_until_scheduled();
                        drawable.present();
                    } else {
                        command_buffer.present_drawable(drawable);
                        command_buffer.commit();
                    }
                    return;
                }
                Err(err) => {
                    log::error!(
                        "failed to render: {}. retrying with larger instance buffer size",
                        err
                    );
                    let mut instance_buffer_pool = self.instance_buffer_pool.lock();
                    let buffer_size = instance_buffer_pool.buffer_size;
                    if buffer_size >= 256 * 1024 * 1024 {
                        log::error!("instance buffer size grew too large: {}", buffer_size);
                        break;
                    }
                    instance_buffer_pool.reset(buffer_size * 2);
                    log::info!(
                        "increased instance buffer size to {}",
                        instance_buffer_pool.buffer_size
                    );
                }
            }
        }
    }

    /// Encode the scene's primitive batches into a Metal command buffer for the provided drawable.
    ///
    /// The function creates a render pass (using an MSAA resolve target when available), sets the viewport,
    /// iterates the scene's primitive batches and dispatches each to its corresponding draw method, and
    /// finalizes the command encoder. On success it marks the instance buffer's modified range and returns
    /// the prepared `metal::CommandBuffer`. If any batch cannot be encoded because the instance buffer is
    /// too small, encoding is ended and an error describing the scene contents is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain a renderer, scene, instance buffer and current drawable from your context, then:
    /// // let cmd_buf = renderer.draw_primitives(&scene, &mut instance_buffer, &drawable, viewport_size)?;
    /// // command_queue.submit_command_buffer(cmd_buf);
    /// ```
    fn draw_primitives(
        &mut self,
        scene: &Scene,
        instance_buffer: &mut InstanceBuffer,
        drawable: &metal::MetalDrawableRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue.new_command_buffer();
        let mut instance_offset = 0;
        let render_pass_descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .unwrap();

        if let Some(msaa_texture_ref) = self.msaa_texture.as_deref() {
            color_attachment.set_texture(Some(msaa_texture_ref));
            color_attachment.set_load_action(metal::MTLLoadAction::Clear);
            color_attachment.set_store_action(metal::MTLStoreAction::MultisampleResolve);
            color_attachment.set_resolve_texture(Some(drawable.texture()));
        } else {
            color_attachment.set_load_action(metal::MTLLoadAction::Clear);
            color_attachment.set_texture(Some(drawable.texture()));
            color_attachment.set_store_action(metal::MTLStoreAction::Store);
        }

        let alpha = if self.layer.is_opaque() { 1. } else { 0. };
        color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., alpha));
        let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);

        command_encoder.set_viewport(metal::MTLViewport {
            originX: 0.0,
            originY: 0.0,
            width: i32::from(viewport_size.width) as f64,
            height: i32::from(viewport_size.height) as f64,
            znear: 0.0,
            zfar: 1.0,
        });

        for batch in scene.batches() {
            let ok = match batch {
                PrimitiveBatch::Shadows(shadows) => self.draw_shadows(
                    shadows,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Quads(quads) => self.draw_quads(
                    quads,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Paths(paths) => self.draw_paths(
                    paths,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Underlines(underlines) => self.draw_underlines(
                    underlines,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_monochrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_polychrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Surfaces(surfaces) => self.draw_surfaces(
                    surfaces,
                    instance_buffer,
                    &mut instance_offset,
                    viewport_size,
                    command_encoder,
                ),
            };

            if !ok {
                command_encoder.end_encoding();
                anyhow::bail!(
                    "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),
                );
            }
        }

        command_encoder.end_encoding();

        instance_buffer.metal_buffer.did_modify_range(NSRange {
            location: 0,
            length: instance_offset as u64,
        });
        Ok(command_buffer.to_owned())
    }

    /// Render a list of shadows by writing instance data into the provided instance buffer and issuing an instanced draw.
    ///
    /// Writes shadow instance data into `instance_buffer` at `*instance_offset`, binds the appropriate vertex/fragment buffers
    /// and viewport size, then issues instanced triangle draws for each shadow.
    ///
    /// # Parameters
    ///
    /// - `shadows`: slice of shadow instances to render.
    /// - `instance_buffer`: GPU-backed buffer used to store per-instance data for this frame.
    /// - `instance_offset`: byte offset into `instance_buffer` where instance data will be written; advanced on success.
    /// - `viewport_size`: current viewport size in device pixels.
    /// - `command_encoder`: active render command encoder used to record draw calls.
    ///
    /// # Returns
    ///
    /// `true` if all shadow data fit into the instance buffer and the draw call was recorded, `false` if there was insufficient
    /// space in `instance_buffer` (no draw is recorded in that case).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assume `renderer`, `encoder`, `mut instance_buffer`, and `mut offset` are set up appropriately.
    /// let shadows: Vec<Shadow> = vec![/* ... */];
    /// let viewport = Size::new(1024.0, 768.0);
    /// let ok = renderer.draw_shadows(&shadows, &mut instance_buffer, &mut offset, viewport, &encoder);
    /// assert!(ok || offset > 0);
    /// ```
    fn draw_shadows(
        &self,
        shadows: &[Shadow],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if shadows.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.shadows_pipeline_state);
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            ShadowInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let shadow_bytes_len = mem::size_of_val(shadows);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + shadow_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                shadows.as_ptr() as *const u8,
                buffer_contents,
                shadow_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            shadows.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    /// Renders a sequence of quads by writing their instance data into the provided instance buffer and issuing an instanced draw.
    ///
    /// On success, advances `instance_offset` by the number of bytes written for `quads`.
    ///
    /// Returns `true` on success, `false` if the instance buffer does not have enough space to hold the quad data.
    ///
    /// # Parameters
    ///
    /// - `quads`: slice of quad instance data to render.
    /// - `instance_buffer`: mutable instance buffer into which quad data will be copied.
    /// - `instance_offset`: byte offset within `instance_buffer` where quad data will be written; advanced on success.
    /// - `viewport_size`: current viewport size passed to the vertex shader.
    /// - `command_encoder`: render command encoder used to configure pipeline state and issue the draw.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use crate::{MetalRenderer, InstanceBuffer, Quad, Size, DevicePixels};
    /// # let renderer: MetalRenderer = unimplemented!();
    /// # let mut instance_buffer: InstanceBuffer = unimplemented!();
    /// # let encoder = unimplemented!();
    /// let quads: &[Quad] = &[];
    /// let mut offset = 0usize;
    /// let viewport = Size::new(800u32, 600u32);
    /// assert!(renderer.draw_quads(quads, &mut instance_buffer, &mut offset, viewport, &encoder));
    /// ```
    fn draw_quads(
        &self,
        quads: &[Quad],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if quads.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.quads_pipeline_state);
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            QuadInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let quad_bytes_len = mem::size_of_val(quads);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + quad_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(quads.as_ptr() as *const u8, buffer_contents, quad_bytes_len);
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            quads.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    /// Renders a sequence of vector paths into the current render pass using indirect draws.
    ///
    /// This writes each path's vertex data, per-path sprite data, and an array of indirect draw
    /// commands into the provided instance buffer, binds those buffers to the encoder, and issues
    /// one indirect draw call per path. If the instance buffer does not have enough space for the
    /// required data, the function returns `false` and performs no draw calls.
    ///
    /// # Parameters
    ///
    /// - `paths`: slice of paths to render. An empty slice is a no-op and returns `true`.
    /// - `instance_buffer`: a mutable instance buffer whose underlying Metal buffer will be written to.
    /// - `instance_offset`: byte offset within `instance_buffer` where data should be written; updated
    ///   on success to point just past the written data.
    /// - `viewport_size`: current viewport size provided to the vertex shader.
    /// - `command_encoder`: active render command encoder used to record draw calls.
    ///
    /// # Returns
    ///
    /// `true` if all path data fit into the instance buffer and draw commands were recorded,
    /// `false` if the instance buffer did not have enough space (no draws recorded).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Render nothing is a safe, trivial usage.
    /// # use gpui::platform::ios::metal_renderer::Path;
    /// # use gpui::platform::ios::metal_renderer::ScaledPixels;
    /// // let renderer: MetalRenderer = ...;
    /// // let mut instance_buffer = InstanceBuffer { metal_buffer: /* ... */, size: 4096, modified: 0 };
    /// // let mut offset = 0usize;
    /// // let viewport = Size::new(800.0f32, 600.0f32);
    /// // let encoder: &metal::RenderCommandEncoderRef = /* current encoder */;
    /// // assert!(renderer.draw_paths(&[], &mut instance_buffer, &mut offset, viewport, encoder));
    /// ```
    fn draw_paths(
        &self,
        paths: &[Path<ScaledPixels>],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if paths.is_empty() {
            return true;
        }

        command_encoder.set_render_pipeline_state(&self.path_pipeline_state);

        unsafe {
            let base_addr = instance_buffer.metal_buffer.contents();
            let mut p = (base_addr as *mut u8).add(*instance_offset);
            let mut draw_indirect_commands = Vec::with_capacity(paths.len());

            // copy vertices
            let vertices_offset = (p as usize) - (base_addr as usize);
            let mut first_vertex = 0;
            for (i, path) in paths.iter().enumerate() {
                if (p as usize) - (base_addr as usize)
                    + (mem::size_of::<PathVertex<ScaledPixels>>() * path.vertices.len())
                    > instance_buffer.size
                {
                    return false;
                }

                for v in &path.vertices {
                    *(p as *mut PathVertex<ScaledPixels>) = PathVertex {
                        xy_position: v.xy_position,
                        st_position: v.st_position,
                        content_mask: ContentMask {
                            bounds: path.content_mask.bounds,
                        },
                    };
                    p = p.add(mem::size_of::<PathVertex<ScaledPixels>>());
                }

                draw_indirect_commands.push(MTLDrawPrimitivesIndirectArguments {
                    vertexCount: path.vertices.len() as u32,
                    instanceCount: 1,
                    vertexStart: first_vertex,
                    baseInstance: i as u32,
                });
                first_vertex += path.vertices.len() as u32;
            }

            // copy sprites
            let sprites_offset = (p as u64) - (base_addr as u64);
            if (p as usize) - (base_addr as usize) + (mem::size_of::<PathSprite>() * paths.len())
                > instance_buffer.size
            {
                return false;
            }
            for path in paths {
                *(p as *mut PathSprite) = PathSprite {
                    bounds: path.bounds,
                    color: path.color,
                };
                p = p.add(mem::size_of::<PathSprite>());
            }

            // copy indirect commands
            let icb_bytes_len = mem::size_of_val(draw_indirect_commands.as_slice());
            let icb_offset = (p as u64) - (base_addr as u64);
            if (p as usize) - (base_addr as usize) + icb_bytes_len > instance_buffer.size {
                return false;
            }
            ptr::copy_nonoverlapping(
                draw_indirect_commands.as_ptr() as *const u8,
                p,
                icb_bytes_len,
            );
            p = p.add(icb_bytes_len);

            // draw path
            command_encoder.set_vertex_buffer(
                PathInputIndex::Vertices as u64,
                Some(&instance_buffer.metal_buffer),
                vertices_offset as u64,
            );

            command_encoder.set_vertex_bytes(
                PathInputIndex::ViewportSize as u64,
                mem::size_of_val(&viewport_size) as u64,
                &viewport_size as *const Size<DevicePixels> as *const _,
            );

            command_encoder.set_vertex_buffer(
                PathInputIndex::Sprites as u64,
                Some(&instance_buffer.metal_buffer),
                sprites_offset,
            );

            command_encoder.set_fragment_buffer(
                PathInputIndex::Sprites as u64,
                Some(&instance_buffer.metal_buffer),
                sprites_offset,
            );

            for i in 0..paths.len() {
                command_encoder.draw_primitives_indirect(
                    metal::MTLPrimitiveType::Triangle,
                    &instance_buffer.metal_buffer,
                    icb_offset
                        + (i * std::mem::size_of::<MTLDrawPrimitivesIndirectArguments>()) as u64,
                );
            }

            *instance_offset = (p as usize) - (base_addr as usize);
        }

        true
    }

    /// Renders a slice of underlines into the active render command encoder.
    ///
    /// This writes underline instance data into `instance_buffer` (aligned to 256 bytes),
    /// binds vertex and fragment buffers/state, and issues an instanced triangle draw for each underline.
    /// It does not modify other GPU state beyond the provided `command_encoder`.
    ///
    /// # Parameters
    /// - `underlines`: slice of underline instances to render.
    /// - `instance_buffer`: mutable GPU-side buffer used to store per-instance data for this frame.
    /// - `instance_offset`: byte offset within `instance_buffer` where new instance data will be written; advanced on success.
    /// - `viewport_size`: current viewport size in device pixels (passed to the vertex shader).
    /// - `command_encoder`: the Metal render command encoder to record draw commands into.
    ///
    /// # Returns
    /// `true` if all underlines were encoded and drawn successfully; `false` if the instance buffer did not have enough remaining space.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Pseudo-code example (platform-specific types omitted):
    /// let underlines: &[Underline] = &[];
    /// let mut instance_buffer = InstanceBuffer { metal_buffer: /* ... */ , size: 1024 };
    /// let mut offset = 0usize;
    /// let viewport = Size::new(800.0, 600.0);
    /// let encoder: &metal::RenderCommandEncoderRef = /* created from command buffer */;
    ///
    /// // Early-exits and returns true for empty slices.
    /// assert!(renderer.draw_underlines(underlines, &mut instance_buffer, &mut offset, viewport, encoder));
    /// ```
    fn draw_underlines(
        &self,
        underlines: &[Underline],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if underlines.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.underlines_pipeline_state);
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            UnderlineInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let underline_bytes_len = mem::size_of_val(underlines);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + underline_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                underlines.as_ptr() as *const u8,
                buffer_contents,
                underline_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            underlines.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    /// Renders an array of monochrome sprites using the sprite atlas and writes per-sprite instance data into the provided instance buffer.
    ///
    /// This encodes vertex/fragment state and issues an instanced draw for the sprites. If the instance buffer does not have enough space to hold the sprite data, the function returns `false` and does not advance `instance_offset`.
    ///
    /// # Parameters
    ///
    /// - `texture_id`: Identifier of the atlas texture containing sprite imagery.
    /// - `sprites`: Slice of `MonochromeSprite` instance data to upload and draw.
    /// - `instance_buffer`: Mutable reference to the GPU-backed instance buffer used for per-instance data.
    /// - `instance_offset`: Mutable byte offset into `instance_buffer`; advanced by the total bytes written on success.
    /// - `viewport_size`: Current viewport size expressed in device pixels.
    /// - `command_encoder`: Render command encoder used to bind resources and emit the draw call.
    ///
    /// # Returns
    ///
    /// `true` if all sprites were encoded and drawn and `instance_offset` was advanced, `false` if there was insufficient space in `instance_buffer`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Prepare inputs: texture_id, sprites, instance_buffer, instance_offset, viewport_size, command_encoder
    /// // Then call:
    /// // let ok = renderer.draw_monochrome_sprites(texture_id, &sprites, &mut instance_buffer, &mut instance_offset, viewport_size, &command_encoder);
    /// ```
    fn draw_monochrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[MonochromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.monochrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    /// Renders an array of polychrome sprites using the sprite atlas into the active render encoder.
    ///
    /// Writes sprite instance data into `instance_buffer` at `*instance_offset`, binds the atlas
    /// texture and related vertex/fragment resources, issues an instanced triangle draw for each
    /// sprite, and advances `*instance_offset` by the number of bytes written.
    ///
    /// The function returns `false` if the instance buffer does not have enough remaining space to
    /// hold the sprite data; in that case no draw is issued and the caller may grow or replace the
    /// buffer and retry.
    ///
    /// # Parameters
    ///
    /// - `texture_id`: Identifier for the atlas texture containing sprite artwork.
    /// - `sprites`: Slice of `PolychromeSprite` instances to render.
    /// - `instance_buffer`: Mutable reference to the GPU-backed instance buffer to receive sprite data.
    /// - `instance_offset`: Mutable reference to the byte offset within `instance_buffer` where data
    ///   will be written and advanced.
    /// - `viewport_size`: Current viewport size in device pixels (used by the vertex shader).
    /// - `command_encoder`: Active render command encoder to record binding and draw commands.
    ///
    /// # Returns
    ///
    /// `true` if the sprites were successfully written to the instance buffer and a draw was recorded, `false` if the buffer lacked sufficient space.
    fn draw_polychrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[PolychromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.polychrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    /// Draw video surfaces (no-op on iOS Phase 1).
    ///
    /// On iOS Phase 1, video texture rendering via CoreVideo is unavailable; this function
    /// does not render video surfaces but reports success so rendering can continue.
    ///
    /// # Returns
    ///
    /// `true` if the surfaces were handled (always `true` on iOS Phase 1).
    fn draw_surfaces(
        &mut self,
        _surfaces: &[PaintSurface],
        _instance_buffer: &mut InstanceBuffer,
        _instance_offset: &mut usize,
        _viewport_size: Size<DevicePixels>,
        _command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        // Video texture rendering requires core_video which we're not using on iOS Phase 1.
        // Return true to indicate success (surfaces are simply not rendered).
        true
    }
}

/// Creates a render pipeline state for the given vertex and fragment functions, pixel format, and MSAA sample count.
///
/// The returned pipeline state uses additive blending configured for typical alpha compositing (source RGB blended with `sourceAlpha`/`oneMinusSourceAlpha`, alpha channel blended with `one`).
///
/// # Returns
///
/// A configured `metal::RenderPipelineState`.
///
/// # Examples
///
/// ```no_run
/// // `device` and `library` are assumed to be valid Metal device and compiled library references.
/// let pipeline = build_pipeline_state(
///     &device,
///     &library,
///     "quad_pipeline",
///     "vertex_main",
///     "fragment_main",
///     metal::MTLPixelFormat::BGRA8Unorm,
///     1,
/// );
/// ```
fn build_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    sample_count: u64,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    descriptor.set_sample_count(sample_count);
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::One);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

// Align to multiples of 256 make Metal happy.
/// Round an offset up to the next 256-byte boundary in place.
///
/// # Examples
///
/// ```
/// let mut off = 1usize;
/// align_offset(&mut off);
/// assert_eq!(off, 256);
///
/// let mut off = 512usize;
/// align_offset(&mut off);
/// assert_eq!(off, 512);
/// ```
fn align_offset(offset: &mut usize) {
    *offset = (*offset).div_ceil(256) * 256;
}

/// Creates a multisample (MSAA) texture sized to the layer's drawable for the given sample count.
///
/// Returns `None` if the layer's drawable size has zero width or height, or if `sample_count` is 1 or less.
/// The created texture uses a 2D multisample texture type, the layer's pixel format, private storage mode,
/// render-target usage, and the provided sample count.
///
/// # Examples
///
/// ```no_run
/// # use metal;
/// # // `device` and `layer` would be obtained from a real Metal environment.
/// # let device: &metal::Device = unimplemented!();
/// # let layer: &metal::MetalLayer = unimplemented!();
/// let sample_count = 4;
/// let msaa = create_msaa_texture(device, layer, sample_count);
/// match msaa {
///     Some(tex) => println!("Created MSAA texture with sample count {}", tex.sample_count()),
///     None => println!("MSAA texture not created"),
/// }
/// ```
fn create_msaa_texture(
    device: &metal::Device,
    layer: &metal::MetalLayer,
    sample_count: u64,
) -> Option<metal::Texture> {
    let viewport_size = layer.drawable_size();
    let width = viewport_size.width.ceil() as u64;
    let height = viewport_size.height.ceil() as u64;

    if width == 0 || height == 0 {
        return None;
    }

    if sample_count <= 1 {
        return None;
    }

    let texture_descriptor = metal::TextureDescriptor::new();
    texture_descriptor.set_texture_type(metal::MTLTextureType::D2Multisample);

    // MTLStorageMode default is `shared` only for Apple silicon GPUs. Use `private` for Apple and Intel GPUs both.
    // Reference: https://developer.apple.com/documentation/metal/choosing-a-resource-storage-mode-for-apple-gpus
    texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);

    texture_descriptor.set_width(width);
    texture_descriptor.set_height(height);
    texture_descriptor.set_pixel_format(layer.pixel_format());
    texture_descriptor.set_usage(metal::MTLTextureUsage::RenderTarget);
    texture_descriptor.set_sample_count(sample_count);

    let metal_texture = device.new_texture(&texture_descriptor);
    Some(metal_texture)
}

#[repr(C)]
enum ShadowInputIndex {
    Vertices = 0,
    Shadows = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum QuadInputIndex {
    Vertices = 0,
    Quads = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum UnderlineInputIndex {
    Vertices = 0,
    Underlines = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum SpriteInputIndex {
    Vertices = 0,
    Sprites = 1,
    ViewportSize = 2,
    AtlasTextureSize = 3,
    AtlasTexture = 4,
}

#[repr(C)]
#[allow(dead_code)]
enum SurfaceInputIndex {
    Vertices = 0,
    Surfaces = 1,
    ViewportSize = 2,
    TextureSize = 3,
    YTexture = 4,
    CbCrTexture = 5,
}

#[repr(C)]
enum PathInputIndex {
    Vertices = 0,
    ViewportSize = 1,
    Sprites = 2,
}

#[repr(C)]
enum PathRasterizationInputIndex {
    Vertices = 0,
    ViewportSize = 1,
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct PathRasterizationVertex {
    pub xy_position: crate::Point<ScaledPixels>,
    pub st_position: crate::Point<f32>,
    pub color: Background,
    pub bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PathSprite {
    pub bounds: Bounds<ScaledPixels>,
    pub color: Background,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SurfaceBounds {
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
}