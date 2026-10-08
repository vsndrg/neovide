//! Releases of keys Nvim watches (`neovide.on_key_release`).
//!
//! Nvim only gets key presses. For a watched key, Neovide reports when it goes up and how long it
//! was held, through the same serial queue as key input: Nvim has the press and its repeats before
//! the release.

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use winit::{
    event::{ElementState, WindowEvent},
    keyboard::PhysicalKey,
};

use crate::bridge::{NeovimHandler, SerialCommand, send_ui};

#[derive(Debug, Default)]
pub struct KeyReleases {
    /// Keys as Neovide sends them to Nvim, e.g. `<C-m>`.
    watched: HashSet<String>,
    /// Watched keys that are down: the key sent for the press and when it went down.
    down: HashMap<PhysicalKey, (String, Instant)>,
}

impl KeyReleases {
    pub fn new(watched: HashSet<String>) -> Self {
        Self { watched, down: HashMap::new() }
    }

    pub fn watch(&mut self, keys: HashSet<String>) {
        self.watched = keys;
        self.down.retain(|_, (key, _)| self.watched.contains(key));
    }

    /// `sent` is the key the event sent to Nvim, if any.
    pub fn handle_event(
        &mut self,
        event: &WindowEvent,
        sent: Option<&str>,
        neovim_handler: &NeovimHandler,
    ) {
        match event {
            WindowEvent::KeyboardInput { event: key_event, is_synthetic: false, .. } => {
                match key_event.state {
                    ElementState::Pressed if !key_event.repeat => {
                        if let Some(key) = sent.filter(|key| self.watched.contains(*key)) {
                            self.down
                                .insert(key_event.physical_key, (key.to_owned(), Instant::now()));
                        }
                    }
                    ElementState::Released => {
                        if let Some((key, since)) = self.down.remove(&key_event.physical_key) {
                            report(key, since, neovim_handler);
                        }
                    }
                    ElementState::Pressed => {}
                }
            }
            // The release would go to another app.
            WindowEvent::Focused(false) => {
                for (_, (key, since)) in self.down.drain() {
                    report(key, since, neovim_handler);
                }
            }
            _ => {}
        }
    }
}

fn report(key: String, since: Instant, neovim_handler: &NeovimHandler) {
    let held_ms = since.elapsed().as_millis() as u64;
    send_ui(SerialCommand::KeyReleased { key, held_ms }, neovim_handler);
}
