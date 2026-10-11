//! Simple Photo inside SimpleHub. The page mounts the app on a canvas it owns and hands over a
//! bridge object (`open(purpose): Promise<{name, bytes}|null>`, `save(name, bytes): Promise<void>`)
//! that moves files to and from Simple Cloud; nothing here touches the browser's file picker or
//! triggers downloads. `open` is called with the purpose as its only argument, `"open"` for
//! File › Open (documents and preset files) and `"place"` for a single file a command reads (a
//! placed image, a script, notes), so a bridge that wants to filter its picker can, and one that
//! ignores arguments still works. An optional `theme` property (`"dark"` or `"light"`) on the
//! bridge picks the initial theme, and `PhotoHandle::set_theme` changes it while the app runs.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use photocraft_engine::Session;
use photocraft_ui_egui::theme::ThemeKind;
use photocraft_ui_egui::{PhotocraftApp, Services};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use super::{FileDialogAnswer, FileDialogRequest, Inbox, ThemeLink, WebShell, common_services, file_name, listen_pen};

/// A running instance. SimpleHub calls `destroy` when the module unmounts.
#[wasm_bindgen]
pub struct PhotoHandle {
    runner: eframe::WebRunner,
    theme: Rc<ThemeLink>,
}

#[wasm_bindgen]
impl PhotoHandle {
    pub fn destroy(&self) {
        self.runner.destroy();
    }

    pub fn has_panicked(&self) -> bool {
        self.runner.has_panicked()
    }

    /// Switches the running app to `"dark"` or `"light"` on its next frame. Other values are
    /// ignored.
    pub fn set_theme(&self, theme: &str) {
        if let Some(kind) = parse_theme(theme) {
            self.theme.request(kind);
        }
    }
}

/// `"dark"` is the Pro (Dark) theme, `"light"` is Studio (Light).
fn parse_theme(theme: &str) -> Option<ThemeKind> {
    match theme.trim().to_ascii_lowercase().as_str() {
        "dark" => Some(ThemeKind::Pro),
        "light" => Some(ThemeKind::StudioLight),
        _ => None,
    }
}

/// Starts Simple Photo on `canvas`, with `bridge` providing open and save. An optional `theme`
/// property (`"dark"` or `"light"`) on the bridge picks the initial theme; without it the app
/// keeps its default (the saved preference, Pro (Medium Gray) at first).
#[wasm_bindgen]
pub async fn start_simple_photo(canvas: web_sys::HtmlCanvasElement, bridge: JsValue) -> Result<PhotoHandle, JsValue> {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    let initial = js_sys::Reflect::get(&bridge, &JsValue::from_str("theme")).ok().and_then(|v| v.as_string()).and_then(|s| parse_theme(&s));
    let theme = Rc::new(ThemeLink::default());
    let link = theme.clone();
    let mut options = eframe::WebOptions::default();
    photocraft_ui_egui::gpu_canvas::use_adapter_limits(&mut options.wgpu_options.wgpu_setup);
    let pen_target = canvas.clone();
    let runner = eframe::WebRunner::new();
    runner
        .start(
            canvas,
            options,
            Box::new(move |cc| {
                let first = initial.unwrap_or(ThemeKind::Pro);
                PhotocraftApp::setup_context(&cc.egui_ctx, first);
                let inbox: Inbox = Arc::default();
                let mut app = PhotocraftApp::new(Session::new(), services(bridge, inbox.clone()));
                listen_pen(&pen_target, app.stylus.feed.clone());
                app.set_theme(&cc.egui_ctx, first);
                // The shell applies this after the first frame's preference load, so the bridge's
                // choice wins over a saved theme.
                link.pending.set(initial);
                *link.ctx.borrow_mut() = Some(cc.egui_ctx.clone());
                if let Some(rs) = cc.wgpu_render_state.clone() {
                    log::info!("photocraft-web (simplehub): wgpu backend {:?}", rs.adapter.get_info().backend);
                    app.set_wgpu(rs);
                }
                // SimpleHub owns the page, so it (not a `beforeunload` guard here) decides what
                // happens to unsaved work when the module unmounts.
                Ok(Box::new(WebShell { app, inbox, unsaved: Arc::new(AtomicBool::new(false)), theme: link }))
            }),
        )
        .await?;
    Ok(PhotoHandle { runner, theme })
}

/// The shared web services, with File › Open / Place answered by `bridge.open` and every write
/// (Save, Save As, Export, Quick Export) handed to `bridge.save`.
fn services(bridge: JsValue, inbox: Inbox) -> Services {
    let open_bridge = bridge.clone();
    Services {
        file_dialog: Some(Box::new(move |request, _parent, reply| match request {
            FileDialogRequest::Open { multiple } => {
                let bridge = open_bridge.clone();
                let purpose = if multiple { "open" } else { "place" };
                wasm_bindgen_futures::spawn_local(async move {
                    let answer = match call_open(&bridge, purpose).await {
                        Ok(picked) => picked.map(|(name, bytes)| FileDialogAnswer::Contents(name, bytes)),
                        Err(e) => {
                            log::error!("SimpleHub open failed: {e:?}");
                            None
                        }
                    };
                    reply.send(answer);
                });
            }
            // No save dialog: the suggested name is the name Simple Cloud gets.
            FileDialogRequest::Save { suggested } => reply.send(Some(FileDialogAnswer::SaveTo(file_name(&suggested)))),
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            let bridge = bridge.clone();
            let name = file_name(path);
            let bytes = js_sys::Uint8Array::from(bytes);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = call_save(&bridge, &name, &bytes).await {
                    log::error!("SimpleHub save of {name} failed: {e:?}");
                }
            });
            Ok(())
        })),
        ..common_services(inbox)
    }
}

fn method(bridge: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
    js_sys::Reflect::get(bridge, &JsValue::from_str(name))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| JsValue::from_str(&format!("bridge.{name} is not a function")))
}

async fn call_open(bridge: &JsValue, purpose: &str) -> Result<Option<(String, Vec<u8>)>, JsValue> {
    let promise: js_sys::Promise = method(bridge, "open")?.call1(bridge, &JsValue::from_str(purpose))?.dyn_into()?;
    let result = JsFuture::from(promise).await?;
    if result.is_null() || result.is_undefined() {
        return Ok(None);
    }
    let name = js_sys::Reflect::get(&result, &JsValue::from_str("name"))?.as_string().unwrap_or_else(|| "Image.psd".into());
    let bytes: js_sys::Uint8Array = js_sys::Reflect::get(&result, &JsValue::from_str("bytes"))?.dyn_into()?;
    Ok(Some((name, bytes.to_vec())))
}

async fn call_save(bridge: &JsValue, name: &str, bytes: &js_sys::Uint8Array) -> Result<(), JsValue> {
    let ret = method(bridge, "save")?.call2(bridge, &JsValue::from_str(name), bytes)?;
    if let Ok(promise) = ret.dyn_into::<js_sys::Promise>() {
        JsFuture::from(promise).await?;
    }
    Ok(())
}
