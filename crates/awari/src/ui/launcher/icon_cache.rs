use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use futures::FutureExt;

use gpui::{
    App, AppContext, Asset, AssetLogger, DevicePixels, Entity, ImageAssetLoader, ImageCache,
    ImageCacheError, ImageCacheItem, RenderImage, Resource, Size, SvgRenderer, SvgSize, Window, hash,
};
use image::{imageops, Frame, ImageBuffer, Rgba};

/// Maximum number of icon textures retained in the launcher's per-panel image
/// cache. Launcher icon lists are bounded (at most a few hundred installed apps,
/// not infinite scroll), so this caps resident GPU textures during a long-lived
/// resident session instead of growing without bound the way the default global
/// asset cache does.
///
/// Unlike the previous `ICON_CACHE_CAP` (256), this bound is deliberately kept
/// *smaller* than the total number of icons a panel can show (the Apps grid has
/// 63 rows). gpui uploads one sprite-atlas texture per distinct `RenderImage`
/// and only frees it on `cx.drop_image`. With a cap above the row count nothing
/// was ever pruned, so every icon that scrolled past stayed resident. By keeping
/// the cap only modestly above the maximum *simultaneously visible* icon count
/// (~24 grid tiles at 500px panel height) we drop the atlas textures of icons
/// that have scrolled out while still never evicting one that is on screen.
pub(crate) const ICON_GPU_RETENTION: usize = 48;

/// Maximum pixel dimension (longest side) of any cached icon texture. App icons
/// are often 512px+, so storing them at native resolution would cost ~1MB+ each
/// even though they are displayed at ~30-50px. Decoding down to this bound
/// before caching keeps each texture to a few dozen KB, so the cache is bounded
/// in *bytes* (not just entry count) — only a fraction of the unbound cost.
pub(crate) const ICON_TEXTURE_MAX: u32 = 160;

/// A 1x1 transparent image returned when an icon cannot be decoded, so a broken
/// path never panics and never registers a full-resolution image in gpui's
/// sprite atlas.
fn placeholder_image() -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![Frame::new(
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(1, 1, Rgba([0, 0, 0, 0])),
    )]))
}

/// Shrink raster `bytes` to at most `max` pixels on the longest side on the
/// CPU, producing a BGRA `RenderImage` directly. Returns `None` when the bytes
/// are not a decodable raster image.
fn decode_raster(bytes: &[u8], max: u32) -> Option<Arc<RenderImage>> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.into_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    if width == 0 || height == 0 {
        return None;
    }
    let scale = max as f32 / width.max(height) as f32;
    let new_width = (width as f32 * scale).max(1.0) as u32;
    let new_height = (height as f32 * scale).max(1.0) as u32;
    let mut resized = imageops::resize(&rgba, new_width, new_height, imageops::FilterType::Triangle);
    // gpui's `RenderImage` stores pixels in BGRA order; `image` decodes RGBA.
    for pixel in resized.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![Frame::new(resized)])))
}

/// Load an icon straight from disk at a downscaled size, bypassing gpui's
/// `ImageAssetLoader`/`AssetLogger` path entirely so that no full-resolution
/// `RenderImage` is ever constructed or pinned in gpui's sprite atlas.
///
/// * SVG icons are rasterized with gpui's resvg-backed `SvgRenderer` at the
///   target size (the `image` crate cannot decode SVG).
/// * Everything else is decoded with the `image` crate and shrunk on the CPU.
/// * Any failure yields a 1x1 transparent placeholder rather than a panic or a
///   full-res image being retained.
fn load_icon_from_path(
    path: &Arc<Path>,
    renderer: &SvgRenderer,
    max: u32,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    let bytes = std::fs::read(path).map_err(|e| ImageCacheError::Io(Arc::new(e)))?;

    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
        .unwrap_or(false);

    let size = Size::new(DevicePixels(max as i32), DevicePixels(max as i32));
    if is_svg {
        return renderer
            .parse_svg(&bytes)
            .ok()
            .and_then(|parsed| renderer.render_parsed(&parsed, SvgSize::Size(size)).ok())
            .map(Ok)
            .unwrap_or_else(|| Ok(placeholder_image()));
    }

    if let Some(image) = decode_raster(&bytes, max) {
        return Ok(image);
    }

    // Raster decode failed: try SVG without a recognized extension before
    // giving up, then fall back to the placeholder.
    if let Some(image) = renderer
        .parse_svg(&bytes)
        .ok()
        .and_then(|parsed| renderer.render_parsed(&parsed, SvgSize::Size(size)).ok())
    {
        return Ok(image);
    }

    Ok(placeholder_image())
}

pub(crate) struct BoundedImageCache {
    max_items: usize,
    usages: Vec<u64>,
    cache: HashMap<u64, ImageCacheItem>,
}

impl BoundedImageCache {
    pub(crate) fn new(max_items: usize, cx: &mut App) -> Entity<Self> {
        let entity = cx.new(|_cx| Self {
            max_items,
            usages: Vec::with_capacity(max_items),
            cache: HashMap::with_capacity(max_items),
        });
        cx.observe_release(&entity, |cache, cx| {
            for (_, mut item) in std::mem::take(&mut cache.cache) {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        entity
    }

    /// Drop every cached icon and forget all entries, releasing GPU texture
    /// memory. Called when the launcher hides so a resident overlay holds zero
    /// icon memory while not painted. Rows re-decode on the next show — the
    /// deliberate re-decode cost for a launcher whose hidden time vastly
    /// outweighs its visible time.
    pub(crate) fn clear(&mut self, cx: &mut App) {
        for (_, mut item) in std::mem::take(&mut self.cache) {
            if let Some(Ok(image)) = item.get() {
                cx.drop_image(image.clone(), None);
            }
        }
        self.usages.clear();
    }

    fn mark_used(&mut self, key: u64) {
        if let Some(pos) = self.usages.iter().position(|entry| *entry == key) {
            self.usages.remove(pos);
        }
        self.usages.insert(0, key);
    }

    /// Free gpui's retained sprite-atlas texture for every cached icon whose LRU
    /// position is beyond `self.max_items`, while keeping the decoded
    /// `Arc<RenderImage>` in the CPU-side `cache` so it can be re-uploaded cheaply
    /// when the row scrolls back into view.
    ///
    /// This bounds gpui's per-icon retained memory to a small window of recently
    /// shown icons instead of every icon ever rendered this session (which
    /// previously grew RSS by ~1MiB+ per distinct icon and was only reclaimed on
    /// hide). It is safe to call mid-frame: every icon currently on screen is
    /// re-loaded (and therefore re-uploaded) later in the same frame, so dropping
    /// its atlas texture here never leaves it blank.
    fn evict_gpu_beyond(&mut self, cx: &mut App) {
        while self.usages.len() > self.max_items {
            let oldest = self
                .usages
                .pop()
                .expect("usages and cache must stay in sync");
            if let Some(item) = self.cache.get_mut(&oldest)
                && let Some(Ok(image)) = item.get()
            {
                cx.drop_image(image.clone(), None);
            }
        }
    }
}

impl ImageCache for BoundedImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let key = hash(resource);

        if self.cache.contains_key(&key) {
            self.mark_used(key);
            return self.cache.get_mut(&key).unwrap().get();
        }

        let renderer = cx.svg_renderer();

        // Path icons are decoded locally at a downscaled size so we never route
        // a full-resolution image through gpui's asset/atlas machinery. Non-Path
        // resources keep the previous gpui-backed fallback for safety.
        let task = match &resource {
            Resource::Path(path) => {
                let path = path.clone();
                let renderer = renderer.clone();
                cx.background_executor()
                    .spawn(async move {
                        load_icon_from_path(&path, &renderer, ICON_TEXTURE_MAX)
                    })
                    .shared()
            }
            _ => {
                let fallback = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
                cx.background_executor().spawn(fallback).shared()
            }
        };

        self.evict_gpu_beyond(cx);

        self.cache.insert(key, ImageCacheItem::Loading(task.clone()));
        self.mark_used(key);

        let entity = window.current_view();
        window
            .spawn(cx, {
                async move |cx| {
                    _ = task.await;
                    cx.on_next_frame(move |_, cx| {
                        cx.notify(entity);
                    });
                }
            })
            .detach();

        None
    }
}
