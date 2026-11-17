//! The browser build. The page downloads IW4PK shards, hands them to
//! [`mount_shard`], then calls [`run`] once.

#[cfg(target_arch = "wasm32")]
mod entry {
    use std::path::PathBuf;

    use wasm_bindgen::prelude::*;

    const GAMES: &str = "/games";

    /// Mount one shard under `/games/<game_dir>`. Returns the number of files.
    #[wasm_bindgen]
    pub fn mount_shard(game_dir: &str, bytes: Vec<u8>) -> Result<u32, JsValue> {
        let root = format!("{GAMES}/{game_dir}");
        vfs::mount_shard(root, bytes)
            .map(|n| n as u32)
            .map_err(|e| JsValue::from_str(&e))
    }

    /// Start the main menu on the canvas `#iw4l-canvas`.
    #[wasm_bindgen]
    pub fn run() {
        console_error_panic_hook::set_once();
        diag::set_console_sink(|line| web_sys::console::log_1(&line.into()));
        let artifacts = PathBuf::from("/iw4l-artifacts");
        let _ = vfs::create_dir_all(&artifacts);
        diag::init_log(&artifacts);
        bootstrap::launch(
            asset_transport::GamesRoot(PathBuf::from(GAMES)),
            artifacts,
            bootstrap::LaunchMode::Menu,
            None,
            sim::HostCheats::default(),
        );
    }
}
