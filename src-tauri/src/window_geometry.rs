//! Desktop geometry only; user-selected immersive modes are never restored implicitly.
use tauri::{Manager, Runtime, Window, WindowEvent};

#[derive(Clone, Copy, Debug)]
struct Geometry {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[cfg(test)]
fn fit(rect: Geometry, area: Geometry, first: bool) -> Geometry {
    fit_with_minimum(rect, area, first, (0.0, 0.0))
}

fn fit_with_minimum(
    mut rect: Geometry,
    area: Geometry,
    first: bool,
    minimum: (f64, f64),
) -> Geometry {
    let margin = if first {
        24.0_f64.min(area.width / 10.0).min(area.height / 10.0)
    } else {
        0.0
    };
    let width = (area.width - margin * 2.0).max(1.0);
    let height = (area.height - margin * 2.0).max(1.0);
    rect.width = rect.width.clamp(
        1.0,
        if first {
            (width * 0.85).max(minimum.0.min(width))
        } else {
            width
        },
    );
    rect.height = rect.height.clamp(
        1.0,
        if first {
            (height * 0.85).max(minimum.1.min(height))
        } else {
            height
        },
    );
    rect.x = if first {
        area.x + (area.width - rect.width) / 2.0
    } else {
        rect.x
            .clamp(area.x + margin, area.x + area.width - margin - rect.width)
    };
    rect.y = if first {
        area.y + (area.height - rect.height) / 2.0
    } else {
        rect.y
            .clamp(area.y + margin, area.y + area.height - margin - rect.height)
    };
    rect
}

fn path<R: Runtime>(window: &Window<R>) -> Option<std::path::PathBuf> {
    let label: String = window
        .label()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        window
            .app_handle()
            .path()
            .app_config_dir()
            .ok()?
            .join(format!("window-geometry-{label}.txt")),
    )
}

fn load<R: Runtime>(window: &Window<R>) -> Option<(Geometry, f64)> {
    let text = std::fs::read_to_string(path(window)?).ok()?;
    parse(&text)
}

fn parse(text: &str) -> Option<(Geometry, f64)> {
    let v: Vec<f64> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if v.len() != 5
        || !v.iter().all(|v| v.is_finite())
        || v[2] < 160.0
        || v[3] < 120.0
        || v[4] <= 0.0
    {
        return None;
    }
    Some((
        Geometry {
            x: v[0],
            y: v[1],
            width: v[2],
            height: v[3],
        },
        v[4],
    ))
}

fn save<R: Runtime>(window: &Window<R>) {
    if window.is_maximized().unwrap_or(true)
        || window.is_fullscreen().unwrap_or(true)
        || window.is_minimized().unwrap_or(true)
    {
        return;
    }
    let (Ok(position), Ok(size), Ok(scale), Some(path)) = (
        window.outer_position(),
        window.inner_size(),
        window.scale_factor(),
        path(window),
    ) else {
        return;
    };
    if size.width == 0 || size.height == 0 {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // Logical content dimensions survive DPI changes; desktop positions stay physical.
    let contents = format!(
        "{} {} {} {} {}\n",
        position.x,
        position.y,
        size.width as f64 / scale,
        size.height as f64 / scale,
        scale
    );
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, contents).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

fn physical_minimum(
    minimum: (f64, f64),
    width: f64,
    height: f64,
    scale: f64,
) -> tauri::PhysicalSize<u32> {
    tauri::PhysicalSize::new(
        (minimum.0.min(width) * scale).round().max(1.0) as u32,
        (minimum.1.min(height) * scale).round().max(1.0) as u32,
    )
}

fn apply<R: Runtime>(window: &Window<R>, restore: bool) {
    if !restore
        && (window.is_maximized().unwrap_or(false) || window.is_fullscreen().unwrap_or(false))
    {
        return;
    }
    let saved = if restore { load(window) } else { None };
    let monitors = window.available_monitors().unwrap_or_default();
    let chosen = saved.and_then(|(r, scale)| {
        monitors
            .iter()
            .filter_map(|monitor| {
                let a = monitor.work_area();
                let width = (r.x + r.width * scale).min(a.position.x as f64 + a.size.width as f64)
                    - r.x.max(a.position.x as f64);
                let height = (r.y + r.height * scale)
                    .min(a.position.y as f64 + a.size.height as f64)
                    - r.y.max(a.position.y as f64);
                (width > 0.0 && height > 0.0).then_some((width * height, monitor))
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, monitor)| monitor.clone())
    });
    let Some(monitor) = chosen
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten())
    else {
        return;
    };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let Ok(size) = window.inner_size() else {
        return;
    };
    let old_scale = window.scale_factor().unwrap_or(scale);
    let current_position = window.outer_position().unwrap_or(area.position);
    let first = restore && saved.is_none();
    let input = saved.map(|(r, _)| r).unwrap_or(Geometry {
        x: current_position.x as f64,
        y: current_position.y as f64,
        width: size.width as f64 / old_scale,
        height: size.height as f64 / old_scale,
    });
    // Account for native title bars/borders when fitting the outer rectangle.
    let outer = window.outer_size().unwrap_or(size);
    let border_w = outer.width.saturating_sub(size.width) as f64 / old_scale;
    let border_h = outer.height.saturating_sub(size.height) as f64 / old_scale;
    let min = window
        .app_handle()
        .config()
        .app
        .windows
        .iter()
        .find(|c| c.label == window.label())
        .map(|c| (c.min_width.unwrap_or(640.0), c.min_height.unwrap_or(420.0)))
        .unwrap_or(if window.label() == "backend-tasks" {
            (420.0, 520.0)
        } else if window.label().starts_with("annotation-") {
            (800.0, 520.0)
        } else {
            (720.0, 480.0)
        });
    let fitted = fit_with_minimum(
        Geometry {
            x: input.x / scale,
            y: input.y / scale,
            width: input.width + border_w,
            height: input.height + border_h,
        },
        Geometry {
            x: area.position.x as f64 / scale,
            y: area.position.y as f64 / scale,
            width: area.size.width as f64 / scale,
            height: area.size.height as f64 / scale,
        },
        first,
        (min.0 + border_w, min.1 + border_h),
    );
    let width = (fitted.width - border_w).max(1.0);
    let height = (fitted.height - border_h).max(1.0);
    // A removed/smaller display must not be defeated by a large fixed minimum.
    // Use the target display's physical scale even while the native window is
    // still on its previous display; a logical minimum would use that old DPI.
    let _ = window.set_min_size(Some(physical_minimum(min, width, height, scale)));
    let _ = window.set_position(tauri::PhysicalPosition::new(
        (fitted.x * scale).round() as i32,
        (fitted.y * scale).round() as i32,
    ));
    let _ = window.set_size(tauri::PhysicalSize::new(
        (width * scale).round() as u32,
        (height * scale).round() as u32,
    ));
}

pub fn init<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("compact-window-geometry")
        .on_window_ready(|window| {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                // Explicit wallpaper/browser/auxiliary surfaces keep their own behavior.
                if window.label() != "main"
                    && !window.label().starts_with("annotation-")
                    && !window.label().starts_with("tab-window-")
                    && window.label() != "backend-tasks"
                {
                    return;
                }
                apply(&window, true);
                let observed = window.clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::ScaleFactorChanged { .. } => apply(&observed, false),
                    WindowEvent::Moved(_)
                    | WindowEvent::Resized(_)
                    | WindowEvent::CloseRequested { .. } => save(&observed),
                    _ => {}
                });
            }
            let _ = window.show();
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn area() -> Geometry {
        Geometry {
            x: -1280.0,
            y: 24.0,
            width: 1280.0,
            height: 720.0,
        }
    }
    #[test]
    fn first_launch_leaves_desktop_visible() {
        let r = fit(
            Geometry {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
            area(),
            true,
        );
        assert!(r.width < 1100.0 && r.height < 620.0);
        assert!(r.x < 0.0 && r.y > 24.0);
    }
    #[test]
    fn valid_user_geometry_survives() {
        let r = Geometry {
            x: -1100.0,
            y: 80.0,
            width: 900.0,
            height: 600.0,
        };
        let f = fit(r, area(), false);
        assert_eq!((f.x, f.y, f.width, f.height), (r.x, r.y, r.width, r.height));
    }
    #[test]
    fn removed_monitor_geometry_is_recovered() {
        let r = fit(
            Geometry {
                x: 4000.0,
                y: -1000.0,
                width: 1800.0,
                height: 1000.0,
            },
            area(),
            false,
        );
        assert_eq!(r.width, 1280.0);
        assert_eq!(r.height, 720.0);
        assert!(r.x >= -1280.0 && r.y >= 24.0);
    }
    #[test]
    fn saved_edge_and_large_size_survive() {
        let r = Geometry {
            x: -1280.0,
            y: 24.0,
            width: 1280.0,
            height: 720.0,
        };
        let f = fit(r, area(), false);
        assert_eq!((f.x, f.y, f.width, f.height), (r.x, r.y, r.width, r.height));
    }
    #[test]
    fn small_screen_prefers_reachable_controls_over_default_percentage() {
        let f = fit_with_minimum(
            Geometry {
                x: 0.0,
                y: 0.0,
                width: 1080.0,
                height: 700.0,
            },
            Geometry {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            true,
            (800.0, 520.0),
        );
        assert_eq!(f.width, 752.0);
        assert_eq!(f.height, 520.0);
        assert!(f.x > 0.0);
    }
    #[test]
    fn corrupt_storage_is_ignored() {
        for input in [
            "",
            "0 0 NaN 500 2",
            "0 0 800 600 0",
            "0 0 10 10 2",
            "0 0 800 600 2 extra",
        ] {
            assert!(parse(input).is_none());
        }
        let (rect, scale) = parse("-2560 48 1100 700 2").unwrap();
        assert_eq!(rect.width, 1100.0);
        assert_eq!(rect.x, -2560.0);
        assert_eq!(scale, 2.0);
    }
    #[test]
    fn target_dpi_minimum_keeps_small_saved_sizes() {
        let minimum = physical_minimum((800.0, 520.0), 640.0, 400.0, 2.0);
        assert_eq!((minimum.width, minimum.height), (1280, 800));
        let minimum = physical_minimum((800.0, 520.0), 640.0, 400.0, 1.0);
        assert_eq!((minimum.width, minimum.height), (640, 400));
        let saved = Geometry {
            x: -1100.0,
            y: 80.0,
            width: 640.0,
            height: 400.0,
        };
        let fitted = fit_with_minimum(saved, area(), false, (800.0, 520.0));
        assert_eq!(
            (fitted.x, fitted.y, fitted.width, fitted.height),
            (saved.x, saved.y, saved.width, saved.height)
        );
    }
    #[test]
    fn tiny_work_area_never_produces_negative_geometry() {
        let r = fit(
            Geometry {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 600.0,
            },
            Geometry {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 80.0,
            },
            true,
        );
        assert!(r.width > 0.0 && r.height > 0.0);
    }
}
