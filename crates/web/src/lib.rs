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
        start(bootstrap::LaunchMode::Menu);
    }

    /// Start straight on a map (for example `mp_rust`).
    #[wasm_bindgen]
    pub fn run_map(map: &str) {
        start(bootstrap::LaunchMode::Map(map.to_owned()));
    }

    /// A WebRTC link to player `member` finished its handshake.
    #[wasm_bindgen]
    pub fn net_open(member: u32) {
        net::web_link::on_open(member);
    }

    /// The link to player `member` is gone.
    #[wasm_bindgen]
    pub fn net_close(member: u32) {
        net::web_link::on_close(member);
    }

    /// A message arrived from player `member`. Lanes: 0 control, 1 data, 2 bulk.
    #[wasm_bindgen]
    pub fn net_message(member: u32, lane: u8, bytes: Vec<u8>) {
        if let Some(lane) = net::web_link::Lane::from_u8(lane) {
            net::web_link::on_message(member, lane, bytes);
        }
    }

    /// Step the relay. Returns what must be sent, packed as repeated
    /// `[member u32 LE][lane u8][len u32 LE][bytes]`.
    #[wasm_bindgen]
    pub fn net_tick() -> Vec<u8> {
        let mut packed = Vec::new();
        for (member, lane, bytes) in net::web_link::tick() {
            packed.extend_from_slice(&member.to_le_bytes());
            packed.push(lane as u8);
            packed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            packed.extend_from_slice(&bytes);
        }
        packed
    }

    fn start(mode: bootstrap::LaunchMode) {
        console_error_panic_hook::set_once();
        diag::set_console_sink(|line| web_sys::console::log_1(&line.into()));
        let artifacts = PathBuf::from("/iw4l-artifacts");
        let _ = vfs::create_dir_all(&artifacts);
        diag::init_log(&artifacts);
        bootstrap::launch(
            asset_transport::GamesRoot(PathBuf::from(GAMES)),
            artifacts,
            mode,
            None,
            sim::HostCheats::default(),
        );
    }
}
