// SPDX-License-Identifier: MIT
//! `vault-web`: the browser package over [`vault_client`] for apps that keep
//! their state in a vault (`docs/integration.md`).
//!
//! JS API (wasm-bindgen; the typed wrapper is `js/index.ts`):
//!
//! - `pair({ relayOrigin, appId, deviceName, onOffer, onSas, onState, testOrigin? })`
//!   starts one pairing and returns a [`Session`]. The page origin is
//!   `location.origin`; `testOrigin` exists only for runtimes without
//!   `location` (Node tests) and must equal `location.origin` where there is one.
//! - `onOffer({ text, qr: { size, modules } | null })`: the pairing text and its
//!   QR module matrix ([`core::qr_matrix`]; `null` only if the text does not fit
//!   a QR code).
//! - `onSas(code)`: the 6-digit code to compare with the vault's.
//! - `onState(state, reason?, detail?)`: `pairing`, `awaitingApproval`, `active`
//!   or `closed`; when closed, `reason` is vault-client's [`CloseReason`] name in
//!   camelCase (`denied`, `wrongApp`, ...) and `detail` its text.
//! - `Session`: `confirmSas()`, `cancel()`, `state()`, and Promise-returning
//!   `lock()`, `get(key)`, `put(key, value)`, `delete(key)`, `list(prefix)`,
//!   `commit()`. Keys are strings (UTF-8), values `Uint8Array`. Data calls
//!   reject at once, sending nothing, unless the session is `active`.
//!
//! Transport: vault-client's `wasm` feature, i.e. `web_sys::WebSocket` taken
//! from the global scope. Browsers and Node 22+ (global `WebSocket`) provide it.
//!
//! The bindings compile on every target (on the host they panic if called), so
//! the crate is linted and its pure parts ([`core`]) are tested natively.

pub mod core;

use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use vault_client::wasm::{WebClock, WsFactory};
use vault_client::{
    ClientError, RemoteBackend, SessionCallbacks, SessionState, SystemRng, VaultSession,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;
use zeroize::Zeroizing;

/// Rejection text of a data call outside `Active`.
const NOT_ACTIVE: &str = "vault session is not active";

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str, e: &JsValue);
}

fn js_error(msg: &str) -> JsValue {
    js_sys::Error::new(msg).into()
}

fn client_error(e: ClientError) -> JsValue {
    js_error(&e.to_string())
}

fn get(obj: &JsValue, name: &str) -> JsValue {
    Reflect::get(obj, &JsValue::from_str(name)).unwrap_or(JsValue::UNDEFINED)
}

fn set(obj: &Object, name: &str, value: &JsValue) {
    // Plain data objects created here: setting cannot fail.
    let _ = Reflect::set(obj, &JsValue::from_str(name), value);
}

fn opt_string(opts: &JsValue, name: &str) -> Result<Option<String>, JsError> {
    let v = get(opts, name);
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    v.as_string()
        .map(Some)
        .ok_or_else(|| JsError::new(&format!("pair: {name} must be a string")))
}

fn req_string(opts: &JsValue, name: &str) -> Result<String, JsError> {
    opt_string(opts, name)?.ok_or_else(|| JsError::new(&format!("pair: {name} is required")))
}

fn req_function(opts: &JsValue, name: &str) -> Result<Function, JsError> {
    get(opts, name)
        .dyn_into::<Function>()
        .map_err(|_| JsError::new(&format!("pair: {name} must be a function")))
}

/// `globalThis.location.origin`, if this runtime has one (browsers, workers).
fn location_origin() -> Option<String> {
    let loc = get(&js_sys::global(), "location");
    if loc.is_object() {
        get(&loc, "origin").as_string()
    } else {
        None
    }
}

/// Calls a host callback; an exception is logged, not propagated into the
/// session (it would otherwise end inside the session future).
fn call(name: &str, f: &Function, args: &[JsValue]) {
    let r = match args {
        [a] => f.call1(&JsValue::NULL, a),
        [a, b, c] => f.call3(&JsValue::NULL, a, b, c),
        _ => f.call0(&JsValue::NULL),
    };
    if let Err(e) = r {
        console_error(&format!("vault-web: {name} callback threw"), &e);
    }
}

fn qr_js(m: &core::QrMatrix) -> JsValue {
    let qr = Object::new();
    set(&qr, "size", &JsValue::from_f64(m.size as f64));
    set(&qr, "modules", &Uint8Array::from(&m.modules[..]).into());
    qr.into()
}

/// `onOffer`'s `qr`: the matrix, or `null` if the text does not fit a QR code.
fn qr_object(text: &str) -> JsValue {
    match core::qr_matrix(text) {
        Ok(m) => qr_js(&m),
        Err(e) => {
            console_error(&format!("vault-web: {e}"), &JsValue::UNDEFINED);
            JsValue::NULL
        }
    }
}

/// The QR module matrix of `text` (`{ size, modules }`), as `onOffer` gets it.
#[wasm_bindgen(js_name = qrMatrix)]
pub fn qr_matrix(text: &str) -> Result<JsValue, JsError> {
    let m = core::qr_matrix(text).map_err(|e| JsError::new(&e))?;
    Ok(qr_js(&m))
}

/// Start a pairing (see the crate docs for `opts`). The session runs until it
/// is `closed`; pair again with a new `pair()` call.
#[wasm_bindgen]
pub fn pair(opts: JsValue) -> Result<Session, JsError> {
    if !opts.is_object() {
        return Err(JsError::new("pair: options object required"));
    }
    let relay_origin = req_string(&opts, "relayOrigin")?;
    let app_id = req_string(&opts, "appId")?;
    let device_name = req_string(&opts, "deviceName")?;
    let test_origin = opt_string(&opts, "testOrigin")?;
    let on_offer = req_function(&opts, "onOffer")?;
    let on_sas = req_function(&opts, "onSas")?;
    let on_state = req_function(&opts, "onState")?;
    let origin = core::resolve_origin(location_origin().as_deref(), test_origin.as_deref())
        .map_err(|e| JsError::new(&format!("pair: {e}")))?;

    let callbacks = SessionCallbacks::new(
        Rc::new(move |text: &str| {
            let offer = Object::new();
            set(&offer, "text", &JsValue::from_str(text));
            set(&offer, "qr", &qr_object(text));
            call("onOffer", &on_offer, &[offer.into()]);
        }),
        Rc::new(move |sas: &str| call("onSas", &on_sas, &[JsValue::from_str(sas)])),
        Rc::new(move |s: SessionState| {
            let (reason, detail) = match &s {
                SessionState::Closed(r) => (
                    JsValue::from_str(core::reason_name(r)),
                    JsValue::from_str(&r.to_string()),
                ),
                _ => (JsValue::UNDEFINED, JsValue::UNDEFINED),
            };
            let state = JsValue::from_str(core::state_name(&s));
            call("onState", &on_state, &[state, reason, detail]);
        }),
    );
    let (session, run) = VaultSession::pair(
        WsFactory,
        WebClock,
        &SystemRng,
        &relay_origin,
        &app_id,
        &origin,
        &device_name,
        callbacks,
    )
    .map_err(|e| JsError::new(&format!("pair: {e}")))?;
    wasm_bindgen_futures::spawn_local(run);
    Ok(Session {
        inner: Rc::new(session),
    })
}

/// One vault session. Dropping it (`free()`) ends the session like `lock()`.
#[wasm_bindgen]
pub struct Session {
    inner: Rc<VaultSession>,
}

fn key_bytes(key: &JsValue, what: &str) -> Result<Vec<u8>, JsValue> {
    key.as_string()
        .map(String::into_bytes)
        .ok_or_else(|| js_error(&format!("{what} must be a string")))
}

/// A Promise that rejects with `e`.
fn rejected(e: JsValue) -> Promise {
    Promise::reject(&e)
}

#[wasm_bindgen]
impl Session {
    /// The store, or the rejection of a data call outside `Active` (checked
    /// when the call is made, before anything is sent).
    fn backend(&self) -> Result<RemoteBackend, JsValue> {
        self.inner.backend().ok_or_else(|| js_error(NOT_ACTIVE))
    }

    /// The user's "Codes match": the session goes `active` once the vault
    /// approved too. Throws before the code was shown, after `cancel()` and
    /// after `lock()`.
    #[wasm_bindgen(js_name = confirmSas)]
    pub fn confirm_sas(&self) -> Result<(), JsError> {
        self.inner
            .confirm_sas()
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// "Cancel" before `active`: the session closes as `denied`. Throws once
    /// active (use `lock()`) or closed.
    pub fn cancel(&self) -> Result<(), JsError> {
        self.inner
            .cancel_pairing()
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// End the session; resolves once it is `closed`.
    pub fn lock(&self) -> Promise {
        let s = self.inner.clone();
        future_to_promise(async move {
            s.lock().await;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// The current state name (`pairing`, `awaitingApproval`, `active`, `closed`).
    pub fn state(&self) -> String {
        core::state_name(&self.inner.state()).to_string()
    }

    /// The value at `key` (`Uint8Array`), or `undefined`.
    pub fn get(&self, key: JsValue) -> Promise {
        let (b, k) = match (self.backend(), key_bytes(&key, "key")) {
            (Ok(b), Ok(k)) => (b, k),
            (Err(e), _) | (_, Err(e)) => return rejected(e),
        };
        future_to_promise(async move {
            match b.get(&k).await.map_err(client_error)? {
                Some(v) => Ok(Uint8Array::from(&v[..]).into()),
                None => Ok(JsValue::UNDEFINED),
            }
        })
    }

    /// Buffer a write of `value` (`Uint8Array`) at `key`; durable after `commit()`.
    pub fn put(&self, key: JsValue, value: JsValue) -> Promise {
        let (b, k) = match (self.backend(), key_bytes(&key, "key")) {
            (Ok(b), Ok(k)) => (b, k),
            (Err(e), _) | (_, Err(e)) => return rejected(e),
        };
        let Some(v) = value.dyn_ref::<Uint8Array>() else {
            return rejected(js_error("value must be a Uint8Array"));
        };
        let v = Zeroizing::new(v.to_vec());
        future_to_promise(async move {
            b.put(&k, &v).await.map_err(client_error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Buffer a delete of `key`; durable after `commit()`.
    pub fn delete(&self, key: JsValue) -> Promise {
        let (b, k) = match (self.backend(), key_bytes(&key, "key")) {
            (Ok(b), Ok(k)) => (b, k),
            (Err(e), _) | (_, Err(e)) => return rejected(e),
        };
        future_to_promise(async move {
            b.delete(&k).await.map_err(client_error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Every `{ key, value }` whose key starts with `prefix`, in key order.
    /// Keys that are not UTF-8 (not written through this API) are skipped.
    pub fn list(&self, prefix: JsValue) -> Promise {
        let (b, p) = match (self.backend(), key_bytes(&prefix, "prefix")) {
            (Ok(b), Ok(p)) => (b, p),
            (Err(e), _) | (_, Err(e)) => return rejected(e),
        };
        future_to_promise(async move {
            let entries = b.scan_prefix(&p).await.map_err(client_error)?;
            let out = Array::new();
            for (k, v) in entries {
                let Ok(k) = String::from_utf8(k) else {
                    continue;
                };
                let e = Object::new();
                set(&e, "key", &JsValue::from_str(&k));
                set(&e, "value", &Uint8Array::from(&v[..]).into());
                out.push(&e);
            }
            Ok(out.into())
        })
    }

    /// Make every earlier write durable in the vault. A failed commit ends the
    /// session (`closed`, `commitFailed`) and the writes are lost.
    pub fn commit(&self) -> Promise {
        let b = match self.backend() {
            Ok(b) => b,
            Err(e) => return rejected(e),
        };
        future_to_promise(async move {
            b.flush_commit().await.map_err(client_error)?;
            Ok(JsValue::UNDEFINED)
        })
    }
}
