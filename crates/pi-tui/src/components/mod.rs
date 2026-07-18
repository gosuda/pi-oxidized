//! Built-in product-agnostic TUI components.

pub mod editor;

mod image;
mod input;
mod loader;
mod markdown;
mod padded;
mod select_list;
mod settings_list;
mod spacer;
mod text;
mod truncated_text;

pub mod util;

pub use image::{ImageComponent, ImageOptions, ImageTheme};
pub use input::Input;
pub use loader::{CancellableLoader, DEFAULT_LOADER_FRAMES, Loader, LoaderIndicatorOptions};
pub use markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
pub use padded::Padded;
pub use select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
    SelectListTruncatePrimaryContext,
};
pub use settings_list::{SettingItem, SettingsList, SettingsListOptions, SettingsListTheme};
pub use spacer::Spacer;
pub use text::Text;
pub use truncated_text::TruncatedText;
