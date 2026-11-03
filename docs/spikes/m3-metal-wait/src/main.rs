//! Measures the frame-time cost of Metal's `waitUntilCompleted` on a full text grid.

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("this spike needs an Apple Silicon Mac");

use std::ffi::c_void;
use std::mem::{size_of, size_of_val};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLSize,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

const COLUMNS: usize = 200;
const ROWS: usize = 60;
const CELL_WIDTH: usize = 10;
const CELL_HEIGHT: usize = 20;
const RING: usize = 3;

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
                constant float &atlas_size [[buffer(2)]]) {
    Instance instance = instances[id];
    float2 corner = float2(corner_id & 1, corner_id >> 1);
    float2 at = float2(instance.position) + corner * float2(instance.size);
    Out out;
    out.position = float4(at.x / viewport.x * 2 - 1, 1 - at.y / viewport.y * 2, 0, 1);
    out.atlas = (float2(instance.atlas) + corner * float2(instance.size)) / atlas_size;
    out.colour = float4(linear(instance.colour >> 16), linear(instance.colour >> 8),
                        linear(instance.colour), 1);
    out.textured = instance.textured;
    return out;
}
fragment float4 shade(Out in [[stage_in]], texture2d<float> atlas [[texture(0)]]) {
    constexpr sampler nearest(filter::nearest);
    float alpha = in.textured ? atlas.sample(nearest, in.atlas).r : 1;
    return float4(in.colour.rgb * alpha, alpha);
}
";

#[repr(C)]
#[derive(Clone, Copy)]
struct Instance {
    position: [f32; 2],
    size: [f32; 2],
    atlas: [f32; 2],
    colour: u32,
    textured: u32,
}

type Commands = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

struct Gpu {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    target: Retained<ProtocolObject<dyn MTLTexture>>,
    atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    buffers: Vec<Retained<ProtocolObject<dyn MTLBuffer>>>,
    viewport: [f32; 2],
}

impl Gpu {
    fn new(instance_bytes: usize) -> Self {
        let device = MTLCreateSystemDefaultDevice().expect("Metal device");
        assert!(
            device.hasUnifiedMemory(),
            "measurement requires unified memory"
        );
        let queue = device.newCommandQueue().expect("command queue");
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .expect("shader library");
        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(
            &library
                .newFunctionWithName(&NSString::from_str("cell"))
                .expect("vertex shader"),
        ));
        descriptor.setFragmentFunction(Some(
            &library
                .newFunctionWithName(&NSString::from_str("shade"))
                .expect("fragment shader"),
        ));
        let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
        attachment.setBlendingEnabled(true);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .expect("render pipeline");

        let texture = |format, width, height, usage| unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    format, width, height, false,
                );
            descriptor.setUsage(usage);
            device
                .newTextureWithDescriptor(&descriptor)
                .expect("texture")
        };
        let target = texture(
            MTLPixelFormat::BGRA8Unorm_sRGB,
            COLUMNS * CELL_WIDTH,
            ROWS * CELL_HEIGHT,
            MTLTextureUsage::RenderTarget,
        );
        let atlas = texture(MTLPixelFormat::R8Unorm, 16, 16, MTLTextureUsage::ShaderRead);
        let mask = [220_u8; 16 * 16];
        unsafe {
            atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width: 16,
                        height: 16,
                        depth: 1,
                    },
                },
                0,
                NonNull::from(&mask).cast::<c_void>(),
                16,
            );
        }
        let buffers = (0..RING)
            .map(|_| {
                device
                    .newBufferWithLength_options(
                        instance_bytes,
                        MTLResourceOptions::StorageModeShared,
                    )
                    .expect("instance buffer")
            })
            .collect();
        Self {
            queue,
            pipeline,
            target,
            atlas,
            buffers,
            viewport: [(COLUMNS * CELL_WIDTH) as f32, (ROWS * CELL_HEIGHT) as f32],
        }
    }

    fn submit(&self, slot: usize, instances: &[Instance]) -> Commands {
        let buffer = &self.buffers[slot];
        let source = NonNull::new(instances.as_ptr().cast_mut()).expect("non-empty frame");
        unsafe {
            buffer
                .contents()
                .cast::<Instance>()
                .copy_from_nonoverlapping(source, instances.len());
        }
        let commands = self.queue.commandBuffer().expect("command buffer");
        let pass = MTLRenderPassDescriptor::new();
        let colour = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        colour.setTexture(Some(&self.target));
        colour.setLoadAction(MTLLoadAction::Clear);
        colour.setStoreAction(MTLStoreAction::Store);
        colour.setClearColor(MTLClearColor {
            red: 0.005,
            green: 0.008,
            blue: 0.012,
            alpha: 1.0,
        });
        let encoder = commands
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("render encoder");
        encoder.setRenderPipelineState(&self.pipeline);
        let atlas_size = 16.0_f32;
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(buffer), 0, 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(&self.viewport).cast::<c_void>(),
                size_of_val(&self.viewport),
                1,
            );
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(&atlas_size).cast::<c_void>(),
                size_of::<f32>(),
                2,
            );
            encoder.setFragmentTexture_atIndex(Some(&self.atlas), 0);
            encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::TriangleStrip,
                0,
                4,
                instances.len(),
            );
        }
        encoder.endEncoding();
        commands.commit();
        commands
    }
}

#[derive(Default)]
struct Samples {
    frame: Vec<Duration>,
    blocked: Vec<Duration>,
    blocked_total: Duration,
    elapsed: Duration,
}

fn run(gpu: &Gpu, instances: &[Instance], frames: usize, immediate_wait: bool) -> Samples {
    let mut samples = Samples::default();
    let mut in_flight: Vec<Option<Commands>> = (0..RING).map(|_| None).collect();
    let whole = Instant::now();
    for frame in 0..frames {
        let slot = if immediate_wait { 0 } else { frame % RING };
        let block = Instant::now();
        if let Some(commands) = in_flight[slot].take() {
            commands.waitUntilCompleted();
        }
        let blocked = block.elapsed();
        let start = Instant::now();
        let commands = gpu.submit(slot, instances);
        if immediate_wait {
            let block = Instant::now();
            commands.waitUntilCompleted();
            let waited = block.elapsed();
            samples.blocked.push(waited);
            samples.blocked_total += waited;
        } else {
            samples.blocked.push(blocked);
            samples.blocked_total += blocked;
            in_flight[slot] = Some(commands);
        }
        samples.frame.push(start.elapsed());
    }
    for commands in in_flight.into_iter().flatten() {
        let block = Instant::now();
        commands.waitUntilCompleted();
        samples.blocked_total += block.elapsed();
    }
    samples.elapsed = whole.elapsed();
    samples
}

fn percentile(samples: &mut [Duration], percent: usize) -> f64 {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100].as_secs_f64() * 1000.0
}

fn print_row(name: &str, mut samples: Samples) {
    let median = percentile(&mut samples.frame, 50);
    let p99 = percentile(&mut samples.frame, 99);
    let blocked_median = percentile(&mut samples.blocked, 50);
    let blocked_p99 = percentile(&mut samples.blocked, 99);
    let completed = samples.elapsed.as_secs_f64() * 1000.0 / samples.frame.len() as f64;
    println!(
        "| {name} | {median:.3} | {p99:.3} | {blocked_median:.3} | {blocked_p99:.3} | {:.2} | {completed:.3} |",
        samples.blocked_total.as_secs_f64() * 1000.0,
    );
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let frames = arguments.get(1).map_or(1000, |value| {
        value.parse().expect("frame count must be an integer")
    });
    let warmup = arguments.get(2).map_or(120, |value| {
        value.parse().expect("warm-up count must be an integer")
    });
    assert!(frames > 0, "frame count must be positive");

    let instances: Vec<Instance> = (0..ROWS)
        .flat_map(|row| {
            (0..COLUMNS).map(move |column| Instance {
                position: [
                    (column * CELL_WIDTH + 1) as f32,
                    (row * CELL_HEIGHT + 3) as f32,
                ],
                size: [(CELL_WIDTH - 2) as f32, (CELL_HEIGHT - 6) as f32],
                atlas: [0.0, 0.0],
                colour: 0x00c8_d0d8 + ((row + column) % 4) as u32 * 0x0004_0302,
                textured: 1,
            })
        })
        .collect();
    let gpu = Gpu::new(size_of_val(instances.as_slice()));
    let _ = run(&gpu, &instances, warmup, true);
    let _ = run(&gpu, &instances, warmup, false);

    println!("target: 200x60 cells, 2000x1200 pixels, 12000 textured quads");
    println!(
        "| mode | frame/submit median ms | p99 ms | blocked median ms | blocked p99 ms | blocked total ms | completed ms/frame |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    print_row("wait after commit", run(&gpu, &instances, frames, true));
    print_row("3-buffer submit", run(&gpu, &instances, frames, false));
}
