use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub fn annotation_route(project_id: &str, image_id: Option<&str>) -> String {
    match image_id {
        Some(image_id) => format!("#/annotate/{project_id}/{image_id}"),
        None => format!("#/annotate/{project_id}"),
    }
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
        window.show().map_err(|err| err.to_string())?;
        window.set_focus().map_err(|err| err.to_string())?;
        return Ok(());
    }

    WebviewWindowBuilder::new(
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

    Ok(())
}
