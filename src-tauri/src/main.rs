// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{collections::HashMap, sync::Arc, path::PathBuf, time::Duration};

use crate::errors::{AppError, AppResult};
use conductor::launch_holochain_process;
use errors::{LairKeystoreError, LaunchHolochainError};
use filesystem::{AppFileSystem, Profile};
use futures::lock::Mutex;
use holochain::{conductor::{
    config::{AdminInterfaceConfig, ConductorConfig, KeystoreConfig},
    interface::InterfaceDriver,
    Conductor, ConductorBuilder,
}, prelude::{KitsuneP2pConfig, ProxyConfig, TransportConfig, kitsune_p2p::dependencies::kitsune_p2p_types::config::KitsuneP2pTuningParams}};
use holochain_types::prelude::AppBundle;
use holochain_keystore::MetaLairClient;

use holochain_client::{AdminWebsocket, InstallAppPayload};

use lair::{initialize_keystore, launch_lair_keystore_process};
use logs::{setup_logs, log};
use menu::{build_menu, handle_menu_event};
use serde_json::Value;
use system_tray::{handle_system_tray_event, app_system_tray};
use tauri::{Manager, WindowBuilder, RunEvent, SystemTray, SystemTrayEvent, AppHandle, Window, App, api::process::Command, UserAttentionType, WindowEvent};

use utils::{sign_zome_call, ZOOM_ON_SCROLL, create_and_apply_lair_symlink};
use commands::{
    profile::{
        get_existing_profiles,
        set_active_profile,
        set_profile_network_seed,
        get_active_profile,
        open_profile_settings
    },
    restart::restart,
    notifications::{clear_systray_icon, notify_os, SysTrayIconState, IconState}
};


const APP_NAME: &str = "Word Condenser"; // name of the app. Can be changed without breaking your app.
const APP_ID: &str = "Word Condenser"; // App id used to install your app in the Holochain conductor - can be the same as APP_NAME. Changing this means a breaking change to your app.
pub const WINDOW_TITLE: &str = "Word Condenser"; // Title of the window
// pub const WINDOW_WIDTH: f64 = 1400.0; // Default window width when the app is opened
// pub const WINDOW_HEIGHT: f64 = 880.0; // Default window height when the app is opened
const PASSWORD: &str = "pass"; // Password to the lair keystore
pub const DEFAULT_NETWORK_SEED: Option<&str> = None;  // replace-me (optional): Depending on your application, you may want to put a network seed here or
                                                    // read it secretly from an environment variable. If so, replace `None` with `Some("your network seed here")`


mod conductor;
mod errors;
mod filesystem;
mod menu;
mod lair;
mod logs;
mod system_tray;
mod utils;
mod commands;



fn main() {
    let disable_deep_link = std::env::var("DISABLE_DEEP_LINK").is_ok();

    if !disable_deep_link {
        // Needs to be equal to the identifier in tauri.conf.json
        tauri_plugin_deep_link::prepare("wordcondenser");
    }

    let builder_result = tauri::Builder::default()

        .on_menu_event(|event| handle_menu_event(event.menu_item_id(), event.window()))

        // optional (systray) -- Adds your app with an icon to the OS system tray.
        .system_tray(SystemTray::new().with_menu(app_system_tray()))
        .on_system_tray_event(|app, event| match event {
          SystemTrayEvent::MenuItemClick { id, .. } => handle_system_tray_event(app, id),
          _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            clear_systray_icon,
            notify_os,
            sign_zome_call,
            log,
            set_active_profile,
            get_active_profile,
            get_existing_profiles,
            set_profile_network_seed,
            open_profile_settings,
            restart,
        ])
        .setup(move |app| {

            let handle = app.handle();

            let mut deep_link_url = None;

            if cfg!(not(target_os = "macos")) {
                deep_link_url = std::env::args().nth(1);
            }

            // convert profile from CLI to option, then read from filesystem instead. if profile from CLI,
            // then set current profile!
            let profile_from_cli = read_profile_from_cli(app)?;

            let profile = match profile_from_cli {
                Some(profile) => profile,
                None => {
                    let fs_tmp = AppFileSystem::new(&handle, &String::from("default"))?;
                    fs_tmp.get_active_profile()
                },
            };

            // start conductor and lair
            let fs = AppFileSystem::new(&handle, &profile).unwrap();

            // set up logs
            if let Err(err) = setup_logs(fs.clone()) {
                println!("Error setting up the logs: {:?}", err);
            }

            app.manage(fs.clone());

            app.manage(Mutex::new(SysTrayIconState { icon_state: IconState::Clean }));

            tauri::async_runtime::block_on(async move {
                let (meta_lair_client, app_port, admin_port) = launch(&fs, PASSWORD.to_string()).await.unwrap();

                app.manage(Mutex::new(meta_lair_client));
                app.manage((app_port, admin_port));

                // Get startup time to write on window object for UI to use.
                let startup_time = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    Ok(time) => Some(time.as_millis()),
                    Err(e) => {
                        log::error!("Failed to get system time: {}", e);
                        None
                    }
                };

                let app_window: Window = build_main_window(fs, &app.app_handle(), app_port, admin_port, startup_time, deep_link_url);

                if !disable_deep_link {
                    if let Err(err) = tauri_plugin_deep_link::register("wordcondenser", move |request| {
                        println!("Received deep-link: {}", request);
                        app_window.emit("deep-link-received", request.clone()).unwrap();
                        app_window
                            .request_user_attention(Some(UserAttentionType::Informational))
                            .unwrap();
                        app_window.show().unwrap();
                        app_window.unminimize().unwrap();
                        app_window.set_focus().unwrap();

                        if cfg!(target_os = "macos") {
                            app_window.eval(format!("window.localStorage.setItem('initialDeepLink', {})", request).as_str()).unwrap()
                        }

                        if cfg!(target_os = "linux") { // remove dock icon wiggeling after 10 seconds
                            std::thread::sleep(std::time::Duration::from_secs(10));
                            app_window
                                .request_user_attention(None)
                                .unwrap();
                        }
                    }) {
                        println!("Error registering the deep link plugin: {:?}", err);
                    }
                }
            });

            Ok(())

        }).build(tauri::generate_context!());

        match builder_result {
            Ok(builder) => {
                builder.run(|app_handle, event| {
                    match event {
                        // This event is emitted upon quitting the Launcher via cmq+Q on macOS.
                        // Sidecar binaries need to get explicitly killed in this case (https://github.com/holochain/launcher/issues/141)
                        RunEvent::Exit => tauri::api::process::kill_children(),

                        // optional (systray):
                        // This event is emitted upon pressing the x to close the App window
                        // The app is prevented from exiting to keep it running in the background with the system tray
                        // Remove those lines below with () if you don't want the systray functionality
                        RunEvent::ExitRequested { api, .. } => api.prevent_exit(),

                        // If a window is requested to be closed, hide it instead. This is to keep the UI running in the
                        // background to be able to send/receive notifications.
                        // TODO garbage collect windows in the front-end if they have notificationSettings all turned off
                        RunEvent::WindowEvent { label, event: window_event, .. } => {
                            match window_event {
                                WindowEvent::CloseRequested { api, .. } => {
                                    if label == "main" {
                                        let window_option = app_handle.get_window(&label);
                                        if let Some(window) = window_option {
                                            #[cfg(not(target_os = "macos"))]
                                            window.hide().unwrap();
                                            // https://github.com/tauri-apps/tauri/issues/3084
                                            #[cfg(target_os = "macos")]
                                            app_handle.hide();

                                            api.prevent_close();
                                        }
                                    }
                                },
                                _ => (),
                            }
                        },
                        _ => (),
                    }
              });
            }
            Err(err) => log::error!("Error building the app: {:?}", err),
        }

}


pub fn build_main_window(fs: AppFileSystem, app_handle: &AppHandle, app_port: u16, admin_port: u16, startup_time: Option<u128>, deep_link_url: Option<String>) -> Window {

    let startup_time = match startup_time {
        Some(time) => time.to_string(),
        None => String::from("undefined"),
    };

    let mut builder = WindowBuilder::new(
        app_handle,
        "main",
        tauri::WindowUrl::App("index.html".into())
      )
        // optional (OSmenu) -- Adds an OS menu to the app
        .menu(build_menu())
        // optional -- diables file drop handler. Disabling is required for drag and drop to work on certain platforms
        .disable_file_drop_handler()
        // .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(true)
        .title(WINDOW_TITLE)
        .maximized(true)
        .data_directory(fs.profile_data_dir)
        .initialization_script(format!("window.__HC_LAUNCHER_ENV__ = {{ 'APP_INTERFACE_PORT': {}, 'ADMIN_INTERFACE_PORT': {}, 'INSTALLED_APP_ID': '{}' }}", app_port, admin_port, APP_ID).as_str())
        .initialization_script(format!("window.__HC_KANGAROO__ = {{ startup_time: {} }}", startup_time).as_str())
        .initialization_script(ZOOM_ON_SCROLL);

    if let Some(deep_link) = deep_link_url {
        builder = builder.initialization_script(format!("window.localStorage.setItem('initialDeepLink', '{}')", deep_link).as_str());
    }

    // // maximizing the window does not seem to work on Windows
    // if cfg!(not(target_family="windows")) {
    //     builder = builder.maximized(true);
    // }

    let window = builder
        .build()
        .unwrap();

    // // try maximizing after building the window instead on Windows
    // if cfg!(target_family="windows") {
    //     window.maximize().unwrap();
    // }

    window

}

pub async fn launch(
    fs: &AppFileSystem,
    password: String,
) -> AppResult<(MetaLairClient, u16, u16)> {

    let log_level = log::Level::Warn;

    if !fs.keystore_dir().exists() {
        std::fs::create_dir_all(fs.keystore_dir().clone())?;
    }

    if !fs.conductor_dir().exists() {
        std::fs::create_dir_all(fs.conductor_dir().clone())?;
    }

    // initialize lair keystore if necessary
    if !fs.keystore_initialized() {
        initialize_keystore(fs.keystore_dir(), password.clone()).await?;
    }

    // spawn lair keystore process and connect to it
    let lair_url = launch_lair_keystore_process(log_level.clone(), fs.keystore_dir(), password.clone()).await?;

    let meta_lair_client = holochain_keystore::lair_keystore::spawn_lair_keystore(
        lair_url.clone(),
        sodoken::BufRead::from(password.clone().into_bytes())
        ).await
        .map_err(|e| LairKeystoreError::SpawnMetaLairClientError(format!("{}", e)))?;

    // write conductor config to file

    let mut config = ConductorConfig::default();
    config.environment_path = fs.conductor_dir().into();
    config.keystore = KeystoreConfig::LairServer { connection_url: lair_url };

    let admin_port = portpicker::pick_unused_port().expect("Cannot find any unused port");

    config.admin_interfaces = Some(vec![AdminInterfaceConfig {
        driver: InterfaceDriver::Websocket {
            port: admin_port.clone(),
        },
    }]);

    let mut network_config = KitsuneP2pConfig::default();
    network_config.bootstrap_service = Some(url2::url2!("https://bootstrap.holo.host"));

    let tuning_params = KitsuneP2pTuningParams::default();
    network_config.tuning_params = tuning_params;

    network_config.transport_pool.push(TransportConfig::Proxy {
      sub_transport: Box::new(TransportConfig::Quic {
        bind_to: None,
        override_host: None,
        override_port: None,
      }),
      proxy_config: ProxyConfig::RemoteProxyClient {
        proxy_url:  url2::url2!("kitsune-proxy://f3gH2VMkJ4qvZJOXx0ccL_Zo5n-s_CnBjSzAsEHHDCA/kitsune-quic/h/137.184.142.208/p/5788/--")
      },
    });

    config.network = Some(network_config);

    // will fail if lair has not ever been set up yet.
    #[cfg(target_family="unix")]
    let _symlink_succeeded = match create_and_apply_lair_symlink(fs.keystore_dir()) {
        Ok(()) => true,
        Err(_e) => false,
    };

    // TODO more graceful error handling
    let config_string = serde_yaml::to_string(&config).expect("Could not convert conductor config to string");

    let conductor_config_path = fs.conductor_dir().join("conductor-config.yaml");

    std::fs::write(conductor_config_path.clone(), config_string)
        .expect("Could not write conductor config");

    // NEW_VERSION change holochain version number here if necessary
    let command = Command::new_sidecar("holochain-wc-v0.1.6")
        .map_err(|err| AppError::LaunchHolochainError(
            LaunchHolochainError::SidecarBinaryCommandError(format!("{}", err)))
        )?;

    let _command_child = launch_holochain_process(
        log_level,
        command,
        conductor_config_path,
        password
    ).await?;

    std::thread::sleep(Duration::from_millis(100));

    // Try to connect twice. This fixes the os(111) error for now that occurs when the conducor is not ready yet.
    let mut admin_ws = match AdminWebsocket::connect(format!("ws://localhost:{}", admin_port))
    .await
    {
        Ok(ws) => ws,
        Err(_) => {
            log::error!("[HOLOCHAIN] Could not connect to the AdminWebsocket. Starting another attempt in 5 seconds.");
            std::thread::sleep(Duration::from_millis(5000));
            AdminWebsocket::connect(format!("ws://localhost:{}", admin_port))
                .await
                .map_err(|err| LaunchHolochainError::CouldNotConnectToConductor(format!("{}", err)))?
        }
    };

    let app_port = {
        let app_interfaces = admin_ws.list_app_interfaces().await.map_err(|e| {
            LaunchHolochainError::CouldNotConnectToConductor(format!(
            "Could not list app interfaces: {:?}",
            e
            ))
        })?;

        if app_interfaces.len() > 0 {
            app_interfaces[0]
        } else {
            let free_port = portpicker::pick_unused_port().expect("No ports free");

            admin_ws.attach_app_interface(free_port).await.or(Err(
            LaunchHolochainError::CouldNotConnectToConductor("Could not attach app interface".into()),
            ))?;
            free_port
        }
    };

    // let network_seed = match fs.read_profile_network_seed() {
    //     Some(seed) => Some(seed),
    //     None => DEFAULT_NETWORK_SEED.map(|s| s.to_string()),
    // };

    let network_seed: Option<String> = Some(uuid::Uuid::new_v4().to_string());

    install_app_if_necessary(network_seed, &mut admin_ws).await?;

    Ok((meta_lair_client, app_port, admin_port))
}


fn read_profile_from_cli(app: &mut App) -> Result<Option<Profile>, tauri::Error> {
    // reading profile from cli
    let cli_matches = app.get_cli_matches()?;
    let profile: Option<Profile> = match cli_matches.args.get("profile") {
        Some(data) => match data.value.clone() {
            Value::String(profile) => {
            if profile == "default" {
                eprintln!("Error: The name 'default' is not allowed for a profile.");
                panic!("Error: The name 'default' is not allowed for a profile.");
            }
            // \, /, and ? have a meaning as path symbols or domain socket url symbols and are therefore not allowed
            // because they would break stuff
            if profile.contains("/") || profile.contains("\\") || profile.contains("?") {
                eprintln!("Error: \"/\", \"\\\" and \"?\" are not allowed in profile names.");
                panic!("Error: \"/\", \"\\\" and \"?\" are not allowed in profile names.");
            }
            Some(profile)
            },
            _ => {
            // println!("ERROR: Value passed to --profile option could not be interpreted as string.");
            None
            // panic!("Value passed to --profile option could not be interpreted as string.")
            }
        },
        None => None
    };

    Ok(profile)
}



pub async fn install_app_if_necessary(
    network_seed: Option<String>,
    admin_ws: &mut AdminWebsocket,
) -> AppResult<()> {

    let apps = admin_ws.list_apps(None).await
        .map_err(|e| AppError::ConductorApiError(e))?;

    if !apps
        .iter()
        .map(|info| info.installed_app_id.clone())
        .collect::<Vec<String>>()
        .contains(&APP_ID.to_string())
    {
        let agent_key = admin_ws.generate_agent_pub_key().await
            .map_err(|e| AppError::ConductorApiError(e))?;

        // replace-me --- replace the path with the correct path to your .happ file here
        let app_bundle = AppBundle::decode(include_bytes!("../../pouch/word-condenser.happ"))
            .map_err(|e| AppError::AppBundleError(e))?;

        admin_ws
            .install_app(InstallAppPayload {
                source: holochain_types::prelude::AppBundleSource::Bundle(
                    app_bundle,
                ),
                agent_key: agent_key.clone(),
                network_seed: network_seed.clone(),
                installed_app_id: Some(APP_ID.to_string()),
                membrane_proofs: HashMap::new(),
            })
            .await
            .map_err(|e| AppError::ConductorApiError(e))?;

        admin_ws.enable_app(APP_ID.to_string()).await
            .map_err(|e| AppError::ConductorApiError(e))?;
    }

    Ok(())
}


async fn _try_build_conductor(conductor_builder: ConductorBuilder, keystore_data_dir: PathBuf, config: ConductorConfig, password: String) -> AppResult<Arc<Conductor>> {
    match conductor_builder.build().await {
        Ok(conductor) => Ok(conductor),
        Err(e) => {
            if cfg!(target_family="unix") && e.to_string().contains("path must be shorter than libc::sockaddr_un.sun_path") {
                create_and_apply_lair_symlink(keystore_data_dir)?;
                return Conductor::builder()
                    .config(config)
                    .passphrase(
                        Some(
                            utils::_vec_to_locked(password.into_bytes())
                                .map_err(|e| AppError::IoError(e))?
                        )
                    ).build()
                    .await
                    .map_err(|e| AppError::ConductorError(e))
            }
            Err(AppError::ConductorError(e))
        }
    }
}

