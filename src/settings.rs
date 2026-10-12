//! Constants mirrored from the upstream `app/settings.py`.

pub const DEFAULT_FONT: &str = "impact";
pub const MINIMUM_FONT_SIZE: u32 = 7;

pub const ALLOWED_EXTENSIONS: &[&str] = &["gif", "jpg", "jpeg", "mp4", "png", "webp"];
pub const DEFAULT_STATIC_EXTENSION: &str = "png";
pub const DEFAULT_ANIMATED_EXTENSION: &str = "gif";

pub const DEFAULT_SIZE: (u32, u32) = (600, 600);

/// Example value API doc UIs fill in for strings; treated as "not provided".
pub const PLACEHOLDER: &str = "string";

pub const MAX_SLUG_PART_BYTES: usize = 200;

/// Template ID for a meme on a background of the request's own, without a
/// template.
pub const CUSTOM_TEMPLATE: &str = "custom";

/// Limits on a request's own background: its file size, and its pixels
/// (summed over a GIF's frames), which bound the memory decoding takes.
pub const MAX_BACKGROUND_BYTES: usize = 10 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_BACKGROUND_PIXELS: u64 = 100_000_000;
#[cfg(target_arch = "wasm32")]
pub const MAX_BACKGROUND_PIXELS: u64 = 8_000_000;
/// How much longer one side of a background can be than the other.
pub const MAX_BACKGROUND_RATIO: u32 = 5;
