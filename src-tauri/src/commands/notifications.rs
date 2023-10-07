use futures::lock::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{api::notification::Notification, Manager, Icon};


#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NotificationPayload {
    title: String,
    body: String,
    urgency: String,
}


#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum IconState {
    Clean,
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SysTrayIconState {
    pub icon_state: IconState
}

impl SysTrayIconState {
    pub fn get_icon_state(&self) -> IconState {
        self.icon_state.clone()
    }
}


#[tauri::command]
pub async fn notify_os(
    window: tauri::Window,
    app_handle: tauri::AppHandle,
    // profile: tauri::State<'_, Profile>,
    notification: NotificationPayload,
    os: bool,
    systray: bool,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err(String::from("Unauthorized: Attempted to call tauri command 'notify_os' which is not allowed in this window."))
    }

    if !window.is_focused().map_err(|e| format!("Failed to get focus state of main window: {}", e))? {
        if systray {
            change_systray_icon_state(&app_handle, &notification.urgency).await
                .map_err(|e| format!("Failed to change systray icon state: {}", e))?;
        }
        if os && notification.urgency == "high" {
            let os_notification =  Notification::new(&app_handle.config().tauri.bundle.identifier)
                .body(notification.body)
                .title(notification.title);

            os_notification.show()
                .map_err(|e| format!("Failed to to show OS notification: {}", e))?;

        }
    }

    Ok(())
}


async fn change_systray_icon_state(
  app_handle: &tauri::AppHandle,
  urgency: &String,
) -> tauri::Result<()> {

    let mutex = app_handle.state::<Mutex<SysTrayIconState>>();

    match urgency.as_str() {
        "low" => (),
        "medium" => {
            let systray_icon_state = mutex.lock().await.get_icon_state();
            match systray_icon_state {
                IconState::Clean | IconState::Low => {
                    let icon_path_option = app_handle.path_resolver().resolve_resource("icons/icon_priority_medium_32x32.png");
                    if let Some(icon_path) = icon_path_option {
                        app_handle.tray_handle().set_icon(Icon::File(icon_path))?;
                    }
                    *mutex.lock().await = SysTrayIconState { icon_state: IconState::Medium };
                },
                _ => (),
            }
        },
        "high" => {
            let icon_path_option = app_handle.path_resolver().resolve_resource("icons/icon_priority_high_32x32.png");
            if let Some(icon_path) = icon_path_option {
                app_handle.tray_handle().set_icon(Icon::File(icon_path))?;
            }
            *mutex.lock().await = SysTrayIconState { icon_state: IconState::High };
        },
        _ => log::error!("Got invalid notification urgency level: {}", urgency),
    }

    Ok(())
}


#[tauri::command]
pub async fn clear_systray_icon(
    app_handle: tauri::AppHandle,
  ) -> tauri::Result<()> {
    let mutex = app_handle.state::<Mutex<SysTrayIconState>>();
    let icon_path_option = app_handle.path_resolver().resolve_resource("icons/32x32.png");
    if let Some(icon_path) = icon_path_option {
        app_handle.tray_handle().set_icon(Icon::File(icon_path))?;
    }
    *mutex.lock().await = SysTrayIconState { icon_state: IconState::Clean };
    Ok(())
  }



