//! iOS-specific text system.
//!
//! This is a copy of the macOS text system with iOS-specific adaptations:
//! - CGFloat is defined locally instead of imported from cocoa
//! - CGPoint is imported from core_graphics::geometry (iOS-compatible path)

use crate::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, Point,
    RenderGlyphParams, Result, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ShapedGlyph, ShapedRun,
    SharedString, Size, point, px, size, swap_rgba_pa_to_bgra,
};
use anyhow::anyhow;

// CGFloat type - defined locally for iOS (always f64 on 64-bit)
#[allow(non_camel_case_types)]
type CGFloat = f64;

use collections::HashMap;
use core_foundation::{
    attributed_string::CFMutableAttributedString,
    base::{CFRange, TCFType},
    number::CFNumber,
    string::CFString,
};
use core_graphics::{
    base::{CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::CGContext,
    geometry::CGPoint,
};
use core_text::{
    font::CTFont,
    font_descriptor::{
        kCTFontSlantTrait, kCTFontSymbolicTrait, kCTFontWeightTrait, kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    metrics::Metrics,
    properties::{Style as FontkitStyle, Weight as FontkitWeight},
    source::SystemSource,
    sources::mem::MemSource,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, cmp, convert::TryFrom, sync::Arc};

use super::open_type::apply_features_and_fallbacks;

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;

pub(crate) struct MacTextSystem(RwLock<MacTextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    font_family: SharedString,
    font_features: FontFeatures,
    font_fallbacks: Option<FontFallbacks>,
}

struct MacTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_postscript_name: HashMap<String, FontId>,
    font_ids_by_font_key: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
}

impl MacTextSystem {
    /// Creates a new, empty MacTextSystem backed by fresh internal state.
    ///
    /// The returned instance contains empty memory and system font sources and cleared font caches.
    ///
    /// # Examples
    ///
    /// ```
    /// let ts = MacTextSystem::new();
    /// ```
    pub(crate) fn new() -> Self {
        Self(RwLock::new(MacTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            font_selections: HashMap::default(),
            font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_font_key: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
        }))
    }
}

impl Default for MacTextSystem {
    /// Creates the default value for this type.
    ///
    /// # Returns
    ///
    /// `Self` initialized with the type's default configuration.
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for MacTextSystem {
    /// Adds the given font data to the text system's in-memory font source.
    ///
    /// Each entry in `fonts` is a font file's bytes (owned or borrowed) that will be registered for use by the text system.
    ///
    /// # Returns
    ///
    /// `Ok(())` if all fonts were added successfully, `Err` if an error occurred while loading any font.
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    /// Collects available font family names from the system and from in-memory fonts.
    ///
    /// Queries CoreText for all family descriptors, extracts family names using
    /// lenient attribute access, and appends any families currently registered in
    /// the in-memory font source.
    ///
    /// # Returns
    ///
    /// A `Vec<String>` containing font family names discovered from the system and
    /// in-memory fonts. The vector may be empty if no names could be retrieved.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Obtain a MacTextSystem instance from the surrounding context and list families.
    /// let names: Vec<String> = /* mac_text_system */.all_font_names();
    /// println!("Found {} font families", names.len());
    /// ```
    fn all_font_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let collection = core_text::font_collection::create_for_all_families();
        let Some(descriptors) = collection.get_descriptors() else {
            return names;
        };
        for descriptor in descriptors.into_iter() {
            names.extend(lenient_font_attributes::family_name(&descriptor));
        }
        if let Ok(fonts_in_memory) = self.0.read().memory_source.all_families() {
            names.extend(fonts_in_memory);
        }
        names
    }

    /// Resolve the best-matching `FontId` for the given `Font` request.
    ///
    /// Looks up a cached selection and returns it if present; otherwise it loads
    /// candidate fonts for the requested family (including memory and system
    /// sources), evaluates their properties against the requested style/weight,
    /// selects the best match, caches that selection, and returns its `FontId`.
    ///
    /// # Errors
    ///
    /// Returns an error if font family loading or best-match selection fails.
    ///
    /// # Examples
    ///
    /// ```
    /// // `text_system` implements this method and `font` is a Font request.
    /// // This demonstrates the common usage pattern.
    /// let id = text_system.font_id(&font).unwrap();
    /// ```
    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let font_key = FontKey {
                font_family: font.family.clone(),
                font_features: font.features.clone(),
                font_fallbacks: font.fallbacks.clone(),
            };
            let candidates = if let Some(font_ids) = lock.font_ids_by_font_key.get(&font_key) {
                font_ids.as_slice()
            } else {
                let font_ids =
                    lock.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
                lock.font_ids_by_font_key.insert(font_key.clone(), font_ids);
                lock.font_ids_by_font_key[&font_key].as_ref()
            };

            let candidate_properties = candidates
                .iter()
                .map(|font_id| lock.fonts[font_id.0].properties())
                .collect::<SmallVec<[_; 4]>>();

            let ix = font_kit::matching::find_best_match(
                &candidate_properties,
                &font_kit::properties::Properties {
                    style: font.style.into(),
                    weight: font.weight.into(),
                    stretch: Default::default(),
                },
            )?;

            let font_id = candidates[ix];
            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }

    /// Retrieves the typographic metrics for the specified font.
    ///
    /// The returned `FontMetrics` contains values such as ascent, descent, line gap,
    /// units-per-em, and the font's bounding box.
    ///
    /// # Examples
    ///
    /// ```
    /// // `system` must be an initialized MacTextSystem and `id` a valid FontId.
    /// let metrics = system.font_metrics(id);
    /// assert!(metrics.units_per_em > 0);
    /// ```
    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.read().fonts[font_id.0].metrics().into()
    }

    /// Returns the typographic bounding box for a glyph in the given font.
    ///
    — /// The returned bounds describe the glyph's typographic extents in font units converted to `f32`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assume `sys` implements the same trait and types are in scope.
    /// let bounds = sys.typographic_bounds(FontId(0), GlyphId(0)).unwrap();
    /// // Bounds should be a finite rectangle (width/height may be zero for empty glyphs).
    /// assert!(bounds.size.width.is_finite());
    /// ```
    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(self.0.read().fonts[font_id.0]
            .typographic_bounds(glyph_id.0)?
            .into())
    }

    /// Get the glyph advance (horizontal and vertical advance) for a glyph in a font.
    ///
    /// # Returns
    ///
    /// `Ok(Size<f32>)` containing the glyph's advance (x and y) in logical pixels; `Err` if the requested font or glyph cannot be resolved.
    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    /// Maps a Unicode character to the glyph identifier used by the specified font.
    ///
    /// Returns `Some(GlyphId)` if the font provides a glyph for `ch`, `None` if the character has no glyph in that font.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `ts` is a MacTextSystem; `font_id` is a FontId obtained from the text system.
    /// let glyph = ts.glyph_for_char(font_id, 'a');
    /// match glyph {
    ///     Some(gid) => println!("Glyph id: {:?}", gid),
    ///     None => println!("No glyph for character"),
    /// }
    /// ```
    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    /// Computes the device-pixel bounding rectangle required to rasterize the specified glyph.
    ///
    /// Returns the smallest axis-aligned `Bounds<DevicePixels>` that fully contains the glyph's
    /// raster area for the provided `RenderGlyphParams`, taking scale and subpixel positioning
    /// into account.
    ///
    /// # Examples
    ///
    /// ```
    /// // `text_system` is a `MacTextSystem` and `params` is a prepared `RenderGlyphParams`.
    /// // let bounds = text_system.glyph_raster_bounds(&params).unwrap();
    /// ```
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    /// Rasterizes a single glyph into a pixel bitmap using the provided render parameters and raster bounds.
    ///
    /// # Returns
    ///
    /// A tuple `(Size<DevicePixels>, Vec<u8>)` with the bitmap size and the raw pixel bytes. Color emoji glyphs are returned as BGRA straight-alpha pixels; non-color glyphs are returned as grayscale bytes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assume `text_system` is a MacTextSystem and `params` and `bounds` are prepared:
    /// // let (size, pixels) = text_system.rasterize_glyph(&params, bounds).unwrap();
    /// ```
    fn rasterize_glyph(
        &self,
        glyph_id: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(glyph_id, raster_bounds)
    }

    /// Layout a single line of text into shaped runs using the supplied font runs.
    ///
    /// The returned `LineLayout` contains shaped runs, glyph positions, typographic bounds,
    /// and the original text length for the laid-out line.
    ///
    /// # Examples
    ///
    /// ```
    /// // `text_system` is a `MacTextSystem` previously created and configured.
    /// let layout = text_system.layout_line("Hello, world!", 12.0.into(), &[]);
    /// assert_eq!(layout.text_length, "Hello, world!".len());
    /// ```
    fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, font_runs)
    }
}

impl MacTextSystemState {
    /// Adds fonts to the internal memory font source from either embedded slices or owned byte buffers.
    ///
    /// Accepts a list of font data items where each item is either a borrowed byte slice (embedded font)
    /// or an owned byte buffer. Embedded slices are converted through Core Graphics/Core Text into a
    /// memory handle; owned buffers are registered directly. Returns `Ok(())` if all fonts were loaded
    /// and registered successfully, or an `Err` if any font failed to be converted or if registration
    /// into the memory source failed.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::borrow::Cow;
    ///
    /// // Add an owned font blob
    /// let owned_font: Vec<u8> = vec![/* font bytes */];
    /// let fonts = vec![Cow::Owned(owned_font)];
    /// // state.add_fonts(fonts).unwrap();
    ///
    /// // Add an embedded/font slice
    /// // let embedded: &'static [u8] = include_bytes!("SomeFont.ttf");
    /// // let fonts = vec![Cow::Borrowed(embedded)];
    /// // state.add_fonts(fonts).unwrap();
    /// ```
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let fonts = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(embedded_font) => {
                    let data_provider = unsafe {
                        core_graphics::data_provider::CGDataProvider::from_slice(embedded_font)
                    };
                    let font = core_graphics::font::CGFont::from_data_provider(data_provider)
                        .map_err(|()| anyhow!("Could not load an embedded font."))?;
                    let font = font_kit::loaders::core_text::Font::from_core_graphics_font(font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(fonts.into_iter())?;
        Ok(())
    }

    /// Loads and fonts for a family name, applies font features and optional fallbacks, and registers the loaded fonts.
    ///
    /// Attempts to load the family from the in-memory source first, falling back to the system source. For each successfully loaded font this registers a new `FontId`, records mappings by PostScript name, and returns the assigned `FontId`s. Fonts lacking required glyphs, readable trait values, or a PostScript name are skipped; ".SystemUIFont" is normalized to ".AppleSystemUIFont".
    ///
    /// # Parameters
    ///
    /// - `name` — family name to load (".SystemUIFont" is normalized to ".AppleSystemUIFont").
    /// - `features` — OpenType features to apply to each loaded font.
    /// - `fallbacks` — optional fallback font configuration to apply.
    ///
    /// # Returns
    ///
    /// A `SmallVec<[FontId; 4]>` containing the `FontId`s of the fonts successfully loaded and registered.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Load fonts for the "Helvetica" family with default features and no fallbacks.
    /// let font_ids = state.load_family("Helvetica", &FontFeatures::default(), None).unwrap();
    /// assert!(!font_ids.is_empty());
    /// ```
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let name = if name == ".SystemUIFont" {
            ".AppleSystemUIFont"
        } else {
            name
        };

        let mut font_ids = SmallVec::new();
        let family = self
            .memory_source
            .select_family_by_name(name)
            .or_else(|_| self.system_source.select_family_by_name(name))?;
        for font in family.fonts() {
            let mut font = font.load()?;

            apply_features_and_fallbacks(&mut font, features, fallbacks)?;
            // This block contains a precautionary fix to guard against loading fonts
            // that might cause panics due to `.unwrap()`s up the chain.
            {
                // We use the 'm' character for text measurements in various spots
                // (e.g., the editor). However, at time of writing some of those usages
                // will panic if the font has no 'm' glyph.
                //
                // Therefore, we check up front that the font has the necessary glyph.
                let has_m_glyph = font.glyph_for_char('m').is_some();

                // HACK: The 'Segoe Fluent Icons' font does not have an 'm' glyph,
                // but we need to be able to load it for rendering Windows icons in
                // the Storybook (on macOS).
                let is_segoe_fluent_icons = font.full_name() == "Segoe Fluent Icons";

                if !has_m_glyph && !is_segoe_fluent_icons {
                    // I spent far too long trying to track down why a font missing the 'm'
                    // character wasn't loading. This log statement will hopefully save
                    // someone else from suffering the same fate.
                    log::warn!(
                        "font '{}' has no 'm' character and was not loaded",
                        font.full_name()
                    );
                    continue;
                }
            }

            // We've seen a number of panics in production caused by calling font.properties()
            // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
            // and to try and identify the incalcitrant font.
            let traits = font.native_font().all_traits();
            if unsafe {
                !(traits
                    .get(kCTFontSymbolicTrait)
                    .downcast::<CFNumber>()
                    .is_some()
                    && traits
                        .get(kCTFontWidthTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontWeightTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontSlantTrait)
                        .downcast::<CFNumber>()
                        .is_some())
            } {
                log::error!(
                    "Failed to read traits for font {:?}",
                    font.postscript_name().unwrap_or_else(|| "<unknown>".to_string())
                );
                continue;
            }

            let Some(postscript_name) = font.postscript_name() else {
                log::error!(
                    "Failed to get postscript name for font {:?}",
                    font.full_name()
                );
                continue;
            };

            let font_id = FontId(self.fonts.len());
            font_ids.push(font_id);
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
            self.fonts.push(font);
        }
        Ok(font_ids)
    }

    /// Get the advance size for a glyph in the specified font.
    ///
    /// # Returns
    ///
    /// `Size<f32>` containing the horizontal and vertical advance for the glyph, or an error if the font-kit query fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assuming `state`, `font_id`, and `glyph_id` are available:
    /// let size = state.advance(font_id, glyph_id).unwrap();
    /// assert!(size.width >= 0.0);
    /// ```
    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(self.fonts[font_id.0].advance(glyph_id.0)?.into())
    }

    /// Retrieve the glyph identifier for a Unicode character in the specified font.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assume `state` is a MacTextSystemState with loaded fonts and `fid` a valid `FontId`.
    /// let gid = state.glyph_for_char(fid, 'a');
    /// ```
    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts[font_id.0].glyph_for_char(ch).map(GlyphId)
    }

    /// Resolve or register the given native Core Text font and return its corresponding FontId.
    ///
    /// If the font's PostScript name is already known, the existing FontId is returned. Otherwise a
    /// new FontId is created, the PostScript name is recorded, and a FontKit `Font` wrapper is
    /// constructed from the provided `CTFont` and stored.
    ///
    /// # Returns
    ///
    /// The `FontId` associated with the provided `CTFont`.
    fn id_for_native_font(&mut self, requested_font: CTFont) -> FontId {
        let postscript_name = requested_font.postscript_name();
        if let Some(font_id) = self.font_ids_by_postscript_name.get(&postscript_name) {
            *font_id
        } else {
            let font_id = FontId(self.fonts.len());
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
            self.fonts
                .push(font_kit::font::Font::from_core_graphics_font(
                    requested_font.copy_to_CGFont(),
                ));
            font_id
        }
    }

    /// Determines whether the specified font is Apple's color emoji font.
    ///
    /// # Returns
    ///
    /// `true` if the font's PostScript name is "AppleColorEmoji" or ".AppleColorEmojiUI", `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `state` is a MacTextSystemState and `font_id` obtained from it.
    /// // let is_emoji = state.is_emoji(font_id);
    /// ```
    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id
            .get(&font_id)
            .map_or(false, |postscript_name| {
                postscript_name == "AppleColorEmoji" || postscript_name == ".AppleColorEmojiUI"
            })
    }

    /// Computes the pixel bounds required to rasterize a glyph using the provided render parameters.
    ///
    /// The returned bounds are in device pixels and reflect the font size and scale factor from `params`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `state: &MacTextSystemState` and `params: RenderGlyphParams` are available:
    /// let bounds = state.raster_bounds(&params).unwrap();
    /// println!("Glyph raster bounds: {:?}", bounds);
    /// ```
    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let scale = Transform2F::from_scale(params.scale_factor);
        Ok(font
            .raster_bounds(
                params.glyph_id.0,
                params.font_size.into(),
                scale,
                HintingOptions::None,
                font_kit::canvas::RasterizationOptions::GrayscaleAa,
            )?
            .into())
    }

    /// Rasterizes a single glyph into a pixel bitmap.
    ///
    /// Returns the bitmap size and a flat pixel buffer. For emoji glyphs the buffer contains
    /// 4-byte pixels in BGRA order with straight alpha; for non-emoji glyphs the buffer contains
    /// one byte per pixel (grayscale). Returns an `Err` if `glyph_bounds` has zero width or height.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Prepare RenderGlyphParams and glyph_bounds appropriately, then:
    /// let (size, pixels) = text_system.rasterize_glyph(&params, glyph_bounds)?;
    /// // `size` is the bitmap dimensions; `pixels` is the raw pixel data as described above.
    /// ```
    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            if params.subpixel_variant.x > 0 {
                bitmap_size.width += DevicePixels(1);
            }
            if params.subpixel_variant.y > 0 {
                bitmap_size.height += DevicePixels(1);
            }
            let bitmap_size = bitmap_size;

            let mut bytes;
            let cx;
            if params.is_emoji {
                bytes = vec![0; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaPremultipliedLast,
                );
            } else {
                bytes = vec![0; bitmap_size.width.0 as usize * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize,
                    &CGColorSpace::create_device_gray(),
                    kCGImageAlphaOnly,
                );
            }

            // Move the origin to bottom left and account for scaling, this
            // makes drawing text consistent with the font-kit's raster_bounds.
            cx.translate(
                -glyph_bounds.origin.x.0 as CGFloat,
                (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
            );
            cx.scale(
                params.scale_factor as CGFloat,
                params.scale_factor as CGFloat,
            );

            let subpixel_shift = point(
                params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32,
                params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32,
            );
            cx.set_allows_font_subpixel_positioning(true);
            cx.set_should_subpixel_position_fonts(true);
            cx.set_allows_font_subpixel_quantization(false);
            cx.set_should_subpixel_quantize_fonts(false);
            self.fonts[params.font_id.0]
                .native_font()
                .clone_with_font_size(f32::from(params.font_size) as CGFloat)
                .draw_glyphs(
                    &[params.glyph_id.0 as CGGlyph],
                    &[CGPoint::new(
                        (subpixel_shift.x / params.scale_factor) as CGFloat,
                        (subpixel_shift.y / params.scale_factor) as CGFloat,
                    )],
                    cx,
                );

            if params.is_emoji {
                // Convert from RGBA with premultiplied alpha to BGRA with straight alpha.
                for pixel in bytes.chunks_exact_mut(4) {
                    swap_rgba_pa_to_bgra(pixel);
                }
            }

            Ok((bitmap_size, bytes))
        }
    }

    /// Layouts and shapes a single line of text using the provided font runs.
    ///
    /// Produces a LineLayout that contains shaped runs (per-font glyph sequences with positions),
    /// maps glyphs back to UTF-8 string indices, marks glyphs coming from emoji fonts,
    /// and reports typographic metrics (width, ascent, descent) for the shaped line.
    ///
    /// # Examples
    ///
    /// ```
    /// // `state` must be a MacTextSystemState (with fonts loaded) in scope.
    /// // This demonstrates the API usage; test harness must provide a valid state.
    /// let layout = state.layout_line("hello", Pixels(12.0), &[]);
    /// assert_eq!(layout.len, 5);
    /// ```
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        // Construct the attributed string, converting UTF8 ranges to UTF16 ranges.
        let mut string = CFMutableAttributedString::new();
        {
            string.replace_str(&CFString::new(text), CFRange::init(0, 0));
            let utf16_line_len = string.char_len() as usize;

            let mut ix_converter = StringIndexConverter::new(text);
            for run in font_runs {
                let utf8_end = ix_converter.utf8_ix + run.len;
                let utf16_start = ix_converter.utf16_ix;

                if utf16_start >= utf16_line_len {
                    break;
                }

                ix_converter.advance_to_utf8_ix(utf8_end);
                let utf16_end = cmp::min(ix_converter.utf16_ix, utf16_line_len);

                let cf_range =
                    CFRange::init(utf16_start as isize, (utf16_end - utf16_start) as isize);

                let font: &FontKitFont = &self.fonts[run.font_id.0];

                unsafe {
                    string.set_attribute(
                        cf_range,
                        kCTFontAttributeName,
                        &font.native_font().clone_with_font_size(font_size.into()),
                    );
                }

                if utf16_end == utf16_line_len {
                    break;
                }
            }
        }

        // Retrieve the glyphs from the shaped line, converting UTF16 offsets to UTF8 offsets.
        let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
        let glyph_runs = line.glyph_runs();
        let mut runs = Vec::with_capacity(glyph_runs.len() as usize);
        let mut ix_converter = StringIndexConverter::new(text);
        for run in glyph_runs.into_iter() {
            let attributes = run.attributes().unwrap();
            let font = unsafe {
                attributes
                    .get(kCTFontAttributeName)
                    .downcast::<CTFont>()
                    .unwrap()
            };
            let font_id = self.id_for_native_font(font);

            let mut glyphs = Vec::with_capacity(run.glyph_count().try_into().unwrap_or(0));
            for ((glyph_id, position), glyph_utf16_ix) in run
                .glyphs()
                .iter()
                .zip(run.positions().iter())
                .zip(run.string_indices().iter())
            {
                let glyph_utf16_ix = usize::try_from(*glyph_utf16_ix).unwrap();
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(*glyph_id as u32),
                    position: point(position.x as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
            }

            runs.push(ShapedRun { font_id, glyphs });
        }
        let typographic_bounds = line.get_typographic_bounds();
        LineLayout {
            runs,
            font_size,
            width: typographic_bounds.width.into(),
            ascent: typographic_bounds.ascent.into(),
            descent: typographic_bounds.descent.into(),
            len: text.len(),
        }
    }
}

#[derive(Clone)]
struct StringIndexConverter<'a> {
    text: &'a str,
    utf8_ix: usize,
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    /// Creates a new StringIndexConverter for `text`.
    ///
    /// The converter starts with both the UTF-8 and UTF-16 indices set to 0 and
    /// can be advanced to map between UTF-8 and UTF-16 positions within `text`.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut conv = StringIndexConverter::new("a𐐷"); // 'a' + U+10437 (surrogate pair in UTF-16)
    /// assert_eq!(conv.utf8_ix, 0);
    /// assert_eq!(conv.utf16_ix, 0);
    ///
    /// conv.advance_to_utf8_ix(1);
    /// // After advancing to byte index 1, utf16 index should be 1 (the 'a')
    /// assert_eq!(conv.utf8_ix, 1);
    /// assert_eq!(conv.utf16_ix, 1);
    ///
    /// conv.advance_to_utf8_ix(4);
    /// // The 2nd character is a 4-byte UTF-8 code point, counting as two UTF-16 units
    /// assert_eq!(conv.utf8_ix, 4);
    /// assert_eq!(conv.utf16_ix, 3);
    /// ```
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    /// Advance the converter's position until the internal UTF-8 index is at or past `utf8_target`,
    /// keeping the UTF-16 index in sync with the traversed characters.
    ///
    /// If `utf8_target` is greater than the text length, the UTF-8 index is set to the end of the text.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut conv = StringIndexConverter::new("a\u{1F600}b"); // "a😀b"
    /// // advance to the byte index of the second character (the emoji starts at byte index 1)
    /// conv.advance_to_utf8_ix(1);
    /// ```
    fn advance_to_utf8_ix(&mut self, utf8_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf8_ix + ix >= utf8_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }

    /// Advance the converter's indices forward until the UTF-16 index reaches the target.
    ///
    /// Advances both `utf16_ix` and `utf8_ix` from their current positions by iterating
    /// Unicode scalar values; stops when `utf16_ix >= utf16_target` or the end of the text
    /// is reached. After returning, `utf8_ix` is the byte index corresponding to the
    /// current `utf16_ix`. If `utf16_target` is past the end, `utf8_ix` is set to `text.len()`.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut conv = StringIndexConverter::new("a𐐷b"); // '𐐷' is a surrogate pair in UTF-16
    /// conv.advance_to_utf16_ix(2);
    /// assert!(conv.utf16_ix >= 2);
    /// // utf8_ix is at a byte boundary corresponding to that UTF-16 index
    /// ```
    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

impl From<Metrics> for FontMetrics {
    /// Convert a `Metrics` value from font-kit into the crate's `FontMetrics`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let metrics: Metrics = /* obtained from font-kit */ ;
    /// let fm: FontMetrics = metrics.into();
    /// ```
    fn from(metrics: Metrics) -> Self {
        FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: metrics.bounding_box.into(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    /// Converts a floating-point rectangle into `Bounds<f32>` by using the rectangle's origin and size.
    ///
    /// # Examples
    ///
    /// ```
    /// let rect = RectF::new(1.0, 2.0, 3.0, 4.0);
    /// let b: Bounds<f32> = rect.into();
    /// assert_eq!(b.origin.x, 1.0);
    /// assert_eq!(b.origin.y, 2.0);
    /// assert_eq!(b.size.width, 3.0);
    /// assert_eq!(b.size.height, 4.0);
    /// ```
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    /// Converts a RectI into Bounds in device-pixel coordinates.
    ///
    /// # Examples
    ///
    /// ```
    /// let rect = RectI::new(1, 2, 3, 4);
    /// let bounds: Bounds<DevicePixels> = rect.into();
    /// assert_eq!(bounds.origin.x, DevicePixels(1));
    /// assert_eq!(bounds.origin.y, DevicePixels(2));
    /// assert_eq!(bounds.size.width, DevicePixels(3));
    /// assert_eq!(bounds.size.height, DevicePixels(4));
    /// ```
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    /// Create a size from the integer vector's x and y components.
    ///
    /// # Examples
    ///
    /// ```
    /// let v = Vector2I::new(3, 4);
    /// let s = Size::from(v);
    /// assert_eq!(s, size(3, 4));
    /// ```
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    /// Creates a `Bounds<i32>` from a `RectI` by converting the rectangle's origin and size into `Bounds` components.
    ///
    /// # Examples
    ///
    /// ```
    /// let rect = RectI::new(1, 2, 30, 40);
    /// let bounds: Bounds<i32> = rect.into();
    /// assert_eq!(bounds.origin.x, 1);
    /// assert_eq!(bounds.origin.y, 2);
    /// assert_eq!(bounds.size.width, 30);
    /// assert_eq!(bounds.size.height, 40);
    /// ```
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    /// Converts a point with unsigned coordinates into a 2D integer vector.
    ///
    /// # Examples
    ///
    /// ```
    /// let p = Point { x: 3u32, y: 5u32 };
    /// let v: Vector2I = p.into();
    /// assert_eq!(v.x, 3);
    /// assert_eq!(v.y, 5);
    /// ```
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    /// Create a Size from a 2D float vector.
    ///
    /// # Examples
    ///
    /// ```
    /// let v = Vector2F::new(3.0, 4.5);
    /// let s = Size::from(v);
    /// let expected = size(3.0, 4.5);
    /// assert_eq!(s, expected);
    /// ```
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for FontkitWeight {
    /// Converts a `FontWeight` into a `FontkitWeight`.
    ///
    /// # Examples
    ///
    /// ```
    /// let fw = FontWeight(400);
    /// let kw: FontkitWeight = fw.into();
    /// assert_eq!(kw, FontkitWeight(400));
    /// ```
    fn from(value: FontWeight) -> Self {
        FontkitWeight(value.0)
    }
}

impl From<FontStyle> for FontkitStyle {
    /// Converts a `FontStyle` into the corresponding `FontkitStyle`.
    ///
    /// # Examples
    ///
    /// ```
    /// let s = FontStyle::Italic;
    /// let ks: FontkitStyle = s.into();
    /// assert_eq!(ks, FontkitStyle::Italic);
    /// ```
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => FontkitStyle::Normal,
            FontStyle::Italic => FontkitStyle::Italic,
            FontStyle::Oblique => FontkitStyle::Oblique,
        }
    }
}

// Some fonts may have no attributes despite `core_text` requiring them (and panicking).
// This is the same version as `core_text` has without `expect` calls.
mod lenient_font_attributes {
    use core_foundation::{
        base::{CFRetain, CFType, TCFType},
        string::{CFString, CFStringRef},
    };
    use core_text::font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttribute, kCTFontFamilyNameAttribute,
    };

    /// Safely retrieves the family name from a Core Text font descriptor.
    ///
    /// Attempts to read the `kCTFontFamilyNameAttribute` from `descriptor` and returns it as an owned
    /// Rust `String` if present.
    ///
    /// # Examples
    ///
    /// ```
    /// use core_text::font_descriptor::CTFontDescriptor;
    /// use crate::lenient_font_attributes::family_name;
    ///
    /// let desc = CTFontDescriptor::from_name("Helvetica");
    /// let name = family_name(&desc);
    /// assert_eq!(name.as_deref(), Some("Helvetica"));
    /// ```
    pub fn family_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { get_string_attribute(descriptor, kCTFontFamilyNameAttribute) }
    }

    /// Safely retrieves a string-valued attribute from a font descriptor.
    ///
    /// Returns `None` when the attribute is absent; otherwise converts the attribute's `CFString` value to a Rust `String`. Panics if the attribute exists but is not a `CFString`.
    fn get_string_attribute(
        descriptor: &CTFontDescriptor,
        attribute: CFStringRef,
    ) -> Option<String> {
        unsafe {
            let value = CTFontDescriptorCopyAttribute(descriptor.as_concrete_TypeRef(), attribute);
            if value.is_null() {
                return None;
            }

            let value = CFType::wrap_under_create_rule(value);
            assert!(value.instance_of::<CFString>());
            let s = wrap_under_get_rule(value.as_CFTypeRef() as CFStringRef);
            Some(s.to_string())
        }
    }

    /// Create an owned `CFString` from an existing `CFStringRef` by retaining the reference.
    
    ///
    
    /// # Safety
    
    ///
    
    /// The caller must provide a valid, non-null `CFStringRef`. The returned `CFString` takes ownership
    
    /// of an additional retain on the supplied reference; the caller must ensure the original reference
    
    /// remains valid for this operation.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```
    
    /// // `existing` must be a valid CFStringRef obtained from Core Foundation APIs.
    
    /// let existing: CFStringRef = /* existing non-null CFStringRef */ std::ptr::null_mut();
    
    /// let owned: CFString = unsafe { wrap_under_get_rule(existing) };
    
    /// assert!(!owned.as_concrete_TypeRef().is_null());
    
    /// ```
    unsafe fn wrap_under_get_rule(reference: CFStringRef) -> CFString {
        unsafe {
            assert!(!reference.is_null(), "Attempted to create a NULL object.");
            let reference = CFRetain(reference as *const ::std::os::raw::c_void) as CFStringRef;
            TCFType::wrap_under_create_rule(reference)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{FontRun, GlyphId, MacTextSystem, PlatformTextSystem, font, px};

    #[test]
    fn test_layout_line_bom_char() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let line = "\u{feff}";
        let mut style = FontRun {
            font_id,
            len: line.len(),
        };

        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b
    }
}