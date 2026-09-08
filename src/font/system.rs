use crate::{Attrs, Font, FontMatchAttrs, HashMap, ShapeBuffer};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, DerefMut};
use fontdb::{FaceInfo, Query, Style};
use skrifa::raw::{ReadError, TableProvider as _};
use skrifa::MetadataProvider;

// re-export fontdb and harfrust
pub use fontdb;
pub use harfrust;

use super::fallback::{Fallback, Fallbacks, MonospaceFallbackInfo, PlatformFallback};

// The fields are used in the derived Ord implementation for sorting fallback candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FontMatchKey {
    pub(crate) not_emoji: bool,
    pub(crate) font_weight_diff: u16,
    pub(crate) font_stretch_diff: u16,
    pub(crate) font_style_diff: u8,
    pub(crate) font_weight: u16,
    pub(crate) font_stretch: u16,
    pub(crate) id: fontdb::ID,
    pub(crate) variable_weight_match: bool,
}

impl FontMatchKey {
    fn new(attrs: &Attrs, face: &FaceInfo, db: &fontdb::Database) -> FontMatchKey {
        // TODO: smarter way of detecting emoji
        let not_emoji = !face.post_script_name.contains("Emoji");
        let font_weight_diff = attrs.weight.0.abs_diff(face.weight.0);

        let variable_weight_match = font_weight_diff != 0
            && db.with_face_data(face.id, |font_data, face_index| {
                let font_ref = skrifa::FontRef::from_index(font_data, face_index).ok()?;
                let axis = font_ref.axes().get_by_tag(skrifa::Tag::new(b"wght"))?;
                let w = attrs.weight.0 as f32;
                Some(w >= axis.min_value() && w <= axis.max_value())
            }) == Some(Some(true));
        let font_weight = face.weight.0;
        let font_stretch_diff = attrs.stretch.to_number().abs_diff(face.stretch.to_number());
        let font_stretch = face.stretch.to_number();
        let font_style_diff = match (attrs.style, face.style) {
            (Style::Normal, Style::Normal)
            | (Style::Italic, Style::Italic)
            | (Style::Oblique, Style::Oblique) => 0,
            (Style::Italic, Style::Oblique) | (Style::Oblique, Style::Italic) => 1,
            (Style::Normal, Style::Italic)
            | (Style::Normal, Style::Oblique)
            | (Style::Italic, Style::Normal)
            | (Style::Oblique, Style::Normal) => 2,
        };
        let id = face.id;
        FontMatchKey {
            not_emoji,
            font_weight_diff,
            font_stretch_diff,
            font_style_diff,
            font_weight,
            font_stretch,
            id,
            variable_weight_match,
        }
    }
}

struct FontCachedCodepointSupportInfo {
    supported: Vec<u32>,
    not_supported: Vec<u32>,
}

impl FontCachedCodepointSupportInfo {
    const SUPPORTED_MAX_SZ: usize = 512;
    const NOT_SUPPORTED_MAX_SZ: usize = 1024;

    fn new() -> Self {
        Self {
            supported: Vec::with_capacity(Self::SUPPORTED_MAX_SZ),
            not_supported: Vec::with_capacity(Self::NOT_SUPPORTED_MAX_SZ),
        }
    }

    #[inline(always)]
    fn unknown_has_codepoint(
        &mut self,
        font_codepoints: &[u32],
        codepoint: u32,
        supported_insert_pos: usize,
        not_supported_insert_pos: usize,
    ) -> bool {
        let ret = font_codepoints.contains(&codepoint);
        if ret {
            // don't bother inserting if we are going to truncate the entry away
            if supported_insert_pos != Self::SUPPORTED_MAX_SZ {
                self.supported.insert(supported_insert_pos, codepoint);
                self.supported.truncate(Self::SUPPORTED_MAX_SZ);
            }
        } else {
            // don't bother inserting if we are going to truncate the entry away
            if not_supported_insert_pos != Self::NOT_SUPPORTED_MAX_SZ {
                self.not_supported
                    .insert(not_supported_insert_pos, codepoint);
                self.not_supported.truncate(Self::NOT_SUPPORTED_MAX_SZ);
            }
        }
        ret
    }

    #[inline(always)]
    fn has_codepoint(&mut self, font_codepoints: &[u32], codepoint: u32) -> bool {
        match self.supported.binary_search(&codepoint) {
            Ok(_) => true,
            Err(supported_insert_pos) => match self.not_supported.binary_search(&codepoint) {
                Ok(_) => false,
                Err(not_supported_insert_pos) => self.unknown_has_codepoint(
                    font_codepoints,
                    codepoint,
                    supported_insert_pos,
                    not_supported_insert_pos,
                ),
            },
        }
    }
}

/// Access to the system fonts.
pub struct FontSystem {
    /// The locale of the system.
    locale: String,

    /// The underlying font database.
    db: fontdb::Database,

    /// Cache for loaded fonts from the database.
    font_cache: HashMap<(fontdb::ID, fontdb::Weight), Option<Arc<Font>>>,

    /// Sorted unique ID's of all Monospace fonts in DB
    monospace_font_ids: Vec<fontdb::ID>,

    /// Sorted unique ID's of all Monospace fonts in DB per script.
    /// A font may support multiple scripts of course, so the same ID
    /// may appear in multiple map value vecs.
    per_script_monospace_font_ids: HashMap<[u8; 4], Vec<fontdb::ID>>,

    /// Cache for font codepoint support info
    font_codepoint_support_info_cache: HashMap<fontdb::ID, FontCachedCodepointSupportInfo>,

    /// Cache for font matches.
    font_matches_cache: HashMap<FontMatchAttrs, Arc<Vec<FontMatchKey>>>,

    /// Scratch buffer for shaping and laying out.
    pub(crate) shape_buffer: ShapeBuffer,

    /// Buffer for use in `FontFallbackIter`.
    pub(crate) monospace_fallbacks_buffer: BTreeSet<MonospaceFallbackInfo>,

    /// Cache for shaped runs
    #[cfg(feature = "shape-run-cache")]
    pub shape_run_cache: crate::ShapeRunCache,

    /// List of fallbacks
    pub(crate) dyn_fallback: Box<dyn Fallback>,

    /// List of fallbacks
    pub(crate) fallbacks: Fallbacks,
}

impl fmt::Debug for FontSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontSystem")
            .field("locale", &self.locale)
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

/// The persistent on-disk system-font index cache used when loading system fonts.
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Default)]
enum SystemFontsCache {
    /// Scan the system font directories on every start.
    #[default]
    Disabled,
    /// Use the cache at [`FontSystem::default_cache_path`].
    DefaultPath,
    /// Use the cache at an explicitly provided path.
    Path(std::path::PathBuf),
}

/// A builder for [`FontSystem`] with the following default configuration:
///
/// * Use the system locale on `std` or `en-US` on `no_std`
/// * Load system fonts, without a persistent on-disk cache
/// * Use `Noto Sans Mono` as the monospace family
/// * Use `Open Sans` as the sans-serif family
/// * Use `DejaVu Serif` as the serif family
/// * Use a platform-specific font fallback
///
/// Passing a database to [`FontSystemBuilder::database`] changes those defaults: the database is
/// then used as-is, so system fonts are not loaded into it and the font families it already has
/// configured are left alone. Both can still be requested explicitly with
/// [`FontSystemBuilder::load_system_fonts`] and the family setters.
///
/// # Timing
///
/// When system fonts are loaded (the default when no database is provided) building takes some
/// time. On the release build, it can take up to a second, while debug builds can take up to ten
/// times longer. For this reason, it should only be built once, and the resulting [`FontSystem`]
/// should be shared.
///
/// # Examples
///
/// Basic usage:
///
/// ```no_run
/// # macro_rules! include_bytes { ($e:expr) => {[]} }
/// use std::sync::Arc;
/// use cosmic_text::FontSystem;
/// use cosmic_text::fontdb::Source;
///
/// let font = Source::Binary(Arc::new(include_bytes!("Roboto.ttf")));
/// FontSystem::builder()
///     .locale(Some("fr-FR"))
///     .load_font(font)
///     .build();
/// ```
pub struct FontSystemBuilder {
    locale: Option<String>,
    #[cfg(feature = "std")]
    load_system_fonts: Option<bool>,
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    system_fonts_cache: SystemFontsCache,
    database: Option<fontdb::Database>,
    fonts: Vec<fontdb::Source>,
    monospace_family: Option<String>,
    sans_serif_family: Option<String>,
    serif_family: Option<String>,
    dyn_fallback: Option<Box<dyn Fallback>>,
}

impl fmt::Debug for FontSystemBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("FontSystemBuilder");
        s.field("locale", &self.locale);
        #[cfg(feature = "std")]
        s.field("load_system_fonts", &self.load_system_fonts);
        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        s.field("system_fonts_cache", &self.system_fonts_cache);
        s.field("database", &self.database)
            .field("fonts", &self.fonts)
            .field("monospace_family", &self.monospace_family)
            .field("sans_serif_family", &self.sans_serif_family)
            .field("serif_family", &self.serif_family)
            .finish_non_exhaustive()
    }
}

impl Default for FontSystemBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FontSystemBuilder {
    /// Family used by `Family::Monospace` unless another one is configured.
    const DEFAULT_MONOSPACE_FAMILY: &'static str = "Noto Sans Mono";

    /// Family used by `Family::SansSerif` unless another one is configured.
    const DEFAULT_SANS_SERIF_FAMILY: &'static str = "Open Sans";

    /// Family used by `Family::Serif` unless another one is configured.
    const DEFAULT_SERIF_FAMILY: &'static str = "DejaVu Serif";

    fn new() -> Self {
        Self {
            locale: None,
            #[cfg(feature = "std")]
            load_system_fonts: None,
            #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
            system_fonts_cache: SystemFontsCache::Disabled,
            database: None,
            fonts: Vec::new(),
            monospace_family: None,
            sans_serif_family: None,
            serif_family: None,
            dyn_fallback: None,
        }
    }

    /// Consume the builder and create the [`FontSystem`]
    pub fn build(self) -> FontSystem {
        FontSystem::new_from_builder(self)
    }

    /// Specify the locale that will be used for font fallback or [`None`] for the default.
    ///
    /// Default: the system locale on `std` or `en-US` on `no_std`
    pub fn locale(mut self, value: Option<impl Into<String>>) -> Self {
        self.locale = value.map(Into::into);
        self
    }

    /// Enable loading all system fonts.
    ///
    /// Default: enabled, unless a database was provided with [`FontSystemBuilder::database`]
    #[cfg(feature = "std")]
    pub fn load_system_fonts(mut self, enabled: bool) -> Self {
        self.load_system_fonts = Some(enabled);
        self
    }

    /// Load the system fonts through a persistent on-disk index cache stored at the default
    /// platform location (see [`FontSystem::default_cache_path`]).
    ///
    /// On a cache hit no font file is parsed, which makes loading the system fonts considerably
    /// faster. A stale or unreadable cache falls back to a normal scan and is then rewritten. If
    /// no cache directory can be determined the system fonts are loaded without a cache.
    ///
    /// This only affects system fonts, the sources added with [`FontSystemBuilder::load_font`] and
    /// [`FontSystemBuilder::load_fonts`] are always loaded fresh and never persisted to the cache.
    ///
    /// Calling this replaces any path previously set with
    /// [`FontSystemBuilder::system_fonts_cache_path`].
    ///
    /// Default: disabled
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn system_fonts_cache(mut self, enabled: bool) -> Self {
        self.system_fonts_cache = if enabled {
            SystemFontsCache::DefaultPath
        } else {
            SystemFontsCache::Disabled
        };
        self
    }

    /// Like [`FontSystemBuilder::system_fonts_cache`], but stores the persistent system-font index
    /// cache at the provided path instead of the default platform location.
    ///
    /// Calling this enables the cache.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn system_fonts_cache_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.system_fonts_cache = SystemFontsCache::Path(path.into());
        self
    }

    /// Use the specified font database instead of a new one.
    ///
    /// The database is used as-is: system fonts are not loaded into it and the font families it
    /// has configured are kept, unless [`FontSystemBuilder::load_system_fonts`] or the family
    /// setters are called explicitly.
    pub fn database(mut self, value: Option<fontdb::Database>) -> Self {
        self.database = value;
        self
    }

    /// Load an additional font
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # macro_rules! include_bytes { ($e:expr) => {[]} }
    /// use std::sync::Arc;
    /// use cosmic_text::FontSystem;
    /// use cosmic_text::fontdb::Source;
    ///
    /// FontSystem::builder()
    ///     .load_font(Source::Binary(Arc::new(include_bytes!("Roboto-Regular.ttf"))))
    ///     .load_font(Source::File("./Roboto-Bold.ttf".into()))
    ///     .build();
    /// ```
    pub fn load_font(mut self, source: fontdb::Source) -> Self {
        self.fonts.push(source);
        self
    }

    /// Load additional fonts
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # macro_rules! include_bytes { ($e:expr) => {[]} }
    /// use std::sync::Arc;
    /// use cosmic_text::FontSystem;
    /// use cosmic_text::fontdb::Source;
    ///
    /// FontSystem::builder()
    ///     .load_fonts([
    ///         Source::Binary(Arc::new(include_bytes!("Roboto-Regular.ttf"))),
    ///         Source::File("./Roboto-Bold.ttf".into())
    ///     ])
    ///     .build();
    /// ```
    pub fn load_fonts(mut self, sources: impl IntoIterator<Item = fontdb::Source>) -> Self {
        self.fonts.extend(sources);
        self
    }

    /// Sets the family that will be used by `Family::Monospace`.
    ///
    /// Default: `Noto Sans Mono`, or the database's own setting when one was provided with
    /// [`FontSystemBuilder::database`]
    pub fn monospace_family(mut self, value: impl Into<String>) -> Self {
        self.monospace_family = Some(value.into());
        self
    }

    /// Sets the family that will be used by `Family::SansSerif`.
    ///
    /// Default: `Open Sans`, or the database's own setting when one was provided with
    /// [`FontSystemBuilder::database`]
    pub fn sans_serif_family(mut self, value: impl Into<String>) -> Self {
        self.sans_serif_family = Some(value.into());
        self
    }

    /// Sets the family that will be used by `Family::Serif`.
    ///
    /// Default: `DejaVu Serif`, or the database's own setting when one was provided with
    /// [`FontSystemBuilder::database`]
    pub fn serif_family(mut self, value: impl Into<String>) -> Self {
        self.serif_family = Some(value.into());
        self
    }

    /// Sets the font fallback implementation
    ///
    /// Default: [`PlatformFallback`]
    pub fn dyn_fallback(mut self, fallback: Option<Box<dyn Fallback>>) -> Self {
        self.dyn_fallback = fallback;
        self
    }

    /// Sets the font fallback implementation
    ///
    /// Default: [`PlatformFallback`]
    pub fn fallback(self, fallback: impl Fallback + 'static) -> Self {
        self.dyn_fallback(Some(Box::new(fallback)))
    }
}

impl FontSystem {
    const FONT_MATCHES_CACHE_SIZE_LIMIT: usize = 256;
    /// Create a new [`FontSystem`], that allows access to any installed system fonts
    ///
    /// # Timing
    ///
    /// This function takes some time to run. On the release build, it can take up to a second,
    /// while debug builds can take up to ten times longer. For this reason, it should only be
    /// called once, and the resulting [`FontSystem`] should be shared.
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Create a new [`FontSystem`] with a pre-specified set of fonts.
    pub fn new_with_fonts(fonts: impl IntoIterator<Item = fontdb::Source>) -> Self {
        Self::builder().load_fonts(fonts).build()
    }

    /// Create a new [`FontSystem`] backed by a persistent on-disk system-font index cache
    /// at the platform's conventional cache location
    /// (e.g. `$XDG_CACHE_HOME/cosmic-text/fonts.cache`).
    ///
    /// Falls back to a normal, uncached scan if no cache directory can be determined.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn new_cached() -> Self {
        Self::builder().system_fonts_cache(true).build()
    }

    /// Returns the default system-font cache file path used by [`FontSystem::new_cached`]
    /// and [`FontSystem::new_with_fonts_and_cache`]
    /// (`<cache-dir>/cosmic-text/fonts.cache`, following the platform's conventional cache
    /// directory).
    ///
    /// Returns `None` if no cache directory can be determined from the environment.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn default_cache_path() -> Option<std::path::PathBuf> {
        super::cache::default_cache_path()
    }

    /// Like [`FontSystem::new_with_fonts`], but reads from and writes to a persistent
    /// system-font index cache at the default platform cache location (see
    /// [`FontSystem::new_cached`]).
    ///
    /// The cache path is resolved automatically; if no cache directory can be determined
    /// this falls back to a normal, uncached scan.
    /// The user-provided `fonts` are always loaded fresh and
    /// are never persisted to the cache
    ///
    /// To cache at an explicit location instead, use
    /// [`FontSystem::new_with_fonts_and_cache_path`].
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn new_with_fonts_and_cache(fonts: impl IntoIterator<Item = fontdb::Source>) -> Self {
        Self::builder()
            .system_fonts_cache(true)
            .load_fonts(fonts)
            .build()
    }

    /// Like [`FontSystem::new_with_fonts_and_cache`], but uses the persistent system-font
    /// index cache at the explicitly provided `cache_path` rather than the default
    /// location.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn new_with_fonts_and_cache_path(
        fonts: impl IntoIterator<Item = fontdb::Source>,
        cache_path: std::path::PathBuf,
    ) -> Self {
        Self::builder()
            .system_fonts_cache_path(cache_path)
            .load_fonts(fonts)
            .build()
    }

    /// Create a builder for [`FontSystem`] with the following default configuration:
    ///
    /// * Use the system locale on `std` or `en-US` on `no_std`
    /// * Load system fonts, without a persistent on-disk cache
    /// * Use `Noto Sans Mono` as the monospace family
    /// * Use `Open Sans` as the sans-serif family
    /// * Use `DejaVu Serif` as the serif family
    /// * Use a platform-specific font fallback
    ///
    /// See [`FontSystemBuilder`] for the available options.
    ///
    /// # Timing
    ///
    /// When system fonts are loaded (the default when no database is provided) building takes
    /// some time. On the release build, it can take up to a second, while debug builds can take
    /// up to ten times longer. For this reason, it should only be built once, and the resulting
    /// [`FontSystem`] should be shared.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```no_run
    /// # macro_rules! include_bytes { ($e:expr) => {[]} }
    /// use std::sync::Arc;
    /// use cosmic_text::FontSystem;
    /// use cosmic_text::fontdb::Source;
    ///
    /// let font = Source::Binary(Arc::new(include_bytes!("Roboto.ttf")));
    /// FontSystem::builder()
    ///     .locale(Some("fr-FR"))
    ///     .load_font(font)
    ///     .build();
    /// ```
    pub fn builder() -> FontSystemBuilder {
        FontSystemBuilder::new()
    }

    /// Load the fonts described by the builder and finish constructing the [`FontSystem`].
    fn new_from_builder(builder: FontSystemBuilder) -> Self {
        // A database provided by the caller is used as-is: neither the system fonts nor the
        // default families are applied to it unless they were requested explicitly.
        let apply_defaults = builder.database.is_none();

        let locale = builder.locale.unwrap_or_else(Self::get_locale);
        log::debug!("Locale: {locale}");

        let mut db = builder.database.unwrap_or_default();

        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        let now = std::time::Instant::now();

        #[cfg(feature = "std")]
        if builder.load_system_fonts.unwrap_or(apply_defaults) {
            #[cfg(not(target_arch = "wasm32"))]
            Self::load_system_fonts(&mut db, &builder.system_fonts_cache);
            #[cfg(target_arch = "wasm32")]
            db.load_system_fonts();
        }

        for source in builder.fonts {
            db.load_font_source(source);
        }

        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        log::debug!(
            "Parsed {} font faces in {}ms.",
            db.len(),
            now.elapsed().as_millis()
        );

        if let Some(family) = builder.monospace_family.or_else(|| {
            apply_defaults.then(|| String::from(FontSystemBuilder::DEFAULT_MONOSPACE_FAMILY))
        }) {
            db.set_monospace_family(family);
        }
        if let Some(family) = builder.sans_serif_family.or_else(|| {
            apply_defaults.then(|| String::from(FontSystemBuilder::DEFAULT_SANS_SERIF_FAMILY))
        }) {
            db.set_sans_serif_family(family);
        }
        if let Some(family) = builder.serif_family.or_else(|| {
            apply_defaults.then(|| String::from(FontSystemBuilder::DEFAULT_SERIF_FAMILY))
        }) {
            db.set_serif_family(family);
        }

        let dyn_fallback = builder
            .dyn_fallback
            .unwrap_or_else(|| Box::new(PlatformFallback));

        Self::new_with_locale_and_db_and_dyn_fallback(locale, db, dyn_fallback)
    }

    fn new_with_locale_and_db_and_dyn_fallback(
        locale: String,
        db: fontdb::Database,
        dyn_fallback: Box<dyn Fallback>,
    ) -> Self {
        let mut monospace_font_ids = db
            .faces()
            .filter(|face_info| {
                face_info.monospaced && !face_info.post_script_name.contains("Emoji")
            })
            .map(|face_info| face_info.id)
            .collect::<Vec<_>>();
        monospace_font_ids.sort();

        let mut per_script_monospace_font_ids: HashMap<[u8; 4], BTreeSet<fontdb::ID>> =
            HashMap::default();

        if cfg!(feature = "monospace_fallback") {
            for &id in &monospace_font_ids {
                db.with_face_data(id, |font_data, face_index| {
                    let face = skrifa::FontRef::from_index(font_data, face_index)?;
                    for script in face
                        .gpos()?
                        .script_list()?
                        .script_records()
                        .iter()
                        .chain(face.gsub()?.script_list()?.script_records().iter())
                    {
                        per_script_monospace_font_ids
                            .entry(script.script_tag().into_bytes())
                            .or_default()
                            .insert(id);
                    }
                    Ok::<_, ReadError>(())
                });
            }
        }

        let per_script_monospace_font_ids = per_script_monospace_font_ids
            .into_iter()
            .map(|(k, v)| (k, Vec::from_iter(v)))
            .collect();

        let fallbacks = Fallbacks::new(&*dyn_fallback, &[], &locale);

        Self {
            locale,
            db,
            monospace_font_ids,
            per_script_monospace_font_ids,
            font_cache: HashMap::default(),
            font_matches_cache: HashMap::default(),
            font_codepoint_support_info_cache: HashMap::default(),
            monospace_fallbacks_buffer: BTreeSet::default(),
            #[cfg(feature = "shape-run-cache")]
            shape_run_cache: crate::ShapeRunCache::default(),
            shape_buffer: ShapeBuffer::default(),
            dyn_fallback,
            fallbacks,
        }
    }

    /// Create a new [`FontSystem`] with a pre-specified locale, font database and font fallback list.
    ///
    /// The database is used as-is: no system fonts are loaded into it and the font families it has
    /// configured are left untouched.
    pub fn new_with_locale_and_db_and_fallback(
        locale: String,
        db: fontdb::Database,
        impl_fallback: impl Fallback + 'static,
    ) -> Self {
        Self::builder()
            .locale(Some(locale))
            .database(Some(db))
            .fallback(impl_fallback)
            .build()
    }

    /// Create a new [`FontSystem`] with a pre-specified locale and font database.
    ///
    /// The database is used as-is: no system fonts are loaded into it and the font families it has
    /// configured are left untouched.
    pub fn new_with_locale_and_db(locale: String, db: fontdb::Database) -> Self {
        Self::builder()
            .locale(Some(locale))
            .database(Some(db))
            .build()
    }

    /// Get the locale.
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// Get the database.
    pub const fn db(&self) -> &fontdb::Database {
        &self.db
    }

    /// Get a mutable reference to the database.
    pub fn db_mut(&mut self) -> &mut fontdb::Database {
        self.font_matches_cache.clear();
        &mut self.db
    }

    /// Consume this [`FontSystem`] and return the locale and database.
    pub fn into_locale_and_db(self) -> (String, fontdb::Database) {
        (self.locale, self.db)
    }

    /// Get a font by its ID and weight.
    pub fn get_font(&mut self, id: fontdb::ID, weight: fontdb::Weight) -> Option<Arc<Font>> {
        self.font_cache
            .entry((id, weight))
            .or_insert_with(|| {
                #[cfg(feature = "std")]
                unsafe {
                    self.db.make_shared_face_data(id);
                }
                if let Some(font) = Font::new(&self.db, id, weight) {
                    Some(Arc::new(font))
                } else {
                    log::warn!(
                        "failed to load font '{}'",
                        self.db.face(id)?.post_script_name
                    );
                    None
                }
            })
            .clone()
    }

    pub fn is_monospace(&self, id: fontdb::ID) -> bool {
        self.monospace_font_ids.binary_search(&id).is_ok()
    }

    pub fn get_monospace_ids_for_scripts(
        &self,
        scripts: impl Iterator<Item = [u8; 4]>,
    ) -> Vec<fontdb::ID> {
        let mut ret = scripts
            .filter_map(|script| self.per_script_monospace_font_ids.get(&script))
            .flat_map(|ids| ids.iter().copied())
            .collect::<Vec<_>>();
        ret.sort();
        ret.dedup();
        ret
    }

    #[inline(always)]
    pub fn get_font_supported_codepoints_in_word(
        &mut self,
        id: fontdb::ID,
        weight: fontdb::Weight,
        word: &str,
    ) -> Option<usize> {
        self.get_font(id, weight).map(|font| {
            let code_points = font.unicode_codepoints();
            let cache = self
                .font_codepoint_support_info_cache
                .entry(id)
                .or_insert_with(FontCachedCodepointSupportInfo::new);
            word.chars()
                .filter(|ch| cache.has_codepoint(code_points, u32::from(*ch)))
                .count()
        })
    }

    pub fn get_font_matches(&mut self, attrs: &Attrs<'_>) -> Arc<Vec<FontMatchKey>> {
        // Clear the cache first if it reached the size limit
        if self.font_matches_cache.len() >= Self::FONT_MATCHES_CACHE_SIZE_LIMIT {
            log::trace!("clear font mache cache");
            self.font_matches_cache.clear();
        }

        self.font_matches_cache
            //TODO: do not create AttrsOwned unless entry does not already exist
            .entry(attrs.into())
            .or_insert_with(|| {
                #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
                let now = std::time::Instant::now();

                let mut font_match_keys = self
                    .db
                    .faces()
                    .map(|face| FontMatchKey::new(attrs, face, &self.db))
                    .collect::<Vec<_>>();

                // Sort so we get the keys with weight_offset=0 first
                font_match_keys.sort();

                // db.query is better than above, but returns just one font
                let query = Query {
                    families: &[attrs.family],
                    weight: attrs.weight,
                    stretch: attrs.stretch,
                    style: attrs.style,
                };

                if let Some(id) = self.db.query(&query) {
                    if let Some(i) = font_match_keys
                        .iter()
                        .enumerate()
                        .find(|(_i, key)| key.id == id)
                        .map(|(i, _)| i)
                    {
                        // if exists move to front
                        let match_key = font_match_keys.remove(i);
                        font_match_keys.insert(0, match_key);
                    } else if let Some(face) = self.db.face(id) {
                        // else insert in front
                        let match_key = FontMatchKey::new(attrs, face, &self.db);
                        font_match_keys.insert(0, match_key);
                    } else {
                        log::error!("Could not get face from db, that should've been there.");
                    }
                }

                #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
                {
                    let elapsed = now.elapsed();
                    log::debug!("font matches for {attrs:?} in {elapsed:?}");
                }

                Arc::new(font_match_keys)
            })
            .clone()
    }

    #[cfg(feature = "std")]
    fn get_locale() -> String {
        sys_locale::get_locale().unwrap_or_else(|| {
            log::warn!("failed to get system locale, falling back to en-US");
            String::from("en-US")
        })
    }

    #[cfg(not(feature = "std"))]
    fn get_locale() -> String {
        String::from("en-US")
    }

    /// Load the system fonts into `db`, going through the persistent on-disk index cache when
    /// one is configured.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    fn load_system_fonts(db: &mut fontdb::Database, cache: &SystemFontsCache) {
        match cache {
            SystemFontsCache::Disabled => db.load_system_fonts(),
            SystemFontsCache::DefaultPath => match super::cache::default_cache_path() {
                Some(cache_path) => super::cache::load_system_fonts_cached(db, &cache_path),
                None => {
                    log::warn!(
                        "failed to determine a font cache path, loading system fonts uncached"
                    );
                    db.load_system_fonts();
                }
            },
            SystemFontsCache::Path(cache_path) => {
                super::cache::load_system_fonts_cached(db, cache_path);
            }
        }
    }
}

/// A value borrowed together with an [`FontSystem`]
#[derive(Debug)]
pub struct BorrowedWithFontSystem<'a, T> {
    pub(crate) inner: &'a mut T,
    pub(crate) font_system: &'a mut FontSystem,
}

impl<T> Deref for BorrowedWithFontSystem<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<T> DerefMut for BorrowedWithFontSystem<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::{fontdb, FontSystem};
    use fontdb::Family;

    #[test]
    fn builder_applies_the_default_families() {
        let font_system = FontSystem::builder()
            .load_system_fonts(false)
            .locale(Some("fr-FR"))
            .build();

        assert_eq!(font_system.locale(), "fr-FR");

        let db = font_system.db();
        assert_eq!(db.family_name(&Family::Monospace), "Noto Sans Mono");
        assert_eq!(db.family_name(&Family::SansSerif), "Open Sans");
        assert_eq!(db.family_name(&Family::Serif), "DejaVu Serif");
    }

    #[test]
    fn builder_overrides_the_default_families() {
        let font_system = FontSystem::builder()
            .load_system_fonts(false)
            .monospace_family("Custom Mono")
            .sans_serif_family("Custom Sans")
            .serif_family("Custom Serif")
            .build();

        let db = font_system.db();
        assert_eq!(db.family_name(&Family::Monospace), "Custom Mono");
        assert_eq!(db.family_name(&Family::SansSerif), "Custom Sans");
        assert_eq!(db.family_name(&Family::Serif), "Custom Serif");
    }

    /// A database provided by the caller is used as-is: no system fonts are loaded into it and
    /// the families it has configured are kept.
    #[test]
    fn a_provided_database_is_used_as_is() {
        let mut db = fontdb::Database::new();
        db.set_monospace_family("Provided Mono");

        let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let db = font_system.db();

        assert_eq!(font_system.locale(), "en-US");
        assert_eq!(db.len(), 0);
        assert_eq!(db.family_name(&Family::Monospace), "Provided Mono");
        // fontdb's own default, not the one the builder applies to a database it creates.
        assert_eq!(db.family_name(&Family::Serif), "Times New Roman");
    }

    /// The defaults a provided database opts out of can still be requested one by one.
    #[test]
    fn a_provided_database_can_still_be_configured() {
        let mut db = fontdb::Database::new();
        db.set_monospace_family("Provided Mono");

        let font_system = FontSystem::builder()
            .database(Some(db))
            .monospace_family("Custom Mono")
            .build();

        assert_eq!(
            font_system.db().family_name(&Family::Monospace),
            "Custom Mono"
        );
    }
}
