//! Reading text from the system clipboard on demand (egui itself only
//! delivers clipboard text on a paste shortcut). Native reads it directly;
//! the web uses the async Clipboard API, which needs a secure context
//! (https, or http on localhost).

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;

pub struct ClipboardReader {
    tx: Sender<Result<String, String>>,
    rx: Receiver<Result<String, String>>,
}

impl ClipboardReader {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }

    /// Start reading the clipboard; the text arrives through [`Self::poll`].
    pub fn request(&self, ctx: &egui::Context) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).map_err(|e| e.to_string());
            let _ = self.tx.send(text);
            ctx.request_repaint();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let _ = tx.send(web::read_text().await);
                ctx.request_repaint();
            });
        }
    }

    pub fn poll(&self) -> Option<Result<String, String>> {
        self.rx.try_recv().ok()
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::{JsCast, JsValue};

    fn message(e: JsValue) -> String {
        match e.dyn_ref::<js_sys::Error>() {
            Some(e) => String::from(e.message()),
            None => format!("{e:?}"),
        }
    }

    pub async fn read_text() -> Result<String, String> {
        let navigator = web_sys::window().ok_or("no window")?.navigator();
        // `navigator.clipboard` is undefined outside secure contexts.
        let clipboard = js_sys::Reflect::get(&navigator, &"clipboard".into()).map_err(message)?;
        if clipboard.is_undefined() {
            return Err("the browser clipboard needs https or localhost; press Ctrl+V over the map instead".into());
        }
        let clipboard: web_sys::Clipboard = clipboard.unchecked_into();
        let text = wasm_bindgen_futures::JsFuture::from(clipboard.read_text()).await.map_err(message)?;
        text.as_string().ok_or_else(|| "the clipboard holds no text".into())
    }
}
