//! Loopback-only development automation for the native GPUI window.
//!
//! The API observes rendered geometry and can only mutate the app by dispatching
//! mouse and keyboard input. It intentionally has no access to `RootView` or to
//! gateway/business methods.

mod driver;
mod element;
pub mod protocol;
mod server;

use std::net::SocketAddr;
use std::thread::JoinHandle;

use gpui::{AnyWindowHandle, App};
use tokio::sync::mpsc;
use uuid::Uuid;

use driver::{attach_driver, DriverEnvelope};
use element::{AutomationRegistry, AutomationRegistryGlobal};

pub use element::{AutomationElementExt, AutomationRoot};
pub use protocol::AutomationRole;

pub const DEFAULT_DEV_PORT: u16 = 8765;

pub struct DevAutomation {
    address: SocketAddr,
    token: String,
    registry: AutomationRegistry,
    receiver: mpsc::UnboundedReceiver<DriverEnvelope>,
    server_thread: JoinHandle<()>,
}

impl DevAutomation {
    pub fn bind(port: u16, token: Option<String>) -> std::io::Result<Self> {
        let token = match token {
            Some(token) if !token.is_empty() => token,
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "dev automation token must not be empty",
                ));
            }
            None => Uuid::new_v4().simple().to_string(),
        };
        let binding = server::bind(port, token.clone())?;
        Ok(Self {
            address: binding.address,
            token,
            registry: binding.registry,
            receiver: binding.receiver,
            server_thread: binding.thread,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn install(&self, cx: &mut App) {
        cx.set_global(AutomationRegistryGlobal(self.registry.clone()));
    }

    pub fn attach(self, window: AnyWindowHandle, cx: &App) {
        let Self {
            registry,
            receiver,
            server_thread,
            ..
        } = self;
        attach_driver(window, registry, receiver, cx);
        // Dropping a JoinHandle detaches the loopback server. The process owns
        // its lifetime, so no business view needs to retain an automation task.
        drop(server_thread);
    }
}
