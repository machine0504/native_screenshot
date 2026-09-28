//! macOS GPU present path (raw Metal via objc2).
//!
//! The overlay is rasterized on the CPU into `present_buf` (a `0xAARRGGBB` u32
//! buffer whose little-endian bytes are BGRA8). softbuffer's CoreGraphics
//! backend re-uploads that *entire* framebuffer every present (it ignores damage
//! rects), which stutters on large / high-refresh HiDPI desktops.
//!
//! This presenter keeps a persistent `MTLTexture` and uploads **only the damaged
//! region** each frame via `replaceRegion`, then blits the whole texture to the
//! layer's drawable (a cheap on-GPU copy). Per-frame CPU→GPU traffic becomes
//! proportional to what actually changed, which is the key to smooth dragging /
//! magnifier movement on ProMotion / multi-monitor Retina setups.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::msg_send;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize,
    MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub struct GpuPresenter {
    // Held to keep the objects alive for the layer's lifetime.
    _device: Retained<ProtocolObject<dyn MTLDevice>>,
    _window: Rc<Window>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    tw: u32,
    th: u32,
}

impl GpuPresenter {
    /// Try to attach a Metal layer to `window` with a `tw`×`th` (physical px)
    /// scene texture. Returns `None` on any failure so the caller can fall back
    /// to softbuffer.
    pub fn new(window: Rc<Window>, tw: u32, th: u32) -> Option<Self> {
        if tw == 0 || th == 0 {
            return None;
        }
        autoreleasepool(|_| {
            let handle = window.window_handle().ok()?;
            let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                return None;
            };
            let view = appkit.ns_view.as_ptr() as *mut AnyObject;
            if view.is_null() {
                return None;
            }

            let device = MTLCreateSystemDefaultDevice()?;
            let queue = device.newCommandQueue()?;

            let layer = CAMetalLayer::new();
            let scale = {
                let s = window.scale_factor();
                if s > 0.0 {
                    s
                } else {
                    1.0
                }
            };
            unsafe {
                layer.setDevice(Some(&device));
                layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
                // We blit into the drawable, so it can't be framebuffer-only.
                layer.setFramebufferOnly(false);
                layer.setDrawableSize(CGSize {
                    width: tw as f64,
                    height: th as f64,
                });

                // CRITICAL for IME: do NOT make winit's view *layer-hosting* via
                // `setLayer:`. That turns it into a non-standard view and macOS
                // then suppresses the input-method candidate panel (CJK 候选条) —
                // composition/commit still work, but the candidate list never
                // shows. Instead keep the view *standard* (layer-backed) and add
                // the Metal layer as a **sublayer**, so the IME treats the view
                // normally AND we still present through Metal.
                let _: () = msg_send![view, setWantsLayer: true];
                let backing: *mut AnyObject = msg_send![view, layer];
                let frame = CGRect {
                    origin: CGPoint { x: 0.0, y: 0.0 },
                    size: CGSize {
                        width: tw as f64 / scale,
                        height: th as f64 / scale,
                    },
                };
                let _: () = msg_send![&*layer, setFrame: frame];
                let _: () = msg_send![&*layer, setContentsScale: scale];
                if backing.is_null() {
                    // No backing layer (shouldn't happen) — fall back to hosting.
                    let _: () = msg_send![view, setLayer: &*layer];
                } else {
                    let _: () = msg_send![backing, addSublayer: &*layer];
                }
            }

            // Persistent CPU-writable scene texture (BGRA8, matches present_buf).
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    tw as usize,
                    th as usize,
                    false,
                )
            };
            desc.setStorageMode(MTLStorageMode::Shared);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            let texture = device.newTextureWithDescriptor(&desc)?;

            Some(Self {
                _device: device,
                _window: window,
                queue,
                layer,
                texture,
                tw,
                th,
            })
        })
    }

    /// Upload the damaged region of `buf` (a `tw*th` BGRA8 buffer, one u32 per
    /// pixel) to the scene texture, then blit it to the drawable and present.
    /// `damage = None` uploads the whole frame.
    pub fn present(&mut self, buf: &[u32], damage: Option<(i32, i32, i32, i32)>) {
        let total = (self.tw as usize) * (self.th as usize);
        if buf.len() < total {
            return;
        }

        let (x0, y0, dw, dh) = match damage {
            Some((x, y, w, h)) => {
                let x0 = x.max(0);
                let y0 = y.max(0);
                let x1 = (x + w).min(self.tw as i32);
                let y1 = (y + h).min(self.th as i32);
                if x1 <= x0 || y1 <= y0 {
                    (0, 0, 0, 0)
                } else {
                    (x0, y0, x1 - x0, y1 - y0)
                }
            }
            None => (0, 0, self.tw as i32, self.th as i32),
        };

        if dw > 0 && dh > 0 {
            let offset = (y0 as usize * self.tw as usize + x0 as usize) * 4;
            let base = buf.as_ptr() as *const u8;
            let ptr = unsafe { base.add(offset) } as *mut c_void;
            let region = MTLRegion {
                origin: MTLOrigin {
                    x: x0 as usize,
                    y: y0 as usize,
                    z: 0,
                },
                size: MTLSize {
                    width: dw as usize,
                    height: dh as usize,
                    depth: 1,
                },
            };
            if let Some(nn) = NonNull::new(ptr) {
                unsafe {
                    self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                        region,
                        0,
                        nn,
                        (self.tw as usize) * 4,
                    );
                }
            }
        }

        autoreleasepool(|_| {
            let Some(drawable) = self.layer.nextDrawable() else {
                return;
            };
            let Some(cb) = self.queue.commandBuffer() else {
                return;
            };
            let Some(enc) = cb.blitCommandEncoder() else {
                return;
            };
            let dst = drawable.texture();
            unsafe {
                enc.copyFromTexture_toTexture(&self.texture, &dst);
            }
            enc.endEncoding();
            // presentDrawable: takes an MTLDrawable; send it directly (CAMetalDrawable
            // inherits MTLDrawable) to avoid an explicit protocol upcast.
            let _: () = unsafe { msg_send![&*cb, presentDrawable: &*drawable] };
            cb.commit();
        });
    }
}
