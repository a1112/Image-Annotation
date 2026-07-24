use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub fn annotation_route(project_id: &str, image_id: Option<&str>) -> String {
    match image_id {
        Some(image_id) => format!("#/annotate/{project_id}/{image_id}"),
        None => format!("#/annotate/{project_id}"),
    }
}

pub fn annotation_navigation_script(route: &str) -> String {
    let route_json = serde_json::to_string(route).unwrap_or_else(|_| "\"#/annotate\"".to_string());
    format!("window.location.hash = {route_json};")
}

pub fn backend_tasks_route() -> String {
    "#/backend-tasks".to_string()
}

#[cfg(not(mobile))]
pub fn open_annotation_window(
    app: &AppHandle,
    project_id: &str,
    image_id: Option<&str>,
) -> Result<(), String> {
    let label = format!("annotation-{project_id}");
    let route = annotation_route(project_id, image_id);

    if let Some(window) = app.get_webview_window(&label) {
        window
            .eval(&annotation_navigation_script(&route))
            .map_err(|err| err.to_string())?;
        window.show().map_err(|err| err.to_string())?;
        window.set_focus().map_err(|err| err.to_string())?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("index.html{route}").into()),
    )
    .title("标注工作台")
    .inner_size(1440.0, 920.0)
    .min_inner_size(1024.0, 720.0)
    .resizable(true)
    .decorations(false)
    .transparent(true)
    .shadow(true)
    .build()
    .map_err(|err| err.to_string())?;
    crate::platform::configure_window(&window);

    Ok(())
}

#[cfg(not(mobile))]
pub fn open_backend_tasks_window(app: &AppHandle) -> Result<(), String> {
    let label = "backend-tasks";
    let route = backend_tasks_route();

    if let Some(window) = app.get_webview_window(label) {
        window.show().map_err(|err| err.to_string())?;
        window.set_focus().map_err(|err| err.to_string())?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("index.html{route}").into()),
    )
    .title("后台任务")
    .inner_size(520.0, 680.0)
    .min_inner_size(420.0, 520.0)
    .resizable(true)
    .decorations(false)
    .transparent(true)
    .shadow(true)
    .build()
    .map_err(|err| err.to_string())?;
    crate::platform::configure_window(&window);

    Ok(())
}
