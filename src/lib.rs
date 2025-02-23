#![doc = include_str!("../README.md")]

use std::borrow::Borrow;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::rc::Rc;

use json_patch::Patch;
use leptos::{create_signal, ReadSignal, WriteSignal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasm_bindgen::JsValue;
use wasm_bindgen::UnwrapThrowExt;

cfg_if::cfg_if! {
    if #[cfg(all(feature = "actix", feature = "ssr"))] {
        mod actix;
        pub use crate::actix::*;
    }
}

cfg_if::cfg_if! {
    if #[cfg(all(feature = "axum", feature = "ssr"))] {
        mod axum;
        pub use crate::axum::*;
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "wasm32")] {
        trait UnwrapExt<T> {
            fn unwrap_with_error_details(self) -> T;

            fn expect_with_error_details(self, msg: &str) -> T;
        }

        impl<T, E: Debug> UnwrapExt<T> for Result<T, E> {
            fn unwrap_with_error_details(self) -> T {
                match self {
                    Ok(val) => val,
                    Err(err) => wasm_bindgen::throw_str(&format!("{:?}", err)),
                }
            }

            fn expect_with_error_details(self, msg: &str) -> T {
                match self {
                    Ok(val) => val,
                    Err(err) => wasm_bindgen::throw_str(&format!("{msg}: {:?}", err)),
                }
            }
        }
    }
}

/// A server signal update containing the signal type name and json patch.
///
/// This is whats sent over the websocket, and is used to patch the signal if the type name matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSignalUpdate {
    name: Cow<'static, str>,
    patch: Patch,
}

impl ServerSignalUpdate {
    /// Creates a new [`ServerSignalUpdate`] from an old and new instance of `T`.
    pub fn new<'s, 'e, T>(
        name: impl Into<Cow<'static, str>>,
        old: &'s T,
        new: &'e T,
    ) -> Result<Self, serde_json::Error>
    where
        T: Serialize,
    {
        let left = serde_json::to_value(old)?;
        let right = serde_json::to_value(new)?;
        let patch = json_patch::diff(&left, &right);
        Ok(ServerSignalUpdate {
            name: name.into(),
            patch,
        })
    }

    /// Creates a new [`ServerSignalUpdate`] from two json values.
    pub fn new_from_json<'s, 'e, T>(
        name: impl Into<Cow<'static, str>>,
        old: &Value,
        new: &Value,
    ) -> Self {
        let patch = json_patch::diff(old, new);
        ServerSignalUpdate {
            name: name.into(),
            patch,
        }
    }
}

/// Provides a websocket url for server signals, if there is not already one provided.
/// This ensures that you can provide it at the highest possible level, without overwriting a websocket
/// that has already been provided (for example, by a server-rendering integration.)
///
/// Note, the server should have a route to handle this websocket.
///
/// # Example
///
/// ```ignore
/// #[component]
/// pub fn App() -> impl IntoView {
///     // Provide websocket connection
///     leptos_server_signal::provide_websocket("ws://localhost:3000/ws").unwrap();
///
///     // ...
/// }
/// ```
#[allow(unused_variables)]
pub fn provide_websocket(url: &str) -> Result<(), JsValue> {
    provide_websocket_inner(url)
}

/// Creates a signal which is controlled by the server.
///
/// This signal is initialized as T::default, is read-only on the client, and is updated through json patches
/// sent through a websocket connection.
///
/// # Example
///
/// ```
/// #[derive(Clone, Default, Serialize, Deserialize)]
/// pub struct Count {
///     pub value: i32,
/// }
///
/// #[component]
/// pub fn App() -> impl IntoView {
///     // Create server signal
///     let count = create_server_signal::<Count>("counter");
///
///     view! { <h1>"Count: " {move || count().value.to_string()}</h1> }
/// }
/// ```
#[allow(unused_variables)]
pub fn create_server_signal<T>(name: impl Into<Cow<'static, str>>) -> ReadSignal<T>
where
    T: Default + Serialize + for<'de> Deserialize<'de>,
{
    let name: Cow<'static, str> = name.into();
    let (get, set) = create_signal(T::default());

    cfg_if::cfg_if! {
        if #[cfg(target_arch = "wasm32")] {
            use web_sys::MessageEvent;
            use wasm_bindgen::{prelude::Closure, JsCast};
            use leptos::{use_context, create_effect, SignalGet, SignalSet, SignalUpdate};
            use js_sys::{Function, JsString};

            let (json_get, json_set) = create_signal(serde_json::to_value(T::default()).expect_with_error_details("Failed to serialize default value"));
            if let Some(ServerSignalWebSocket {state_signals: state_signals, ..}) = use_context::<ServerSignalWebSocket>() {
                state_signals.borrow_mut().insert(name.to_string(), (json_get, json_set));

                // Note: The leptos docs advise against doing this. It seems to work
                // well in testing, and the primary caveats are around unnecessary
                // updates firing, but our state synchronization already prevents
                // that on the server side
                create_effect(move |_| {
                    let name = name.clone();
                    let new_value = serde_json::from_value(json_get.get()).expect_with_error_details("Failed to deserialize new value");
                    set.set(new_value);
                });

            } else {
                leptos::logging::error!(
                    r#"server signal was used without a websocket being provided.

Ensure you call `leptos_server_signal::provide_websocket("ws://localhost:3000/ws")` at the highest level in your app."#
                );
            }

        }
    }

    get
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "wasm32")] {
        use web_sys::WebSocket;
        use leptos::{provide_context, use_context};

        #[derive(Clone, Debug, PartialEq, Eq)]
        struct ServerSignalWebSocket {
            ws: WebSocket,
            // References to these are kept by the closure for the callback
            // onmessage callback on the websocket
            state_signals: Rc<RefCell<HashMap<String, (ReadSignal<serde_json::Value>, WriteSignal<serde_json::Value>)>>>,
            // When the websocket is first established, the leptos may not have
            // completed the traversal that sets up all of the state signals.
            // Without that, we don't have a base state to apply the patches to,
            // and therefore we must keep a record of the patches to apply after
            // the state has been set up.
            delayed_updates: Rc<RefCell<HashMap<String, Vec<Patch>>>>,
        }

        #[inline]
        fn provide_websocket_inner(url: &str) -> Result<(), JsValue> {
            use web_sys::MessageEvent;
            use wasm_bindgen::{prelude::Closure, JsCast};
            use leptos::{use_context, create_effect, SignalGetUntracked, SignalSet, SignalUpdate};
            use js_sys::{Function, JsString};

            if use_context::<ServerSignalWebSocket>().is_none() {
                let ws = WebSocket::new(url)?;
                provide_context(ServerSignalWebSocket{ws: ws, state_signals: Rc::default(), delayed_updates: Rc::default()});
            }

            let ws = use_context::<ServerSignalWebSocket>().unwrap_throw();

            let handlers = ws.state_signals.clone();
            let delayed_updates = ws.delayed_updates.clone();

            let callback = Closure::wrap(Box::new(move |event: MessageEvent| {
                let ws_string = event.data().dyn_into::<JsString>().unwrap_with_error_details().as_string().unwrap_throw();
                if let Ok(update_signal) = serde_json::from_str::<ServerSignalUpdate>(&ws_string) {
                    let handler_map = (*handlers).borrow();
                    let name = update_signal.name.borrow();
                    let mut delayed_map = (*delayed_updates).borrow_mut();
                    if let Some((json_get, json_set)) = handler_map.get::<str>(name) {
                        if let Some(delayed_patches) = delayed_map.remove(name) {
                            json_set.update(|doc| {
                                for patch in delayed_patches {
                                    json_patch::patch(doc, &patch).expect_with_error_details("Failed to patch value");
                                }
                            });
                        }
                        json_set.update(|doc| {
                            json_patch::patch(doc, &update_signal.patch).expect_with_error_details("Failed to patch value");
                        });
                    } else {
                        leptos::logging::warn!("No local state for update to {}. Queuing patch.", name);
                        delayed_map.entry(name.into()).or_default().push(update_signal.patch.clone());
                    }
                }
            }) as Box<dyn FnMut(_)>);
            let function: &Function = callback.as_ref().unchecked_ref();
            ws.ws.set_onmessage(Some(function));

            // Keep the closure alive for the lifetime of the program
            callback.forget();
            Ok(())
        }
    } else {
        #[inline]
        fn provide_websocket_inner(_url: &str) -> Result<(), JsValue> {
            Ok(())
        }
    }
}
