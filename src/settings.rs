//! Constants mirrored from the upstream `app/settings.py`.

pub const DEFAULT_FONT: &str = "impact";
pub const MINIMUM_FONT_SIZE: u32 = 7;

pub const ALLOWED_EXTENSIONS: &[&str] = &["gif", "jpg", "jpeg", "png", "webp"];
pub const DEFAULT_STATIC_EXTENSION: &str = "png";
pub const DEFAULT_ANIMATED_EXTENSION: &str = "gif";

pub const DEFAULT_SIZE: (u32, u32) = (600, 600);

/// Swagger UI placeholder value; treated as "not provided".
pub const PLACEHOLDER: &str = "string";

pub const MAX_SLUG_PART_BYTES: usize = 200;
