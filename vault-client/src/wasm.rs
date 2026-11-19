// SPDX-License-Identifier: MIT
//! Browser drivers (feature `wasm`): the relay's `/mailbox/<id>` WebSocket
//! (binary frames) and a `setTimeout` [`Clock`]. They work in windows and
//! workers.

use crate::error::ClientError;
use crate::transport::{Clock, MailboxFactory, MailboxTransport};
use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use futures::future::LocalBoxFuture;
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt};
use js_sys::{ArrayBuffer, Function, Promise, Reflect, Uint8Array};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

fn net(msg: &str) -> ClientError {
    ClientError::Transport(format!("relay: {msg}"))
}

type Shared<T> = Rc<RefCell<Option<T>>>;

/// One mailbox WebSocket. Dropping it detaches the handlers and closes it.
pub struct WsTransport {
    ws: WebSocket,
    rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
}

impl WsTransport {
    /// Connect and wait for the socket to open (the caller bounds the wait).
    pub async fn open(url: &str) -> Result<Self, ClientError> {
        let ws = WebSocket::new(url).map_err(|_| net("bad relay URL"))?;
        ws.set_binary_type(BinaryType::Arraybuffer);
        let (tx, rx) = mpsc::unbounded::<Vec<u8>>();
        let tx: Shared<mpsc::UnboundedSender<Vec<u8>>> = Rc::new(RefCell::new(Some(tx)));
        let (open_tx, open_rx) = oneshot::channel::<bool>();
        let open_tx: Shared<oneshot::Sender<bool>> = Rc::new(RefCell::new(Some(open_tx)));
        let opened = |o: &Shared<oneshot::Sender<bool>>, ok: bool| {
            if let Some(s) = o.borrow_mut().take() {
                let _ = s.send(ok);
            }
        };

        let on_open = {
            let o = open_tx.clone();
            Closure::<dyn FnMut(Event)>::new(move |_| opened(&o, true))
        };
        let on_message = {
            let tx = tx.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                // Text frames are not part of the mailbox protocol.
                if let Ok(buf) = e.data().dyn_into::<ArrayBuffer>()
                    && let Some(t) = tx.borrow().as_ref()
                {
                    let _ = t.unbounded_send(Uint8Array::new(&buf).to_vec());
                }
            })
        };
        // Close and error both end the stream (an error is followed by close).
        let on_close = {
            let (tx, o) = (tx.clone(), open_tx.clone());
            Closure::<dyn FnMut(CloseEvent)>::new(move |_| {
                tx.borrow_mut().take();
                opened(&o, false);
            })
        };
        let on_error = {
            let (tx, o) = (tx.clone(), open_tx.clone());
            Closure::<dyn FnMut(Event)>::new(move |_| {
                tx.borrow_mut().take();
                opened(&o, false);
            })
        };
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        // Built before waiting so that dropping this future cleans up.
        let t = Self {
            ws,
            rx: Mutex::new(rx),
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        };
        if open_rx.await.unwrap_or(false) {
            Ok(t)
        } else {
            Err(net("connection failed"))
        }
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        self.ws.set_onerror(None);
        let _ = self.ws.close();
    }
}

#[async_trait(?Send)]
impl MailboxTransport for WsTransport {
    async fn send(&self, frame: &[u8]) -> Result<(), ClientError> {
        if self.ws.ready_state() != WebSocket::OPEN {
            return Err(net("closed"));
        }
        self.ws
            .send_with_u8_array(frame)
            .map_err(|_| net("send failed"))
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, ClientError> {
        Ok(self.rx.lock().await.next().await)
    }

    fn close(&self) {
        let _ = self.ws.close();
    }
}

/// Opens [`WsTransport`] mailboxes.
#[derive(Clone, Copy, Debug, Default)]
pub struct WsFactory;

#[async_trait(?Send)]
impl MailboxFactory for WsFactory {
    type Mailbox = WsTransport;
    async fn open_mailbox(&self, url: &str) -> Result<WsTransport, ClientError> {
        WsTransport::open(url).await
    }
}

/// `Date.now()` and `setTimeout` (works in windows and workers).
#[derive(Clone, Copy, Debug, Default)]
pub struct WebClock;

impl Clock for WebClock {
    fn now_ms(&self) -> u64 {
        js_sys::Date::now() as u64
    }

    fn sleep(&self, ms: u64) -> LocalBoxFuture<'static, ()> {
        let delay = ms.min(i32::MAX as u64) as f64;
        async move {
            // Without a timer every timeout and backoff would hang forever:
            // fail loudly instead (both windows and workers have setTimeout).
            let g = js_sys::global();
            let set_timeout = Reflect::get(&g, &JsValue::from_str("setTimeout"))
                .ok()
                .and_then(|f| f.dyn_into::<Function>().ok())
                .expect("setTimeout unavailable");
            let p = Promise::new(&mut |resolve, reject| {
                if let Err(e) = set_timeout.call2(&g, &resolve, &JsValue::from_f64(delay)) {
                    let _ = reject.call1(&JsValue::NULL, &e);
                }
            });
            JsFuture::from(p).await.expect("setTimeout failed");
        }
        .boxed_local()
    }
}
