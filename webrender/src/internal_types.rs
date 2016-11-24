/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use app_units::Au;
use device::TextureFilter;
use euclid::{Size2D, TypedPoint2D, UnknownUnit};
use fnv::FnvHasher;
use offscreen_gl_context::{NativeGLContext, NativeGLContextHandle};
use offscreen_gl_context::{GLContext, NativeGLContextMethods, GLContextDispatcher};
use offscreen_gl_context::{OSMesaContext, OSMesaContextHandle};
use offscreen_gl_context::{ColorAttachmentType, GLContextAttributes, GLLimits};
use profiler::BackendProfileCounters;
use std::collections::{HashMap, HashSet};
use std::f32;
use std::hash::BuildHasherDefault;
use std::{i32, usize};
use std::path::PathBuf;
use std::sync::Arc;
use tiling;
use webrender_traits::{Epoch, ColorF, PipelineId};
use webrender_traits::{ImageFormat, MixBlendMode, NativeFontHandle};
use webrender_traits::{ExternalImageId, ScrollLayerId, WebGLCommand};

// An ID for a texture that is owned by the
// texture cache module. This can include atlases
// or standalone textures allocated via the
// texture cache (e.g. if an image is too large
// to be added to an atlas). The texture cache
// manages the allocation and freeing of these
// IDs, and the rendering thread maintains a
// map from cache texture ID to native texture.

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheTextureId(pub usize);

// Represents the source for a texture.
// These are passed from throughout the
// pipeline until they reach the rendering
// thread, where they are resolved to a
// native texture ID.

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SourceTexture {
    Invalid,
    TextureCache(CacheTextureId),
    WebGL(u32),                         // Is actually a gl::GLuint
    External(ExternalImageId),
}

pub enum GLContextHandleWrapper {
    Native(NativeGLContextHandle),
    OSMesa(OSMesaContextHandle),
}

impl GLContextHandleWrapper {
    pub fn current_native_handle() -> Option<GLContextHandleWrapper> {
        NativeGLContext::current_handle().map(GLContextHandleWrapper::Native)
    }

    pub fn current_osmesa_handle() -> Option<GLContextHandleWrapper> {
        OSMesaContext::current_handle().map(GLContextHandleWrapper::OSMesa)
    }

    pub fn new_context(&self,
                       size: Size2D<i32>,
                       attributes: GLContextAttributes,
                       dispatcher: Option<Box<GLContextDispatcher>>) -> Result<GLContextWrapper, &'static str> {
        match *self {
            GLContextHandleWrapper::Native(ref handle) => {
                let ctx = GLContext::<NativeGLContext>::new_shared_with_dispatcher(size,
                                                                                   attributes,
                                                                                   ColorAttachmentType::Texture,
                                                                                   Some(handle),
                                                                                   dispatcher);
                ctx.map(GLContextWrapper::Native)
            }
            GLContextHandleWrapper::OSMesa(ref handle) => {
                let ctx = GLContext::<OSMesaContext>::new_shared_with_dispatcher(size,
                                                                                 attributes,
                                                                                 ColorAttachmentType::Texture,
                                                                                 Some(handle),
                                                                                 dispatcher);
                ctx.map(GLContextWrapper::OSMesa)
            }
        }
    }
}

pub enum GLContextWrapper {
    Native(GLContext<NativeGLContext>),
    OSMesa(GLContext<OSMesaContext>),
}

impl GLContextWrapper {
    pub fn make_current(&self) {
        match *self {
            GLContextWrapper::Native(ref ctx) => {
                ctx.make_current().unwrap();
            }
            GLContextWrapper::OSMesa(ref ctx) => {
                ctx.make_current().unwrap();
            }
        }
    }

    pub fn unbind(&self) {
        match *self {
            GLContextWrapper::Native(ref ctx) => {
                ctx.unbind().unwrap();
            }
            GLContextWrapper::OSMesa(ref ctx) => {
                ctx.unbind().unwrap();
            }
        }
    }

    pub fn apply_command(&self, cmd: WebGLCommand) {
        match *self {
            GLContextWrapper::Native(ref ctx) => {
                cmd.apply(ctx);
            }
            GLContextWrapper::OSMesa(ref ctx) => {
                cmd.apply(ctx);
            }
        }
    }

    pub fn get_info(&self) -> (Size2D<i32>, u32, GLLimits) {
        match *self {
            GLContextWrapper::Native(ref ctx) => {
                let (real_size, texture_id) = {
                    let draw_buffer = ctx.borrow_draw_buffer().unwrap();
                    (draw_buffer.size(), draw_buffer.get_bound_texture_id().unwrap())
                };

                let limits = ctx.borrow_limits().clone();

                (real_size, texture_id, limits)
            }
            GLContextWrapper::OSMesa(ref ctx) => {
                let (real_size, texture_id) = {
                    let draw_buffer = ctx.borrow_draw_buffer().unwrap();
                    (draw_buffer.size(), draw_buffer.get_bound_texture_id().unwrap())
                };

                let limits = ctx.borrow_limits().clone();

                (real_size, texture_id, limits)
            }
        }
    }

    pub fn resize(&mut self, size: &Size2D<i32>) -> Result<(), &'static str> {
        match *self {
            GLContextWrapper::Native(ref mut ctx) => {
                ctx.resize(*size)
            }
            GLContextWrapper::OSMesa(ref mut ctx) => {
                ctx.resize(*size)
            }
        }
    }
}

const COLOR_FLOAT_TO_FIXED: f32 = 255.0;
pub const ANGLE_FLOAT_TO_FIXED: f32 = 65535.0;

pub const ORTHO_NEAR_PLANE: f32 = -1000000.0;
pub const ORTHO_FAR_PLANE: f32 = 1000000.0;


pub enum FontTemplate {
    Raw(Arc<Vec<u8>>),
    Native(NativeFontHandle),
}

#[derive(Debug, PartialEq, Eq)]
pub enum TextureSampler {
    Color0,
    Color1,
    Color2,
    Mask,
    Cache,
    Data16,
    Data32,
    Data64,
    Data128,
    Layers,
    RenderTasks,
    Geometry,
    ResourceRects,
}

impl TextureSampler {
    pub fn color(n: usize) -> TextureSampler {
        match n {
            0 => TextureSampler::Color0,
            1 => TextureSampler::Color1,
            2 => TextureSampler::Color2,
            _ => {
                panic!("There are only 3 color samplers.");
            }
        }
    }
}

/// Optional textures that can be used as a source in the shaders.
/// Textures that are not used by the batch are equal to TextureId::invalid().
#[derive(Copy, Clone, Debug)]
pub struct BatchTextures {
    pub colors: [SourceTexture; 3],
}

impl BatchTextures {
    pub fn no_texture() -> Self {
        BatchTextures {
            colors: [SourceTexture::Invalid; 3],
        }
    }
}

// In some places we need to temporarily bind a texture to any slot.
pub const DEFAULT_TEXTURE: TextureSampler = TextureSampler::Color0;

pub enum VertexAttribute {
    Position,
    PositionRect,
    ColorRectTL,
    ColorRectTR,
    ColorRectBR,
    ColorRectBL,
    ColorTexCoordRectTop,
    MaskTexCoordRectTop,
    ColorTexCoordRectBottom,
    MaskTexCoordRectBottom,
    BorderRadii,
    BorderPosition,
    BlurRadius,
    DestTextureSize,
    SourceTextureSize,
    Misc,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PackedColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl PackedColor {
    pub fn from_color(color: &ColorF) -> PackedColor {
        PackedColor {
            r: (0.5 + color.r * COLOR_FLOAT_TO_FIXED).floor() as u8,
            g: (0.5 + color.g * COLOR_FLOAT_TO_FIXED).floor() as u8,
            b: (0.5 + color.b * COLOR_FLOAT_TO_FIXED).floor() as u8,
            a: (0.5 + color.a * COLOR_FLOAT_TO_FIXED).floor() as u8,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PackedVertexForQuad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color_tl: PackedColor,
    pub color_tr: PackedColor,
    pub color_br: PackedColor,
    pub color_bl: PackedColor,
    pub u_tl: f32,
    pub v_tl: f32,
    pub u_tr: f32,
    pub v_tr: f32,
    pub u_br: f32,
    pub v_br: f32,
    pub u_bl: f32,
    pub v_bl: f32,
    pub mu_tl: u16,
    pub mv_tl: u16,
    pub mu_tr: u16,
    pub mv_tr: u16,
    pub mu_br: u16,
    pub mv_br: u16,
    pub mu_bl: u16,
    pub mv_bl: u16,
    pub matrix_index: u8,
    pub clip_in_rect_index: u8,
    pub clip_out_rect_index: u8,
    pub tile_params_index: u8,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PackedVertex {
    pub pos: [f32; 2],
}

#[derive(Debug)]
pub struct DebugFontVertex {
    pub x: f32,
    pub y: f32,
    pub color: PackedColor,
    pub u: f32,
    pub v: f32,
}

impl DebugFontVertex {
    pub fn new(x: f32, y: f32, u: f32, v: f32, color: PackedColor) -> DebugFontVertex {
        DebugFontVertex {
            x: x,
            y: y,
            color: color,
            u: u,
            v: v,
        }
    }
}

pub struct DebugColorVertex {
    pub x: f32,
    pub y: f32,
    pub color: PackedColor,
}

impl DebugColorVertex {
    pub fn new(x: f32, y: f32, color: PackedColor) -> DebugColorVertex {
        DebugColorVertex {
            x: x,
            y: y,
            color: color,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RenderTargetMode {
    None,
    SimpleRenderTarget,
    LayerRenderTarget(i32),      // Number of texture layers
}

pub enum TextureUpdateOp {
    Create(u32, u32, ImageFormat, TextureFilter, RenderTargetMode, Option<Arc<Vec<u8>>>),
    Update(u32, u32, u32, u32, Arc<Vec<u8>>, Option<u32>),
    Grow(u32, u32, ImageFormat, TextureFilter, RenderTargetMode),
}

pub struct TextureUpdate {
    pub id: CacheTextureId,
    pub op: TextureUpdateOp,
}

pub struct TextureUpdateList {
    pub updates: Vec<TextureUpdate>,
}

impl TextureUpdateList {
    pub fn new() -> TextureUpdateList {
        TextureUpdateList {
            updates: Vec::new(),
        }
    }

    #[inline]
    pub fn push(&mut self, update: TextureUpdate) {
        self.updates.push(update);
    }
}

/// Mostly wraps a tiling::Frame, adding a bit of extra information.
pub struct RendererFrame {
    /// The last rendered epoch for each pipeline present in the frame.
    /// This information is used to know if a certain transformation on the layout has
    /// been rendered, which is necessary for reftests.
    pub pipeline_epoch_map: HashMap<PipelineId, Epoch, BuildHasherDefault<FnvHasher>>,
    /// The layers that are currently affected by the over-scrolling animation.
    pub layers_bouncing_back: HashSet<ScrollLayerId, BuildHasherDefault<FnvHasher>>,

    pub frame: Option<tiling::Frame>,
}

impl RendererFrame {
    pub fn new(pipeline_epoch_map: HashMap<PipelineId, Epoch, BuildHasherDefault<FnvHasher>>,
               layers_bouncing_back: HashSet<ScrollLayerId, BuildHasherDefault<FnvHasher>>,
               frame: Option<tiling::Frame>)
               -> RendererFrame {
        RendererFrame {
            pipeline_epoch_map: pipeline_epoch_map,
            layers_bouncing_back: layers_bouncing_back,
            frame: frame,
        }
    }
}

pub enum ResultMsg {
    UpdateTextureCache(TextureUpdateList),
    RefreshShader(PathBuf),
    NewFrame(RendererFrame, BackendProfileCounters),
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct StackingContextIndex(pub usize);

#[derive(Clone, Copy, Debug)]
pub struct RectUv<T, U = UnknownUnit> {
    pub top_left: TypedPoint2D<T, U>,
    pub top_right: TypedPoint2D<T, U>,
    pub bottom_left: TypedPoint2D<T, U>,
    pub bottom_right: TypedPoint2D<T, U>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LowLevelFilterOp {
    Blur(Au, AxisDirection),
    Brightness(Au),
    Contrast(Au),
    Grayscale(Au),
    /// Fixed-point in `ANGLE_FLOAT_TO_FIXED` units.
    HueRotate(i32),
    Invert(Au),
    Opacity(Au),
    Saturate(Au),
    Sepia(Au),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompositionOp {
    MixBlend(MixBlendMode),
    Filter(LowLevelFilterOp),
}