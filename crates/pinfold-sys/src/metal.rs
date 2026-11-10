//! A safe layer over the Metal calls the renderer needs (ADR-0022).

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, size_of_val};
use std::ptr::NonNull;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_foundation::{NSSize, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction, MTLOrigin,
    MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLSize,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// The drawable's format. The GPU blends in linear light and stores sRGB, so the shader and
/// the clear colour hand it linear values, premultiplied there.
const DRAWABLE: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm_sRGB;

const SHADER: &str = r"
using namespace metal;
struct Instance {
    packed_float2 position;
    packed_float2 size;
    packed_float2 atlas;
    uint colour;
    uint textured;
};
struct Out {
    float4 position [[position]];
    float2 atlas;
    float4 colour;
    uint textured [[flat]];
};
float linear(uint byte) {
    float value = (byte & 255) / 255.0;
    return value <= 0.04045 ? value / 12.92 : pow((value + 0.055) / 1.055, 2.4);
}
vertex Out cell(uint corner_id [[vertex_id]], uint id [[instance_id]],
                const device Instance *instances [[buffer(0)]],
                constant float2 &viewport [[buffer(1)]],
                constant float &atlas_size [[buffer(2)]],
                constant float &opacity [[buffer(3)]]) {
    Instance instance = instances[id];
    float2 corner = float2(corner_id & 1, corner_id >> 1);
    float2 at = float2(instance.position) + corner * float2(instance.size);
    Out out;
    out.position = float4(at.x / viewport.x * 2 - 1, 1 - at.y / viewport.y * 2, 0, 1);
    out.atlas = (float2(instance.atlas) + corner * float2(instance.size)) / atlas_size;
    out.colour = float4(linear(instance.colour >> 16), linear(instance.colour >> 8),
                        linear(instance.colour), (instance.colour >> 27 & 1) ? opacity : 1);
    out.textured = instance.textured;
    return out;
}
struct Shaded {
    float4 colour [[color(0), index(0)]];
    float4 covered [[color(0), index(1)]];
};
fragment Shaded shade(Out in [[stage_in]], texture2d<float> atlas [[texture(0)]]) {
    constexpr sampler nearest(filter::nearest);
    float alpha = in.textured ? atlas.sample(nearest, in.atlas).r : 1;
    Shaded out;
    out.colour = float4(in.colour.rgb, 1) * alpha * in.colour.a;
    // How much of what is under it a quad hides, which for glass is all of it.
    out.covered = float4(alpha);
    return out;
}
";

/// In [`Instance::colour`], draws a quad as the clear colour is drawn: at the opacity of
/// [`Gpu::set_opacity`], in place of what is under it and not over it.
pub const GLASS: u32 = 1 << 27;

/// One quad: a solid rectangle, or a glyph from the atlas. All values are in pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    /// Top-left corner on screen.
    pub position: [f32; 2],
    /// Width and height.
    pub size: [f32; 2],
    /// Top-left corner in the atlas.
    pub atlas: [f32; 2],
    /// Colour as `0x00RRGGBB`, with [`GLASS`] or not. The rest of the top byte is ignored.
    pub colour: u32,
    /// 1 to use the atlas as coverage, 0 for a solid rectangle.
    pub textured: u32,
}

/// A window's Metal layer, with one 8-bit atlas texture.
pub struct Gpu {
    layer: Retained<CAMetalLayer>,
    instances: RefCell<Option<Retained<ProtocolObject<dyn MTLBuffer>>>>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    atlas_size: usize,
    viewport: [f32; 2],
    /// How much of what is behind the window the clear colour hides, from 0 to 1.
    opacity: f32,
}

impl Gpu {
    /// Attaches a Metal layer to `window`. Returns nothing if this Mac has no Metal device
    /// or the window is not an AppKit window, so the caller can fall back.
    pub fn new(window: &impl HasWindowHandle, atlas_size: usize) -> Option<Self> {
        let gpu = Self::offscreen(atlas_size)?;
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        let view: NonNull<AnyObject> = appkit.ns_view.cast();
        // SAFETY: the handle guarantees a live NSView, which takes `layer` and retains it.
        unsafe {
            let () = msg_send![view.as_ptr(), setLayer: &*gpu.layer];
            let () = msg_send![view.as_ptr(), setWantsLayer: true];
        }
        Some(gpu)
    }

    /// Sets up everything but the window, so memory and pixels can be measured without one.
    /// Returns nothing if this Mac has no Metal device.
    pub fn offscreen(atlas_size: usize) -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .ok()?;
        let vertex = library.newFunctionWithName(&NSString::from_str("cell"))?;
        let fragment = library.newFunctionWithName(&NSString::from_str("shade"))?;
        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&vertex));
        descriptor.setFragmentFunction(Some(&fragment));
        // SAFETY: attachment 0 always exists on a pipeline descriptor.
        let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setPixelFormat(DRAWABLE);
        attachment.setBlendingEnabled(true);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
        // The shader's second output, so a glass quad can hide the clear and stay see-through.
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSource1Alpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSource1Alpha);
        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .ok()?;

        // SAFETY: the sizes describe a plain 2D texture with no mipmaps.
        let texture = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::R8Unorm,
                atlas_size,
                atlas_size,
                false,
            )
        };
        texture.setUsage(MTLTextureUsage::ShaderRead);
        let atlas = device.newTextureWithDescriptor(&texture)?;

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(DRAWABLE);
        layer.setMaximumDrawableCount(2);
        layer.setOpaque(true);
        Some(Self {
            layer,
            instances: RefCell::new(None),
            device,
            queue,
            pipeline,
            atlas,
            atlas_size,
            viewport: [1.0, 1.0],
            opacity: 1.0,
        })
    }

    /// Sets the drawable size in physical pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
        self.layer.setDrawableSize(NSSize::new(width, height));
        self.viewport = [width as f32, height as f32];
    }

    /// Sets the opacity of the clear colour, from 0 to 1, and returns whether the layer is now
    /// translucent. Quads stay opaque.
    pub fn set_opacity(&mut self, opacity: f32) -> bool {
        self.opacity = opacity;
        let translucent = opacity < 1.0;
        self.layer.setOpaque(!translucent);
        translucent
    }

    /// Copies an 8-bit mask into the atlas. Masks that do not fit are ignored.
    pub fn upload(&self, x: usize, y: usize, width: usize, mask: &[u8]) {
        let height = mask.len().checked_div(width).unwrap_or(0);
        let fits = x.saturating_add(width) <= self.atlas_size
            && y.saturating_add(height) <= self.atlas_size;
        let (Some(bytes), true) = (
            NonNull::new(mask.as_ptr().cast_mut().cast::<c_void>()),
            fits && height > 0,
        ) else {
            return;
        };
        let region = MTLRegion {
            origin: MTLOrigin { x, y, z: 0 },
            size: MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        // SAFETY: the region is inside the texture, and `mask` holds height rows of `width` bytes.
        unsafe {
            self.atlas
                .replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, bytes, width)
        };
    }

    /// Clears to `background` and draws `instances`. Skips the frame if no drawable is free.
    pub fn draw(&self, background: u32, instances: &[Instance]) {
        let Some(drawable) = self.layer.nextDrawable() else {
            return;
        };
        let Some(commands) = self.queue.commandBuffer() else {
            return;
        };
        let target = drawable.texture();
        if self
            .encode(&commands, &target, background, instances)
            .is_some()
        {
            commands.presentDrawable(ProtocolObject::<dyn MTLDrawable>::from_ref(&*drawable));
            commands.commit();
            // The next frame writes the same instance buffer. This waits for the GPU, not vsync.
            commands.waitUntilCompleted();
        }
    }

    fn encode(
        &self,
        commands: &ProtocolObject<dyn MTLCommandBuffer>,
        target: &ProtocolObject<dyn MTLTexture>,
        background: u32,
        instances: &[Instance],
    ) -> Option<()> {
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: attachment 0 always exists on a render pass.
        let colour = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        colour.setTexture(Some(target));
        colour.setLoadAction(MTLLoadAction::Clear);
        colour.setStoreAction(MTLStoreAction::Store);
        let opacity = f64::from(self.opacity);
        let channel = |shift: u32| {
            let value = f64::from((background >> shift) & 0xff) / 255.0;
            let light = if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            };
            light * opacity
        };
        colour.setClearColor(MTLClearColor {
            red: channel(16),
            green: channel(8),
            blue: channel(0),
            alpha: opacity,
        });
        let encoder = commands.renderCommandEncoderWithDescriptor(&pass)?;
        encoder.setRenderPipelineState(&self.pipeline);
        if let Some(buffer) = self.instance_buffer(instances) {
            let atlas_size = self.atlas_size as f32;
            let viewport = NonNull::from(&self.viewport).cast::<c_void>();
            let scale = NonNull::from(&atlas_size).cast::<c_void>();
            let opacity = NonNull::from(&self.opacity).cast::<c_void>();
            // SAFETY: the buffers match the shader's bindings, the bytes are copied by the
            // call, and each index stays within the shader's declared buffers and texture.
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(&buffer), 0, 0);
                encoder.setVertexBytes_length_atIndex(viewport, size_of_val(&self.viewport), 1);
                encoder.setVertexBytes_length_atIndex(scale, size_of::<f32>(), 2);
                encoder.setVertexBytes_length_atIndex(opacity, size_of::<f32>(), 3);
                encoder.setFragmentTexture_atIndex(Some(&self.atlas), 0);
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                    instances.len(),
                );
            }
        }
        encoder.endEncoding();
        Some(())
    }

    /// Copies `instances` into the one buffer frames share, which only grows. A new buffer per
    /// frame leaves the driver holding more at idle, by an amount that varies from run to run.
    fn instance_buffer(
        &self,
        instances: &[Instance],
    ) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        let bytes = NonNull::new(instances.as_ptr().cast_mut())?;
        if instances.is_empty() {
            return None;
        }
        let length = size_of_val(instances);
        let mut shared = self.instances.borrow_mut();
        if shared
            .as_ref()
            .is_none_or(|buffer| buffer.length() < length)
        {
            *shared = self
                .device
                .newBufferWithLength_options(length * 2, MTLResourceOptions::StorageModeShared);
        }
        let buffer = shared.clone()?;
        // SAFETY: `bytes` holds `instances.len()` instances and the buffer at least as many, and
        // `draw` waits for the GPU, so nothing else reads the buffer during this write.
        unsafe {
            buffer
                .contents()
                .cast::<Instance>()
                .copy_from_nonoverlapping(bytes, instances.len());
        }
        Some(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: usize = 2;
    /// The atlas holds half coverage at x = 0 and full coverage at x = 1.
    const HALF: f32 = 0.0;
    const FULL: f32 = 1.0;
    const PAPER: u32 = 0x0010_80c0;
    const SOLID: u32 = 0x00a0_b0c0;
    const INK: u32 = 0x00ff_ffff;

    fn gpu() -> Option<Gpu> {
        let mut gpu = Gpu::offscreen(4)?;
        gpu.resize(SIDE as u32, SIDE as u32);
        gpu.upload(0, 0, 2, &[128, 255]);
        gpu.device.hasUnifiedMemory().then_some(gpu)
    }

    fn solid(x: f32, y: f32, colour: u32) -> Instance {
        Instance {
            position: [x, y],
            size: [1.0, 1.0],
            atlas: [0.0, 0.0],
            colour,
            textured: 0,
        }
    }

    fn glyph(x: f32, y: f32, coverage: f32) -> Instance {
        Instance {
            atlas: [coverage, 0.0],
            textured: 1,
            ..solid(x, y, 0x03ff_ffff)
        }
    }

    /// Draws a frame, and returns its pixels as `0xAARRGGBB` in reading order.
    fn pixels(gpu: &Gpu, background: u32, quads: &[Instance]) -> Option<[u32; SIDE * SIDE]> {
        // SAFETY: the sizes describe a plain 2D texture with no mipmaps.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                DRAWABLE, SIDE, SIDE, false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        let target = gpu.device.newTextureWithDescriptor(&descriptor)?;
        let commands = gpu.queue.commandBuffer()?;
        gpu.encode(&commands, &target, background, quads)?;
        commands.commit();
        commands.waitUntilCompleted();
        let mut pixels = [0_u32; SIDE * SIDE];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: SIDE,
                height: SIDE,
                depth: 1,
            },
        };
        // SAFETY: `pixels` holds every byte of the region, and the GPU has finished with it.
        unsafe {
            let bytes = NonNull::from(&mut pixels).cast::<c_void>();
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(bytes, SIDE * 4, region, 0);
        }
        Some(pixels)
    }

    fn light(byte: u32) -> f64 {
        let value = f64::from(byte & 0xff) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    fn srgb(light: f64) -> f64 {
        let value = if light <= 0.003_130_8 {
            light * 12.92
        } else {
            1.055 * light.powf(1.0 / 2.4) - 0.055
        };
        value * 255.0
    }

    /// `coverage` of white over `colour` at `opacity`, premultiplied in linear light.
    fn expected(colour: u32, opacity: f64, coverage: f64) -> [f64; 4] {
        let channel = |byte| srgb(coverage + (1.0 - coverage) * opacity * light(byte));
        [
            (coverage + (1.0 - coverage) * opacity) * 255.0,
            channel(colour >> 16),
            channel(colour >> 8),
            channel(colour),
        ]
    }

    #[track_caller]
    fn assert_near(pixel: u32, expected: [f64; 4]) {
        let bytes = pixel.to_be_bytes();
        for (byte, want) in bytes.iter().zip(expected) {
            assert!(
                (f64::from(*byte) - want).abs() <= 1.0,
                "{pixel:#010x} is not {expected:?}"
            );
        }
    }

    #[test]
    fn shaded_pixels_are_mixed_in_linear_light() {
        let Some(gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        let quads = [glyph(0.0, 0.0, HALF), solid(1.0, 0.0, SOLID)];
        let pixels = pixels(&gpu, 0x0010_2030, &quads).unwrap();
        // Solid colours pass through unchanged, and the first pixel is `blend` in `pinfold-render`.
        assert_eq!(pixels, [0xffbc_bdbe, 0xffa0_b0c0, 0xff10_2030, 0xff10_2030]);
    }

    #[test]
    fn frames_of_any_length_share_the_instance_buffer() {
        let Some(gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        const BACKGROUND: u32 = 0x0010_2030;
        let mut quads = vec![solid(1.0, 1.0, SOLID); 64];
        let first = pixels(&gpu, BACKGROUND, &quads[..1]).unwrap();
        assert_eq!(pixels(&gpu, BACKGROUND, &quads).unwrap(), first);
        let grown = gpu
            .instances
            .borrow()
            .as_ref()
            .map(|buffer| buffer.length());
        quads[0] = solid(0.0, 0.0, SOLID);
        let second = pixels(&gpu, BACKGROUND, &quads[..1]).unwrap();
        // A short frame after a long one draws its own quads, not what is left in the buffer.
        assert_eq!(
            second,
            [
                first[3],
                BACKGROUND | 0xff00_0000,
                BACKGROUND | 0xff00_0000,
                BACKGROUND | 0xff00_0000
            ]
        );
        assert_eq!(
            gpu.instances
                .borrow()
                .as_ref()
                .map(|buffer| buffer.length()),
            grown
        );
    }

    #[test]
    fn the_default_background_alone_is_translucent() {
        let Some(mut gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        assert!(gpu.set_opacity(0.85));
        let quads = [solid(1.0, 0.0, SOLID)];
        let [paper, solid, ..] = pixels(&gpu, PAPER, &quads).unwrap();
        // Scaling the sRGB bytes would give about 0xd9_0e_6d_a3, too dark in linear light.
        assert_near(paper, expected(PAPER, 0.85, 0.0));
        assert_eq!(paper >> 24, 217);
        assert_eq!(solid, 0xff00_0000 | SOLID);
    }

    /// ADR-0049, section 4: a pane's OSC 11 background is glass as the clear is, not a layer
    /// over it, and text on it mixes as it does on the clear.
    #[test]
    fn a_glass_quad_replaces_the_clear_at_its_opacity() {
        let Some(mut gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        let quads = [
            solid(0.0, 0.0, SOLID | GLASS),
            solid(1.0, 0.0, SOLID | GLASS),
            glyph(1.0, 0.0, HALF),
        ];
        let [glass, ..] = pixels(&gpu, PAPER, &quads).unwrap();
        assert_eq!(glass, 0xff00_0000 | SOLID, "opaque while the window is");
        gpu.set_opacity(0.85);
        let [glass, text, paper, _] = pixels(&gpu, PAPER, &quads).unwrap();
        assert_near(glass, expected(SOLID, 0.85, 0.0));
        assert_eq!(glass >> 24, 217);
        assert_near(text, expected(SOLID, 0.85, 128.0 / 255.0));
        assert_near(paper, expected(PAPER, 0.85, 0.0));
    }

    #[test]
    fn full_coverage_text_is_opaque_on_any_background() {
        let Some(mut gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        gpu.set_opacity(0.85);
        let quads = [
            glyph(0.0, 0.0, FULL),
            solid(1.0, 0.0, SOLID),
            glyph(1.0, 0.0, FULL),
        ];
        let [on_paper, on_cell, ..] = pixels(&gpu, PAPER, &quads).unwrap();
        assert_eq!(on_paper, 0xff00_0000 | INK);
        assert_eq!(on_cell, 0xff00_0000 | INK);
    }

    #[test]
    fn text_edges_mix_with_what_is_under_them() {
        let Some(mut gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        gpu.set_opacity(0.85);
        let quads = [
            glyph(0.0, 0.0, HALF),
            solid(1.0, 0.0, SOLID),
            glyph(1.0, 0.0, HALF),
        ];
        let [on_paper, on_cell, ..] = pixels(&gpu, PAPER, &quads).unwrap();
        let coverage = 128.0 / 255.0;
        // Over the translucent background the pixel stays partly see-through.
        assert_near(on_paper, expected(PAPER, 0.85, coverage));
        assert_near(on_cell, expected(SOLID, 1.0, coverage));
    }

    #[test]
    fn opacity_can_change_at_runtime_and_back() {
        let Some(mut gpu) = gpu() else {
            eprintln!("skipped: no Metal device with readable pixels");
            return;
        };
        assert!(gpu.layer.isOpaque());
        assert!(gpu.set_opacity(0.0));
        assert!(!gpu.layer.isOpaque());
        assert_eq!(pixels(&gpu, PAPER, &[]).unwrap()[0], 0);
        assert!(!gpu.set_opacity(1.0));
        assert!(gpu.layer.isOpaque());
        assert_eq!(pixels(&gpu, PAPER, &[]).unwrap()[0], 0xff00_0000 | PAPER);
    }
}
