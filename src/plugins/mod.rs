mod device;
mod music;
mod output;
mod system;
mod weather;
mod wol;

pub use device::DevicePlugin;
pub(crate) use music::{MUSIC_DIR, SUBTITLE_LANG_PRIORITY};
pub use music::MusicPlugin;
pub use output::OutputPlugin;
pub use system::SystemPlugin;
pub use weather::WeatherPlugin;
pub use wol::WolPlugin;
