use anyhow::{Context as _, Result};
use tauri::{Cef, Manager as _, WindowEvent};

use crate::commands::{
    agent::AgentState,
    canvas::CanvasChannel,
    lifecycle::{DownloadChannel, ProjectChannel, ResourceChannel},
    processing::JobChannel,
};

pub fn run(context: tauri::Context<Cef>) -> Result<()> {
    let builder = tauri::Builder::<Cef>::default()
        .command_line_args::<_, &str>([("--hide-chrome-bubbles", None)]);
    #[cfg(debug_assertions)]
    let builder = builder.command_line_args([
        ("remote-debugging-port", Some("4000")),
        ("--use-mock-keychain", None),
    ]);
    #[cfg(target_os = "linux")]
    let builder = builder.command_line_args([
        ("enable-unsafe-webgpu", None),
        ("enable-features", Some("Vulkan,VulkanFromANGLE")),
        ("use-angle", Some("vulkan")),
    ]);
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .clear_targets()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: None },
                ))
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|handle, _, _| {
            if let Some(window) = handle.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED
                        | tauri_plugin_window_state::StateFlags::FULLSCREEN,
                )
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(crate::commands::bindings().invoke_handler())
        .setup(move |application| {
            #[cfg(target_os = "windows")]
            koharu_runtime::Store::configure(
                application
                    .path()
                    .resource_dir()
                    .context("failed to locate Koharu's installation directory")?
                    .join("store"),
            )?;

            // Transport-side channel slots for the desktop UI; the state
            // itself lives in the shared App aggregate.
            application.manage(CanvasChannel::default());
            application.manage(JobChannel::default());
            application.manage(DownloadChannel::default());
            application.manage(ResourceChannel::default());
            application.manage(ProjectChannel::default());

            let handle = application.handle().clone();
            let app = std::sync::Arc::new(crate::core::App::new()?);
            application.manage(AgentState::new(app.clone())?);
            application.manage(app.clone());

            // Forward core events into the desktop channel slots that the
            // `subscribe` command registers; headless serves the same stream
            // through SSE instead of duplicating producers.
            drop(tauri::async_runtime::spawn(
                crate::commands::forward_events(app.clone(), handle.clone()),
            ));

            let window_config = application
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .context("the main Tauri window configuration is unavailable")?;
            let window = tauri::WebviewWindowBuilder::from_config(application, window_config)?
                .build()
                .context("failed to create the main window")?;
            window.show().context("failed to show the main window")?;
            window
                .set_focus()
                .context("failed to focus the main window")?;
            let initialization_handle = app.clone();
            drop(tauri::async_runtime::spawn(async move {
                initialization_handle
                    .initialize()
                    .await
                    .expect("failed to initialize the desktop runtime");
                initialization_handle.mark_ready();
            }));

            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(
                event,
                WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed
            ) {
                let app = window.state::<std::sync::Arc<crate::core::App>>();
                app.cancel_processing();
                window.state::<AgentState>().cancel_all();
            }
            if matches!(event, WindowEvent::Destroyed) {
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "app_closed",
                    phase = "shutdown",
                );
                koharu_metrics::shutdown();
            }
        })
        .run(context)?;
    Ok(())
}
