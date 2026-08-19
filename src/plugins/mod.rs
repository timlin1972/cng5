mod activities;
mod clock;
mod device;
mod ereader;
mod gitrepo;
mod global;
mod music;
mod notepad;
mod output;
mod qr;
mod remote;
mod remote_output;
mod storage;
mod sync;
mod sync_baseline;
mod system;
mod table_diff;
mod todo;
mod wallpaper;
mod weather;
mod wol;
mod worldclock;

pub use activities::ActivitiesPlugin;
pub use clock::ClockPlugin;
pub use device::DevicePlugin;
pub(crate) use ereader::{
    book_cover, book_meta, book_resource, chapter_is_vertical, haodoo_import, inject_pagination_style, list_books,
    normalize_vertical_css, safe_ebook_path, save_chapter_progress,
};
pub use ereader::EReaderPlugin;
pub(crate) use storage::{
    list_dir, make_dir, paginate_sync_entries, read_chunk, remove, remove_conflict_files, rename_path,
    safe_storage_path, walk_with_hashes, write_chunk, STORAGE_DIR,
};
pub(crate) use storage::SyncEntry;
pub use gitrepo::GitRepoPlugin;
pub use global::GlobalPlugin;
pub(crate) use music::{
    load_favorites, remove_favorite, safe_music_copy_path, toggle_favorite, url_encode_filename, MUSIC_DIR,
    SUBTITLE_LANG_PRIORITY,
};
pub use music::MusicPlugin;
pub(crate) use notepad::{DEFAULT_NOTEPAD_FILE, NOTEPAD_DIR};
pub use notepad::NotepadPlugin;
pub use output::OutputPlugin;
pub use qr::QrPlugin;
pub use remote::RemotePlugin;
pub use remote_output::RemoteOutputPlugin;
pub use storage::StoragePlugin;
pub use sync::SyncPlugin;
pub(crate) use system::REPORT_INTERVAL;
pub use system::SystemPlugin;
pub(crate) use table_diff::{RowDiffTracker, TableSnapshot};
pub use todo::TodoPlugin;
pub(crate) use wallpaper::{current_wallpaper, list_wallpapers, rotate_enabled, select_wallpaper, set_rotate_enabled};
pub use wallpaper::WallpaperPlugin;
pub use weather::WeatherPlugin;
pub use wol::WolPlugin;
pub use worldclock::WorldClockPlugin;
