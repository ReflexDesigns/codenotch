//! Self-update against the GitHub release feed.
//! The Release workflow publishes latest.json next to the installer; the updater reads it, checks the
//! minisign signature against the pubkey in tauri.conf.json and runs the NSIS installer, which relaunches us.
//! Anything that fails here is logged and retried at the next tick — a broken update must never keep the notch off screen.

use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

const FIRST_CHECK_SECS: u64 = 60; // let the notch draw and the pollers settle before touching the network
const CHECK_EVERY_SECS: u64 = 6 * 60 * 60;

async fn check_once(app: AppHandle) -> tauri_plugin_updater::Result<()> {
    let Some(update) = app.updater()?.check().await? else {
        return Ok(());
    };
    crate::applog(&format!("update: {} available, installing", update.version));
    update.download_and_install(|_, _| {}, || {}).await?;
    // Windows never reaches this: the NSIS installer takes over and relaunches the app itself
    app.restart()
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(FIRST_CHECK_SECS));
        loop {
            if let Err(e) = tauri::async_runtime::block_on(check_once(app.clone())) {
                crate::applog(&format!("update: check failed: {e}"));
            }
            std::thread::sleep(Duration::from_secs(CHECK_EVERY_SECS));
        }
    });
}
