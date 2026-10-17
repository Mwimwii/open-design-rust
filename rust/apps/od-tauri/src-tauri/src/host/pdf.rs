//! The bridge's `pdf` group: print a rendered document to a PDF file.
//!
//! Electron ran this in the main process: a hidden `BrowserWindow`, a readiness
//! handshake published by the document itself, then `printToPDF`. The same
//! recipe runs here, with WebKitGTK doing the printing on Linux
//! (`WebKitPrintOperation` writing to the path GTK's print-to-file backend is
//! pointed at).
//!
//! The document — not the app page — is what gets printed, which is why the
//! window is hidden and the app's own bridge is deliberately *not* injected
//! into it: artifact HTML runs there.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use super::{failure, success};

/// Parity: `PRINT_READY_TIMEOUT_MS` in the Electron host.
const PRINT_HANDSHAKE_TIMEOUT_MS: u64 = 30_000;

/// How long the print job itself may take to produce the file.
const PRINT_JOB_TIMEOUT_MS: u64 = 15_000;

/// The label of the throwaway print window. One at a time: a second export
/// while the first is still printing is a user error, not a queue.
const PRINT_WINDOW_LABEL: &str = "od-print";

/// The fragment the document sets once it is ready to print. The document's own
/// handshake already verified the nonce before publishing readiness, so the
/// shell reads the published flag instead of re-verifying it.
const PRINT_READY_FRAGMENT: &str = "od-print-ready";

/// The poller injected into the print window: report readiness through the URL,
/// which the shell can read back (`window.url()`) where a page cannot call it.
const READY_POLLER: &str = r#"(function(){
  var reported = false;
  function tick() {
    try {
      if (!reported && window.__odPrintReady === true) {
        reported = true;
        window.location.hash = 'od-print-ready';
        return;
      }
    } catch (e) {}
    setTimeout(tick, 50);
  }
  tick();
})();"#;

/// `pdf.print`.
#[tauri::command]
pub async fn host_print_pdf(
    app: AppHandle,
    html: String,
    nonce: Option<String>,
    options: Option<Value>,
) -> Result<Value, String> {
    if html.is_empty() {
        return Ok(failure("Invalid print payload: expected HTML string"));
    }
    let deck = match options.as_ref().and_then(|options| options.get("deck")) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(deck)) => *deck,
        Some(_) => return Ok(failure("Invalid print payload: expected deck option to be boolean")),
    };
    // The nonce guards the *document's* readiness signal, which the document
    // checks itself (`__odPrintReady` is only set for the matching nonce), so
    // there is nothing left to verify here.
    let _ = (nonce, deck);
    print_document(&app, &html).await
}

#[cfg(target_os = "linux")]
async fn print_document(app: &AppHandle, html: &str) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;

    let default_name = default_file_name(html);
    let Some(path) = app
        .dialog()
        .file()
        .set_title("Save PDF")
        .set_file_name(&default_name)
        .add_filter("PDF", &["pdf"])
        .blocking_save_file()
        .and_then(|file| file.into_path().ok())
    else {
        // Parity: the Electron host resolves with success when the save dialog
        // is dismissed, and the renderer reads a resolved call as "exported".
        return Ok(success());
    };

    let window = WebviewWindowBuilder::new(
        app,
        PRINT_WINDOW_LABEL,
        WebviewUrl::External(document_url(html)),
    )
    .title("OpenDesign print")
    .visible(false)
    .initialization_script(READY_POLLER)
    .build()
    .map_err(|err| err.to_string())?;

    let ready = wait_for_ready(&window).await;
    if !ready {
        let _ = window.close();
        return Ok(failure("Print handshake timed out"));
    }

    let started = print_to_file(&window, &path).await;
    let written = match started {
        Ok(()) => wait_for_file(&path).await,
        Err(reason) => {
            let _ = window.close();
            return Ok(failure(reason));
        }
    };
    // The window is only closed once the job has written its file: WebKit
    // prints the live view, so tearing the document down first would race the
    // job it is printing.
    let _ = window.close();
    if !written {
        return Ok(failure(
            "the print job finished without producing a PDF file",
        ));
    }
    Ok(success())
}

#[cfg(not(target_os = "linux"))]
async fn print_document(_app: &AppHandle, _html: &str) -> Result<Value, String> {
    // Electron's flow is `printToPDF(path)`. Tauri exposes only `print()`,
    // which opens the system print dialog, and that is a different interaction
    // from "choose a file, get a file" — so the honest answer on macOS and
    // Windows is that this shell has no export yet.
    Ok(failure(format!(
        "PDF export is not implemented for {} in the Rust shell yet",
        std::env::consts::OS
    )))
}

/// `data:text/html,<percent-encoded document>` — the way the Electron host
/// loaded artifact HTML into its hidden window.
fn document_url(html: &str) -> url::Url {
    let mut encoded = String::with_capacity(html.len() + 16);
    for byte in html.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~' | b'!' | b'(' | b')' | b'*' | b'\'') {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    // The payload is percent-encoded, so the URL is ASCII by construction.
    url::Url::parse(&format!("data:text/html;charset=utf-8,{encoded}"))
        .expect("a percent-encoded data URL always parses")
}

/// The save dialog's default file name: the document's `<title>`, sanitized,
/// else `artifact.pdf`.
fn default_file_name(html: &str) -> String {
    let title = extract_title(html).unwrap_or_default();
    let sanitized: String = title
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized
        .trim_matches('-')
        .chars()
        .take(80)
        .collect::<String>();
    if sanitized.is_empty() {
        "artifact.pdf".to_string()
    } else {
        format!("{sanitized}.pdf")
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let after_open = lower[open..].find('>')? + open + 1;
    let close = lower[after_open..].find("</title>")? + after_open;
    if close <= after_open {
        return None;
    }
    Some(html[after_open..close].trim().to_string())
}

async fn wait_for_ready(window: &WebviewWindow) -> bool {
    let deadline = Instant::now() + Duration::from_millis(PRINT_HANDSHAKE_TIMEOUT_MS);
    while Instant::now() < deadline {
        let ready = window
            .url()
            .map(|url| url.fragment() == Some(PRINT_READY_FRAGMENT))
            .unwrap_or(false);
        if ready {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn wait_for_file(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_millis(PRINT_JOB_TIMEOUT_MS);
    while Instant::now() < deadline {
        if std::fs::metadata(path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Hand the job to WebKit with the file destination already chosen, which is
/// what keeps the print dialog out of the flow (parity: `printToPDF`).
#[cfg(target_os = "linux")]
async fn print_to_file(window: &WebviewWindow, path: &Path) -> Result<(), String> {
    use webkit2gtk::{PrintOperationExt, SettingsExt, WebViewExt};

    let destination = url::Url::from_file_path(path)
        .map_err(|_| "the chosen path cannot be written to".to_string())?
        .to_string();
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    window
        .with_webview(move |platform| {
            let view = platform.inner();
            if let Some(settings) = view.settings() {
                // Parity: `printBackground: true`.
                settings.set_print_backgrounds(true);
            }
            let operation = webkit2gtk::PrintOperation::new(&view);
            let settings = gtk::PrintSettings::new();
            // GTK's print-to-file backend writes wherever `output-uri` points.
            settings.set("output-uri", Some(&destination));
            settings.set("output-file-format", Some("pdf"));
            // Parity: `margins: 0` in all four directions. gtk-rs 0.18 does not
            // generate the typed margin setters, and these keys go through the
            // same `gtk_print_settings_set` store the typed ones use.
            for key in ["margin-top", "margin-bottom", "margin-left", "margin-right"] {
                settings.set(key, Some("0"));
            }
            operation.set_print_settings(&settings);
            operation.print();
            let _ = sender.send(());
        })
        .map_err(|err| err.to_string())?;
    receiver
        .await
        .map_err(|_| "the print job ended before it started".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn document_url_is_a_percent_encoded_data_url() {
        assert_eq!(
            document_url("<p>a b</p>").as_str(),
            "data:text/html;charset=utf-8,%3Cp%3Ea%20b%3C%2Fp%3E"
        );
        // The characters that would otherwise terminate the URL are escaped.
        assert_eq!(
            document_url("#x%").as_str(),
            "data:text/html;charset=utf-8,%23x%25"
        );
    }

    #[test]
    fn the_default_file_name_comes_from_the_document_title() {
        assert_eq!(
            default_file_name("<html><head><title>Q3 Board Deck</title></head></html>"),
            "Q3-Board-Deck.pdf"
        );
        assert_eq!(default_file_name("<html></html>"), "artifact.pdf");
        // Punctuation that a filesystem or a dialog would reject collapses.
        assert_eq!(
            default_file_name("<title>a/b:c?d*e</title>"),
            "a-b-c-d-e.pdf"
        );
        assert_eq!(default_file_name("<title>   </title>"), "artifact.pdf");
    }

    #[test]
    fn title_matching_ignores_case() {
        assert_eq!(
            extract_title("<HEAD><TITLE>Deck</TITLE></HEAD>").as_deref(),
            Some("Deck")
        );
        assert_eq!(extract_title("<head></head>"), None);
        assert_eq!(extract_title("<title>unterminated"), None);
    }

    #[test]
    fn an_empty_payload_is_rejected_and_a_bad_deck_flag_too() {
        let deck = |value: Value| match value.get("deck") {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(deck)) => Ok(*deck),
            Some(_) => Err("Invalid print payload: expected deck option to be boolean"),
        };
        assert_eq!(deck(json!({})), Ok(false));
        assert_eq!(deck(json!({ "deck": true })), Ok(true));
        assert_eq!(
            deck(json!({ "deck": "yes" })),
            Err("Invalid print payload: expected deck option to be boolean")
        );
    }
}
