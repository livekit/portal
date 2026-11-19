// Copyright 2026 LiveKit, Inc.
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

use log::{Level, LevelFilter, Log, Metadata, Record};
use wasm_bindgen::prelude::*;
use web_sys::console;

struct ConsoleLogger;

static LOGGER: ConsoleLogger = ConsoleLogger;

impl Log for ConsoleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = JsValue::from_str(&format!("[livekit-portal] {}", record.args()));
        match record.level() {
            Level::Error => console::error_1(&line),
            Level::Warn => console::warn_1(&line),
            Level::Info => console::info_1(&line),
            Level::Debug | Level::Trace => console::debug_1(&line),
        }
    }

    fn flush(&self) {}
}

#[wasm_bindgen(start)]
fn start() {
    // A panic aborts the module; say why before it does.
    std::panic::set_hook(Box::new(|info| console::error_1(&info.to_string().into())));
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Warn);
    }
}

/// Route the core's logs at or above `level` ("error", "warn", "info",
/// "debug", "trace" or "off") to the console. Defaults to "warn".
#[wasm_bindgen(js_name = setLogLevel)]
pub fn set_log_level(level: &str) -> Result<(), JsError> {
    let filter = level
        .parse::<LevelFilter>()
        .map_err(|_| JsError::new(&format!("unknown log level '{level}'")))?;
    log::set_max_level(filter);
    Ok(())
}
