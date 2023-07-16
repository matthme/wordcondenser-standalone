// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;

use crate::errors::{AppError, AppResult};
use filesystem::{AppFileSystem, Profile};
use futures::lock::Mutex;
use holochain::{conductor::{
    config::{AdminInterfaceConfig, ConductorConfig, KeystoreConfig},
    interface::InterfaceDriver,
    Conductor, ConductorHandle,
}, prelude::{KitsuneP2pConfig, TransportConfig}};
use holochain_types::prelude::AppBundle;

use holochain_client::{AdminWebsocket, InstallAppPayload};

use logs::{setup_logs, log};
use menu::{build_menu, handle_menu_event};
use serde_json::Value;
use system_tray::{handle_system_tray_event, app_system_tray};
use tauri::{Manager, WindowBuilder, RunEvent, SystemTray, SystemTrayEvent, AppHandle, Window, App};

use utils::{sign_zome_call, ZOOM_ON_SCROLL};

const APP_NAME: &str = "Word Condenser"; // name of the app. Can be changed without breaking your app.
const APP_ID: &str = "Word Condenser"; // App id used to install your app in the Holochain conductor - can be the same as APP_NAME. Changing this means a breaking change to your app.
pub const WINDOW_TITLE: &str = "Word Condenser"; // Title of the window
pub const WINDOW_WIDTH: f64 = 1400.0; // Default window width when the app is opened
pub const WINDOW_HEIGHT: f64 = 880.0; // Default window height when the app is opened
const PASSWORD: &str = "pass"; // Password to the lair keystore
// const NETWORK_SEED: Option<String> = None; // replace-me (optional): You may want to put a network seed here or read it secretly from an environment variable
const SIGNALING_SERVER: &str = "wss://signal.holo.host";


mod errors;
mod filesystem;
mod menu;
mod logs;
mod system_tray;
mod utils;



fn main() {

    let builder_result = tauri::Builder::default()

        // optional (OSmenu) -- Adds an OS menu to the app
        .menu(build_menu())
        .on_menu_event(|event| handle_menu_event(event.menu_item_id(), event.window()))

        // optional (systray) -- Adds your app with an icon to the OS system tray.
        .system_tray(SystemTray::new().with_menu(app_system_tray()))
        .on_system_tray_event(|app, event| match event {
          SystemTrayEvent::MenuItemClick { id, .. } => handle_system_tray_event(app, id),
          _ => {}
        })

        .invoke_handler(tauri::generate_handler![sign_zome_call, log])
        .setup(|app| {

            let profile = read_profile_from_cli(app)?;

            if profile == String::from("default") {
                app.handle().plugin(tauri_plugin_single_instance::init(move |app, _argv, _cwd| {
                    let main_window = app.get_window("main");
                    if let Some(window) = main_window {
                        window.show().unwrap();
                        window.unminimize().unwrap();
                        window.set_focus().unwrap();
                    } else {
                        let fs = app.state::<AppFileSystem>().inner().to_owned();
                        let (app_port, admin_port) = app.state::<(u16, u16)>().inner().to_owned();
                        let _r = build_main_window(fs, app, app_port, admin_port);
                    }
                }))?;
            }

            let handle = app.handle();

            // start conductor and lair
            let fs = AppFileSystem::new(&handle, &profile)?;

            // set up logs
            if let Err(err) = setup_logs(fs.clone()) {
                println!("Error setting up the logs: {:?}", err);
            }

            app.manage(fs.clone());

            tauri::async_runtime::block_on(async move {
                let (conductor, app_port, admin_port) = launch(&fs, PASSWORD.to_string()).await.unwrap();

                app.manage(Mutex::new(conductor));
                app.manage((app_port, admin_port));

                let _app_window: Window = build_main_window(fs, &app.app_handle(), app_port, admin_port);
            });

            Ok(())

        }).build(tauri::generate_context!());

        match builder_result {
            Ok(builder) => {
              builder.run(|_app_handle, event| {
                // optional (systray):
                // This event is emitted upon pressing the x to close the App window
                // The app is prevented from exiting to keep it running in the background with the system tray
                // Remove those lines below with () if you don't want the systray functionality
                if let RunEvent::ExitRequested { api, .. } = event {
                  api.prevent_exit();
                }
              });
            }
            Err(err) => log::error!("Error building the app: {:?}", err),
        }

}


pub fn build_main_window(fs: AppFileSystem, app_handle: &AppHandle, app_port: u16, admin_port: u16) -> Window {
    WindowBuilder::new(
        &app_handle.app_handle(),
        "main",
        tauri::WindowUrl::App("index.html".into())
      )
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(true)
        .title(WINDOW_TITLE)
        .data_directory(fs.app_data_dir)
        .center()
        .initialization_script(format!("window.__HC_LAUNCHER_ENV__ = {{ 'APP_INTERFACE_PORT': {}, 'ADMIN_INTERFACE_PORT': {}, 'INSTALLED_APP_ID': '{}' }}", app_port, admin_port, APP_ID).as_str())
        .initialization_script(ZOOM_ON_SCROLL)
        .build()
        .unwrap()
}

pub async fn launch(
    fs: &AppFileSystem,
    password: String,
) -> AppResult<(ConductorHandle, u16, u16)> {
    let mut config = ConductorConfig::default();
    config.environment_path = fs.conductor_path().into();
    config.keystore = KeystoreConfig::LairServerInProc {
        lair_root: Some(fs.keystore_path()),
    };

    let admin_port = portpicker::pick_unused_port().expect("Cannot find any unused port");

    config.admin_interfaces = Some(vec![AdminInterfaceConfig {
        driver: InterfaceDriver::Websocket {
            port: admin_port.clone(),
        },
    }]);

    let mut network_config = KitsuneP2pConfig::default();
    network_config.bootstrap_service = Some(url2::url2!("https://bootstrap.holo.host")); // replace-me (optional) -- change bootstrap server URL here if desired
    network_config.transport_pool.push(TransportConfig::WebRTC {
        signal_url: SIGNALING_SERVER.into(),
    });

    config.network = Some(network_config);

    // TODO: set the DHT arc depending on whether this is mobile (tauri 2.0)
    let conductor = Conductor::builder()
        .config(config)
        .passphrase(
            Some(
                utils::vec_to_locked(password.into_bytes())
                    .map_err(|e| AppError::IoError(e))?
            )
        )
        .build()
        .await
        .map_err(|e| AppError::ConductorError(e))?;

    let mut admin_ws = utils::get_admin_ws(admin_port).await?;
    let app_port = conductor
        .clone()
        .add_app_interface(either::Either::Left(0))
        .await
        .map_err(|e| AppError::ConductorError(e))?;

    let network_seed = uuid::Uuid::new_v4();

    install_app_if_necessary(Some(network_seed.to_string()), &mut admin_ws).await?;

    Ok((conductor, app_port, admin_port))
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


fn read_profile_from_cli(app: &mut App) -> Result<Profile, tauri::Error> {
    // reading profile from cli
    let cli_matches = app.get_cli_matches()?;
    let profile: Profile = match cli_matches.args.get("profile") {
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
        profile
        },
        _ => {
        // println!("ERROR: Value passed to --profile option could not be interpreted as string.");
        String::from("default")
        // panic!("Value passed to --profile option could not be interpreted as string.")
        }
    },
        None => String::from("default")
    };

    Ok(profile)
}


