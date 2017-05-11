// Copyright 2017 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `PluginManager` handles launching, monitoring, and communicating with plugins.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex, Weak, MutexGuard};

use serde_json::{self, Value};

use tabs::{BufferIdentifier, ViewIdentifier, BufferContainerRef};

use super::{PluginDescription, PluginRef};
use super::rpc_types::{PluginCommand, PluginUpdate, UpdateResponse};
use super::manifest::{PluginActivation, debug_plugins};

type PluginName = String;

/// Manages plugin loading, activation, lifecycle, and dispatch.
pub struct PluginManager<W: Write> {
    catalog: Vec<PluginDescription>,
    running: BTreeMap<(BufferIdentifier, PluginName), PluginRef<W>>,
    buffers: BufferContainerRef<W>,
}

// Note: In general I have adopted the pattern of putting non-threadsafe
// API in the 'inner' type of types with a ___Ref variant. Methods on the
// Ref variant should be minimal, and provide a threadsafe interface.

impl <W: Write + Send + 'static>PluginManager<W> {

    /// Returns available plugins, for populating the client menu.
    pub fn debug_available_plugins(&self) -> Vec<&str> {
        self.catalog.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>()
    }

    /// Handle a request from a plugin.
    pub fn handle_plugin_cmd(&self, cmd: PluginCommand, view_id: &ViewIdentifier) -> Option<Value> {
        use self::PluginCommand::*;
        match cmd {
            LineCount => {
                let n_lines = self.buffers.lock().editor_for_view(view_id).unwrap()
                    .plugin_n_lines() as u64;
                Some(serde_json::to_value(n_lines).unwrap())
            },
            SetFgSpans { start, len, spans, rev } => {
                self.buffers.lock().editor_for_view_mut(view_id).unwrap()
                    .plugin_set_fg_spans(start, len, spans, rev);
                None
            }
            GetData { offset, max_size, rev } => {
                self.buffers.lock().editor_for_view(view_id).unwrap()
                .plugin_get_data(offset, max_size, rev)
                .map(|data| Value::String(data))
            }
            Alert { msg } => {
                self.buffers.lock().editor_for_view(view_id).unwrap()
                .plugin_alert(&msg);
                None
            }
        }
    }

    /// Passes an update from a buffer to all registered plugins.
    pub fn update(&mut self, view_id: &ViewIdentifier, update: PluginUpdate,
                  undo_group: usize) {
        // find all running plugins for this buffer, and send them the update
        for (_, plugin) in self.running.iter().filter(|kv| (kv.0).0 == *view_id) {
            self.buffers.lock().editor_for_view_mut(view_id)
                .unwrap().increment_revs_in_flight();
            let view_id = view_id.to_owned();
            let buffers = self.buffers.clone().to_weak();
            plugin.update(&update, move |response| {
                if let Some(buffers) = buffers.upgrade() {
                    let response = response.expect("bad plugin response");
                    match serde_json::from_value::<UpdateResponse>(response) {
                        Ok(UpdateResponse::Edit(edit)) => {
                            buffers.lock().editor_for_view_mut(&view_id).unwrap()
                                .apply_plugin_edit(edit, undo_group);
                        }
                        Ok(UpdateResponse::Ack(_)) => (),
                        Err(err) => { print_err!("plugin response json err: {:?}", err); }
                    }
                    buffers.lock().editor_for_view_mut(&view_id)
                        .unwrap().dec_revs_in_flight();
                }
            })
        }
        self.buffers.lock().editor_for_view_mut(view_id).unwrap().dec_revs_in_flight();
    }

    /// Launches and initializes the named plugin.
    fn start_plugin(&mut self, self_ref: &PluginManagerRef<W>,
                        view_id: &ViewIdentifier, plugin_name: &str, buf_size: usize, rev: usize) {
        //TODO: error handling: this should maybe have a completion callback with a Result
        let key = (view_id.to_owned(), plugin_name.to_owned());
        if self.running.contains_key(&key) {
            return print_err!("plugin {} already running for buffer {}", plugin_name, view_id);
        }

        let plugin = self.catalog.iter().find(|desc| desc.name == plugin_name)
            .expect(&format!("no plugin found with name {}", plugin_name));

        let me = self_ref.clone();
        plugin.launch(self_ref, view_id, move |result| {
            match result {
                Ok(plugin_ref) => {
                    plugin_ref.init_buf(buf_size, rev);
                    let mut inner = me.lock();
                    inner.buffers.lock().editor_for_view(&key.0).unwrap().plugin_started(&key.0, &key.1);
                    inner.running.insert(key, plugin_ref);
                },
                Err(_) => panic!("error handling is not implemented"),
            }
        });
    }

    fn stop_plugin(&mut self, view_id: &ViewIdentifier, plugin_name: &str) {
        let key = (view_id.to_owned(), plugin_name.to_owned());
        match self.running.remove(&key) {
            Some(plugin) => {
                plugin.shutdown();
                //TODO: should we notify now, or wait until we know this worked?
                //can this fail? (yes.) How do we tell, and when do we kill the proc?
                self.buffers.lock().editor_for_view(view_id)
                    .unwrap().plugin_stopped(view_id, plugin_name, 0);
            }
            None => print_err!("stop_plugin: plugin not running for buffer {} {}",
                               plugin_name, view_id),
        }
    }
}

/// Wrapper around a `Arc<Mutex<PluginManager<W>>>`.
pub struct PluginManagerRef<W: Write>(Arc<Mutex<PluginManager<W>>>);

impl<W: Write> Clone for PluginManagerRef<W> {
    fn clone(&self) -> Self {
        PluginManagerRef(self.0.clone())
    }
}

/// Wrapper around a `Weak<Mutex<PluginManager<W>>>`
pub struct WeakPluginManagerRef<W: Write>(Weak<Mutex<PluginManager<W>>>);

impl <W: Write>WeakPluginManagerRef<W> {
    /// Upgrades the weak reference to an Arc, if possible.
    ///
    /// Returns `None` if the inner value has been deallocated.
    pub fn upgrade(&self) -> Option<PluginManagerRef<W>> {
        match self.0.upgrade() {
            Some(inner) => Some(PluginManagerRef(inner)),
            None => None
        }
    }
}

impl<W: Write + Send + 'static> PluginManagerRef<W> {
    pub fn new(buffers: BufferContainerRef<W>) -> Self {
        PluginManagerRef(Arc::new(Mutex::new(
        PluginManager {
            // TODO: actually parse these from manifest files
            catalog: debug_plugins(),
            running: BTreeMap::new(),
            buffers: buffers,
        })))
    }

    pub fn lock(&self) -> MutexGuard<PluginManager<W>> {
        self.0.lock().unwrap()
    }

    /// Creates a new `WeakPluginManagerRef<W>`.
    pub fn to_weak(&self) -> WeakPluginManagerRef<W> {
        let weak_inner = Arc::downgrade(&self.0);
        WeakPluginManagerRef(weak_inner)
    }


    /// Called when a new empty buffer is created.
    pub fn document_new(&mut self, view_id: &ViewIdentifier) {
        print_err!("document new {}", view_id);
        self.start_initial_plugins(view_id);
        //TODO: send lifecycle notification
    }

    /// Called when an existing file is loaded into a buffer.
    pub fn document_open(&mut self, view_id: &ViewIdentifier) {
        print_err!("document open {}", view_id);
        self.start_initial_plugins(view_id);
        //TODO: send lifecycle notification
    }

    /// Called when a buffer is saved to a file.
    pub fn document_did_save(&mut self, view_id: &ViewIdentifier) {
        print_err!("document_did_save {}", view_id);
    }

    /// Called when a buffer is closed.
    pub fn document_close(&mut self, view_id: &ViewIdentifier) {
        print_err!("document_close {}", view_id);
    }

    /// Called when a document's syntax definition has changed.
    pub fn document_syntax_changed(&mut self, view_id: &ViewIdentifier) {
        print_err!("document_syntax_changed {}", view_id);
    }

    /// Launches and initializes the named plugin.
    pub fn start_plugin(&self, view_id: &ViewIdentifier, plugin_name: &str,
                        buf_size: usize, rev: usize) {
        self.lock().start_plugin(self, view_id, plugin_name, buf_size, rev);
    }

    /// Terminates and cleans up the named plugin.
    pub fn stop_plugin(&self, view_id: &ViewIdentifier, plugin_name: &str) {
        self.lock().stop_plugin(view_id, plugin_name);
    }

    // ====================================================================
    // implementation details
    // ====================================================================

    /// Called once each time a buffer is created.
    fn start_initial_plugins(&mut self, view_id: &ViewIdentifier) {
        print_err!("starting initial plugins for {}", view_id);
        let to_start = {
        let inner = self.lock();
        let syntax = inner.buffers.lock()
            .editor_for_view(view_id).unwrap().get_syntax().to_owned();

        inner.catalog.iter()
            .filter(|plug_desc|{
                plug_desc.activations.iter().any(|act|{
                    match *act {
                        PluginActivation::Autorun => true,
                        PluginActivation::OnSyntax(ref on_syntax) if *on_syntax == syntax => true,
                        _ => false,
                    }
                })
            })
            .map(|desc| desc.name.to_owned())
            .collect::<Vec<_>>()
        };

        let (buf_size, _, rev) = {
            let inner = self.lock();
            let params = inner.buffers.lock().editor_for_view(&view_id).unwrap()
                .plugin_init_params();
            params
        };

        for plugin_name in to_start.iter() {
            print_err!("starting plugin {}", plugin_name);
            self.start_plugin(view_id, plugin_name, buf_size, rev);
        }
    }
}

