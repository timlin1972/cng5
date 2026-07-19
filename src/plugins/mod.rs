mod device;
mod music;
mod notepad;
mod output;
mod system;
mod weather;
mod wol;

pub use device::DevicePlugin;
pub(crate) use music::{MUSIC_DIR, SUBTITLE_LANG_PRIORITY};
pub use music::MusicPlugin;
pub(crate) use notepad::{DEFAULT_NOTEPAD_FILE, NOTEPAD_DIR};
pub use notepad::NotepadPlugin;
pub use output::OutputPlugin;
pub use system::SystemPlugin;
pub use weather::WeatherPlugin;
pub use wol::WolPlugin;
