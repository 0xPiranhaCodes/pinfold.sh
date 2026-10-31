//! A safe layer over the Metal calls the renderer needs (ADR-0022).

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
vertex Out cell(uint corner_id [[vertex_id]], uint id [[instance_id]],
                const device Instance *instances [[buffer(0)]],
                constant float2 &viewport [[buffer(1)]],
                constant float &atlas_size [[buffer(2)]]) {
    Instance instance = instances[id];
    float2 corner = float2(corner_id & 1, corner_id >> 1);
    float2 at = float2(instance.position) + corner * float2(instance.size);
    Out out;
    out.position = float4(at.x / viewport.x * 2 - 1, 1 - at.y / viewport.y * 2, 0, 1);
    out.atlas = (float2(instance.atlas) + corner * float2(instance.size)) / atlas_size;
    out.colour = float4(((instance.colour >> 16) & 255) / 255.0, ((instance.colour >> 8) & 255) / 255.0,
                        (instance.colour & 255) / 255.0, 1);
    out.textured = instance.textured;
    return out;
}
fragment float4 shade(Out in [[stage_in]], texture2d<float> atlas [[texture(0)]]) {
    constexpr sampler nearest(filter::nearest);
    float alpha = in.textured ? atlas.sample(nearest, in.atlas).r : 1;
    return float4(in.colour.rgb, alpha);
}
";

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
    /// Colour as `0x00RRGGBB`.
    pub colour: u32,
    /// 1 to use the atlas as coverage, 0 for a solid rectangle.
    pub textured: u32,
}

/// A window's Metal layer, with one 8-bit atlas texture.
pub struct Gpu {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    atlas_size: usize,
    viewport: [f32; 2],
}

impl Gpu {
    /// Attaches a Metal layer to `window`. Returns nothing if this Mac has no Metal device
    /// or the window is not an AppKit window, so the caller can fall back.
    pub fn new(window: &impl HasWindowHandle, atlas_size: usize) -> Option<Self> {
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
        attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        attachment.setBlendingEnabled(true);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
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
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setMaximumDrawableCount(2);
        layer.setOpaque(true);
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        let view: NonNull<AnyObject> = appkit.ns_view.cast();
        // SAFETY: the handle guarantees a live NSView, which takes `layer` and retains it.
        unsafe {
            let () = msg_send![view.as_ptr(), setLayer: &*layer];
            let () = msg_send![view.as_ptr(), setWantsLayer: true];
        }
        Some(Self {
            layer,
            device,
            queue,
            pipeline,
            atlas,
            atlas_size,
            viewport: [1.0, 1.0],
        })
    }

    /// Sets the drawable size in physical pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
        self.layer.setDrawableSize(NSSize::new(width, height));
        self.viewport = [width as f32, height as f32];
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
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: attachment 0 always exists on a render pass.
        let colour = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        colour.setTexture(Some(&drawable.texture()));
        colour.setLoadAction(MTLLoadAction::Clear);
        colour.setStoreAction(MTLStoreAction::Store);
        let channel = |shift: u32| f64::from((background >> shift) & 0xff) / 255.0;
        colour.setClearColor(MTLClearColor {
            red: channel(16),
            green: channel(8),
            blue: channel(0),
            alpha: 1.0,
        });
        let Some(encoder) = commands.renderCommandEncoderWithDescriptor(&pass) else {
            return;
        };
        encoder.setRenderPipelineState(&self.pipeline);
        if let Some(buffer) = self.instance_buffer(instances) {
            let atlas_size = self.atlas_size as f32;
            let viewport = NonNull::from(&self.viewport).cast::<c_void>();
            let scale = NonNull::from(&atlas_size).cast::<c_void>();
            // SAFETY: the buffers match the shader's bindings, the bytes are copied by the
            // call, and each index stays within the shader's declared buffers and texture.
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(&buffer), 0, 0);
                encoder.setVertexBytes_length_atIndex(viewport, size_of_val(&self.viewport), 1);
                encoder.setVertexBytes_length_atIndex(scale, size_of::<f32>(), 2);
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
        commands.presentDrawable(ProtocolObject::<dyn MTLDrawable>::from_ref(&*drawable));
        commands.commit();
    }

    // shortcut: a new buffer each frame is simple but churns, reuse a pair if frames get slow.
    fn instance_buffer(
        &self,
        instances: &[Instance],
    ) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        let bytes = NonNull::new(instances.as_ptr().cast_mut().cast::<c_void>())?;
        if instances.is_empty() {
            return None;
        }
        let options = MTLResourceOptions::StorageModeShared;
        // SAFETY: `bytes` points to `size_of_val(instances)` readable bytes, which are copied.
        unsafe {
            self.device
                .newBufferWithBytes_length_options(bytes, size_of_val(instances), options)
        }
    }
}
