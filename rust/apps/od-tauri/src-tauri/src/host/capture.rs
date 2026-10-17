//! The bridge's `capture` group: a PNG of the app window, optionally cropped.
//!
//! Electron answered this with `webContents.capturePage`. Tauri has no capture
//! API, so the platform webview is asked directly: WebKitGTK's
//! `webkit_web_view_get_snapshot` on Linux. The snapshot is always the visible
//! viewport, so a `clip` is applied here by drawing that region into a new
//! surface — the renderer's contract (`OpenDesignHostCaptureOptions.clip`) is in
//! CSS pixels, the way Electron's `Rectangle` was.

use serde_json::{json, Value};
use tauri::WebviewWindow;

use super::failure;

/// A validated capture region, in CSS pixels (parity: `parseCaptureClip`).
///
/// Electron rounded first and clamped second, so a fractionally positioned
/// element still produced a whole-pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// `capture.page`.
#[tauri::command]
pub async fn host_capture_page(
    window: WebviewWindow,
    options: Option<Value>,
) -> Result<Value, String> {
    let clip = parse_clip(&options);
    capture_impl(&window, clip).await
}

/// Parity: `parseCaptureClip` — a clip with a missing, non-numeric or
/// non-finite field is not a clip at all, and the capture is the full viewport.
pub fn parse_clip(options: &Option<Value>) -> Option<ClipRect> {
    let clip = options.as_ref()?.get("clip")?.as_object()?;
    let number = |key: &str| {
        clip.get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
    };
    let (x, y, width, height) = (
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    );
    Some(ClipRect {
        x: x.max(0.0).round() as u32,
        y: y.max(0.0).round() as u32,
        width: width.round().max(1.0) as u32,
        height: height.round().max(1.0) as u32,
    })
}

#[cfg(target_os = "linux")]
async fn capture_impl(window: &WebviewWindow, clip: Option<ClipRect>) -> Result<Value, String> {
    linux::capture(window, clip).await
}

#[cfg(target_os = "linux")]
mod linux {
    use base64::Engine;
    use cairo::ffi;
    use gtk::prelude::WidgetExt;
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

    use super::*;

    /// Render the visible viewport and answer `{ ok, dataUrl, w, h }`.
    pub async fn capture(window: &WebviewWindow, clip: Option<ClipRect>) -> Result<Value, String> {
        let (sender, receiver) = tokio::sync::oneshot::channel::<Result<Png, String>>();
        window
            .with_webview(move |platform| {
                // GTK allocates in logical pixels; the snapshot comes back at
                // the device scale factor, which `encode` recovers by comparing
                // the two.
                let view = platform.inner();
                let allocation = (
                    f64::from(view.allocated_width().max(1)),
                    f64::from(view.allocated_height().max(1)),
                );
                view.snapshot(
                    SnapshotRegion::Visible,
                    SnapshotOptions::NONE,
                    None::<&webkit2gtk::gio::Cancellable>,
                    move |result| {
                        let payload = match result {
                            Ok(surface) => encode(surface, allocation, clip),
                            Err(err) => Err(err.to_string()),
                        };
                        let _ = sender.send(payload);
                    },
                );
            })
            .map_err(|err| err.to_string())?;

        match receiver.await {
            Ok(Ok(png)) => Ok(json!({
                "ok": true,
                "dataUrl": format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&png.bytes)
                ),
                "w": png.width,
                "h": png.height,
            })),
            Ok(Err(reason)) => Ok(failure(reason)),
            Err(_) => Ok(failure("the window closed before the capture finished")),
        }
    }

    /// A captured PNG plus the CSS size it represents (Electron's
    /// `image.getSize()`).
    struct Png {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
    }

    fn encode(
        surface: cairo::Surface,
        allocation: (f64, f64),
        clip: Option<ClipRect>,
    ) -> Result<Png, String> {
        // The snapshot is a cairo image surface. Its pixel size is the
        // allocation times the device scale factor, which is also how a
        // clip expressed in CSS pixels becomes device pixels.
        let (surface_width, surface_height) = unsafe {
            (
                ffi::cairo_image_surface_get_width(surface.to_raw_none()),
                ffi::cairo_image_surface_get_height(surface.to_raw_none()),
            )
        };
        let (surface_width, surface_height) = if surface_width > 0 && surface_height > 0 {
            (surface_width, surface_height)
        } else {
            (allocation.0 as i32, allocation.1 as i32)
        };
        let scale = if allocation.0 > 0.0 {
            f64::from(surface_width) / allocation.0
        } else {
            1.0
        };

        let (x, y, width, height) = match clip {
            Some(clip) => {
                let x = clamp_index(f64::from(clip.x) * scale, surface_width);
                let y = clamp_index(f64::from(clip.y) * scale, surface_height);
                (
                    x,
                    y,
                    clamp_extent(f64::from(clip.width) * scale, surface_width - x),
                    clamp_extent(f64::from(clip.height) * scale, surface_height - y),
                )
            }
            None => (0, 0, surface_width, surface_height),
        };

        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
            .map_err(|err| err.to_string())?;
        let context = cairo::Context::new(&image).map_err(|err| err.to_string())?;
        unsafe {
            // SAFETY: both pointers come from live cairo objects, and the
            // source surface outlives the paint below. `Surface` itself has no
            // safe `AsRef<Surface>`, so the source is set through the entry
            // point that `Context::set_source_surface` wraps.
            ffi::cairo_set_source_surface(
                context.to_raw_none(),
                surface.to_raw_none(),
                -f64::from(x),
                -f64::from(y),
            );
        }
        let status = unsafe { ffi::cairo_status(context.to_raw_none()) };
        if status != ffi::STATUS_SUCCESS {
            return Err(format!("cairo could not read the snapshot (status {status})"));
        }
        context.paint().map_err(|err| err.to_string())?;

        let mut bytes = Vec::new();
        image.write_to_png(&mut bytes).map_err(|err| err.to_string())?;
        Ok(Png {
            bytes,
            // The renderer's size is the CSS size, exactly as Electron's
            // `NativeImage.getSize()` reported it.
            width: (f64::from(width) / scale).round().max(1.0) as u32,
            height: (f64::from(height) / scale).round().max(1.0) as u32,
        })
    }

    fn clamp_index(value: f64, limit: i32) -> i32 {
        (value.round() as i32).clamp(0, (limit - 1).max(0))
    }

    fn clamp_extent(value: f64, available: i32) -> i32 {
        (value.round() as i32).clamp(1, available.max(1))
    }
}

#[cfg(not(target_os = "linux"))]
async fn capture_impl(_window: &WebviewWindow, _clip: Option<ClipRect>) -> Result<Value, String> {
    // macOS and Windows need their own snapshot entry points (WKWebView's
    // `takeSnapshotWithConfiguration:`, WebView2's `CapturePreview`), which are
    // tracked with the rest of the platform work rather than guessed at here.
    Ok(failure(format!(
        "host page capture is not implemented for {} in the Rust shell yet",
        std::env::consts::OS
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_well_formed_clip_is_rounded_and_kept() {
        assert_eq!(
            parse_clip(&Some(json!({ "clip": { "x": 10.4, "y": 2.6, "width": 100.5, "height": 50.4 } }))),
            Some(ClipRect {
                x: 10,
                y: 3,
                width: 101,
                height: 50
            })
        );
    }

    #[test]
    fn negative_origins_collapse_to_zero_and_dimensions_never_do() {
        assert_eq!(
            parse_clip(&Some(json!({ "clip": { "x": -5, "y": -9, "width": 0, "height": -3 } }))),
            Some(ClipRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        );
    }

    #[test]
    fn a_partial_or_non_numeric_clip_is_no_clip() {
        assert_eq!(parse_clip(&None), None);
        assert_eq!(parse_clip(&Some(json!({}))), None);
        assert_eq!(parse_clip(&Some(json!({ "clip": null }))), None);
        assert_eq!(
            parse_clip(&Some(json!({ "clip": { "x": 1, "y": 2, "width": 3 } }))),
            None
        );
        assert_eq!(
            parse_clip(&Some(json!({ "clip": { "x": "1", "y": 2, "width": 3, "height": 4 } }))),
            None
        );
    }
}
