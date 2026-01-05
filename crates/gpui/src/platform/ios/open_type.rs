//! iOS-specific OpenType font feature handling.
//!
//! This is a copy of the macOS open_type module with iOS-specific adaptations:
//! - CGFloat is defined locally instead of imported from cocoa

#![allow(unused, non_upper_case_globals)]

use crate::{FontFallbacks, FontFeatures};

// CGFloat type - defined locally for iOS (always f64 on 64-bit)
#[allow(non_camel_case_types)]
type CGFloat = f64;

use core_foundation::{
    array::{
        CFArray, CFArrayAppendArray, CFArrayAppendValue, CFArrayCreateMutable, CFArrayGetCount,
        CFArrayGetValueAtIndex, CFArrayRef, CFMutableArrayRef, kCFTypeArrayCallBacks,
    },
    base::{CFRelease, TCFType, kCFAllocatorDefault},
    dictionary::{
        CFDictionaryCreate, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    },
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_foundation_sys::locale::CFLocaleCopyPreferredLanguages;
use core_graphics::geometry::CGAffineTransform;
use core_text::{
    font::{CTFont, CTFontRef, cascade_list_for_languages},
    font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttributes, CTFontDescriptorCreateCopyWithFeature,
        CTFontDescriptorCreateWithAttributes, CTFontDescriptorCreateWithNameAndSize,
        CTFontDescriptorRef, kCTFontCascadeListAttribute, kCTFontFeatureSettingsAttribute,
    },
};
use font_kit::font::Font as FontKitFont;
use std::ptr;

/// Apply OpenType feature settings and an optional fallback cascade to the given font, replacing the
/// provided FontKitFont in place.
///
/// The function updates the font by creating a new font descriptor containing the requested OpenType
/// features and, when `fallbacks` is `Some` and non-empty, a cascade list of fallback descriptors,
/// then constructs a new CTFont from that descriptor and assigns it to `font`.
///
/// # Parameters
///
/// - `font`: the FontKitFont to update; the function replaces this value with the adjusted font.
/// - `features`: OpenType feature settings to apply.
/// - `fallbacks`: optional fallback configuration; if `None` or its list is empty, no cascade fallbacks
///   are applied.
///
/// # Returns
///
/// `Ok(())` on success, `Err(_)` with context if an error occurs.
///
/// # Examples
///
/// ```ignore
/// // Prepare font, features, and optional fallbacks...
/// let mut font: FontKitFont = /* obtain font */ unimplemented!();
/// let features: FontFeatures = /* build features */ unimplemented!();
/// let fallbacks: Option<FontFallbacks> = None;
///
/// apply_features_and_fallbacks(&mut font, &features, fallbacks)?;
/// ```
pub fn apply_features_and_fallbacks(
    font: &mut FontKitFont,
    features: &FontFeatures,
    fallbacks: Option<&FontFallbacks>,
) -> anyhow::Result<()> {
    unsafe {
        let mut keys = vec![kCTFontFeatureSettingsAttribute];
        let mut values = vec![generate_feature_array(features)];
        if let Some(fallbacks) = fallbacks {
            if !fallbacks.fallback_list().is_empty() {
                keys.push(kCTFontCascadeListAttribute);
                values.push(generate_fallback_array(
                    fallbacks,
                    font.native_font().as_concrete_TypeRef(),
                ));
            }
        }
        let attrs = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr() as _,
            values.as_ptr() as _,
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        let new_descriptor = CTFontDescriptorCreateWithAttributes(attrs);
        CFRelease(attrs as _);
        let new_descriptor = CTFontDescriptor::wrap_under_create_rule(new_descriptor);
        let new_font = CTFontCreateCopyWithAttributes(
            font.native_font().as_concrete_TypeRef(),
            0.0,
            std::ptr::null(),
            new_descriptor.as_concrete_TypeRef(),
        );
        let new_font = CTFont::wrap_under_create_rule(new_font);
        *font = font_kit::font::Font::from_native_font(&new_font);

        Ok(())
    }
}

/// Build a Core Foundation array of OpenType feature dictionaries from the provided features.
///
/// Each element is a CFDictionary with `kCTFontOpenTypeFeatureTag` (feature tag string)
/// and `kCTFontOpenTypeFeatureValue` (feature value number).
///
/// # Examples
///
/// ```
/// // Construct features (API shown illustratively; actual constructor may differ).
/// let mut features = FontFeatures::new();
/// features.add("liga", 1); // enable standard ligatures
/// let array = generate_feature_array(&features);
/// assert!(!array.is_null());
/// ```
—
fn generate_feature_array(features: &FontFeatures) -> CFMutableArrayRef {
    unsafe {
        let feature_array = CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);
        for (tag, value) in features.tag_value_list() {
            let keys = [kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue];
            let values = [
                CFString::new(tag).as_CFTypeRef(),
                CFNumber::from(*value as i32).as_CFTypeRef(),
            ];
            let dict = CFDictionaryCreate(
                kCFAllocatorDefault,
                &keys as *const _ as _,
                &values as *const _ as _,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            values.into_iter().for_each(|value| CFRelease(value));
            CFArrayAppendValue(feature_array, dict as _);
            CFRelease(dict as _);
        }
        feature_array
    }
}

/// Builds a mutable Core Foundation array of CTFontDescriptor fallbacks from the provided
/// user fallback list and the system default cascade list for the given font.
///
/// This function appends a CTFontDescriptor for each user-supplied fallback name and then
/// augments the array with system-provided cascade descriptors derived from `font_ref`.
///
/// # Parameters
/// - `fallbacks`: user-provided font fallback collection whose names will be converted to descriptors.
/// - `font_ref`: a `CTFontRef` used to query the system default cascade list for additional descriptors.
///
/// # Returns
/// A `CFMutableArrayRef` containing `CTFontDescriptor` references suitable for use as a cascade/fallback list.
///
/// # Examples
///
/// ```no_run
/// // `fallbacks` and `font_ref` are assumed to be available in the surrounding scope.
/// let fallback_array = unsafe { generate_fallback_array(&fallbacks, font_ref) };
/// assert!(!fallback_array.is_null());
/// ```
fn generate_fallback_array(fallbacks: &FontFallbacks, font_ref: CTFontRef) -> CFMutableArrayRef {
    unsafe {
        let fallback_array = CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);
        for user_fallback in fallbacks.fallback_list() {
            let name = CFString::from(user_fallback.as_str());
            let fallback_desc =
                CTFontDescriptorCreateWithNameAndSize(name.as_concrete_TypeRef(), 0.0);
            CFArrayAppendValue(fallback_array, fallback_desc as _);
            CFRelease(fallback_desc as _);
        }
        append_system_fallbacks(fallback_array, font_ref);
        fallback_array
    }
}

/// Appends the system default font cascade descriptors for the given font to `fallback_array`.
///
/// This queries the user's preferred languages and obtains Core Text's default cascade list for
/// `font_ref`, then appends each descriptor that contains a valid font path to `fallback_array`.
///
/// # Examples
///
/// ```
/// // Unsafe: Core Foundation / Core Text APIs require unsafe context in this crate.
/// unsafe {
///     // `fallback_array` is a `CFMutableArrayRef` previously created (e.g., empty mutable array).
///     // `font` is a `CTFont` obtained elsewhere.
///     append_system_fallbacks(fallback_array, font.as_concrete_TypeRef());
/// }
/// ```
fn append_system_fallbacks(fallback_array: CFMutableArrayRef, font_ref: CTFontRef) {
    unsafe {
        let preferred_languages: CFArray<CFString> =
            CFArray::wrap_under_create_rule(CFLocaleCopyPreferredLanguages());

        let default_fallbacks = CTFontCopyDefaultCascadeListForLanguages(
            font_ref,
            preferred_languages.as_concrete_TypeRef(),
        );
        let default_fallbacks: CFArray<CTFontDescriptor> =
            CFArray::wrap_under_create_rule(default_fallbacks);

        default_fallbacks
            .iter()
            .filter(|desc| desc.font_path().is_some())
            .for_each(|desc| {
                CFArrayAppendValue(fallback_array, desc.as_concrete_TypeRef() as _);
            });
    }
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontOpenTypeFeatureTag: CFStringRef;
    static kCTFontOpenTypeFeatureValue: CFStringRef;

    fn CTFontCreateCopyWithAttributes(
        font: CTFontRef,
        size: CGFloat,
        matrix: *const CGAffineTransform,
        attributes: CTFontDescriptorRef,
    ) -> CTFontRef;
    fn CTFontCopyDefaultCascadeListForLanguages(
        font: CTFontRef,
        languagePrefList: CFArrayRef,
    ) -> CFArrayRef;
}