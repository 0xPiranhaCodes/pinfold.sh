//! The thinnest useful Metal path: a CAMetalLayer with two drawables and instanced cell quads.
//! There is no CPU pixel buffer, because the GPU draws cells directly.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::msg_send;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice,
    MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLStoreAction,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

const CELLS: usize = 120 * 50;

const SHADER: &str = r#"
using namespace metal;
struct Out { float4 position [[position]]; };
vertex Out cell(uint vertex_id [[vertex_id]], uint cell_id [[instance_id]]) {
    float2 corner = float2(vertex_id & 1, (vertex_id >> 1) & 1);
    float2 at = float2(cell_id % 120, cell_id / 120) + corner * 0.8;
    Out out;
    out.position = float4(at.x / 60.0 - 1.0, 1.0 - at.y / 25.0, 0.0, 1.0);
    return out;
}
fragment float4 shade() { return float4(0.9, 0.9, 0.92, 1.0); }
"#;

pub struct Renderer {
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl Renderer {
    pub fn new(window: &Window) -> Self {
        let device = MTLCreateSystemDefaultDevice().expect("device");
        let queue = device.newCommandQueue().expect("queue");
        let source = NSString::from_str(SHADER);
        let library = device.newLibraryWithSource_options_error(&source, None).expect("library");
        let vertex = library.newFunctionWithName(&NSString::from_str("cell")).expect("vertex");
        let fragment = library.newFunctionWithName(&NSString::from_str("shade")).expect("fragment");
        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&vertex));
        descriptor.setFragmentFunction(Some(&fragment));
        let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        let pipeline = device.newRenderPipelineStateWithDescriptor_error(&descriptor).expect("pipeline");

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setMaximumDrawableCount(2);
        let size = window.inner_size();
        layer.setDrawableSize(objc2_foundation::NSSize::new(f64::from(size.width), f64::from(size.height)));
        let handle = window.window_handle().expect("handle");
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { panic!("not AppKit") };
        let view: NonNull<AnyObject> = appkit.ns_view.cast();
        unsafe {
            let _: () = msg_send![view.as_ptr(), setWantsLayer: true];
            let _: () = msg_send![view.as_ptr(), setLayer: &*layer];
        }
        Self { layer, queue, pipeline }
    }

    pub fn draw(&mut self, shade: u32) {
        let Some(drawable) = self.layer.nextDrawable() else { return };
        let pass = MTLRenderPassDescriptor::new();
        let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color.setTexture(Some(&drawable.texture()));
        color.setLoadAction(MTLLoadAction::Clear);
        color.setStoreAction(MTLStoreAction::Store);
        let level = f64::from(shade & 0xff) / 255.0;
        color.setClearColor(MTLClearColor { red: level, green: level, blue: level, alpha: 1.0 });
        let commands = self.queue.commandBuffer().expect("commands");
        let encoder = commands.renderCommandEncoderWithDescriptor(&pass).expect("encoder");
        encoder.setRenderPipelineState(&self.pipeline);
        unsafe {
            encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::TriangleStrip,
                0,
                4,
                CELLS,
            );
        }
        encoder.endEncoding();
        commands.presentDrawable(ProtocolObject::<dyn MTLDrawable>::from_ref(&*drawable));
        commands.commit();
    }
}
