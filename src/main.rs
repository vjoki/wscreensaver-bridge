#![forbid(unsafe_code)]
// Bridbe between the org.freedesktop.ScreenSaver interface and the Wayland idle
// inhibitor protocol.
mod wayland;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::time::{self, Duration};
use anyhow::Context as _;
use zbus::message::Header;
use zbus::names::UniqueName;
use zbus::fdo;
use zbus_macros::interface;
use wayland::InhibitorManager;
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;

// TODO: add a way to list inhibitors
#[allow(dead_code)]
#[derive(Debug)]
struct StoredInhibitor {
    inhibitor: ZwpIdleInhibitorV1,
    name: String,
    reason: String,
    sender: UniqueName<'static>,
}

#[derive(Debug)]
struct OrgFreedesktopScreenSaverServer {
    inhibit_manager: Arc<InhibitorManager>,
    // NOTE: Must not be held across await points.
    inhibitors_by_cookie: Arc<Mutex<HashMap<u32, StoredInhibitor>>>,
}

impl OrgFreedesktopScreenSaverServer {
    fn insert_inhibitor(&self, inhibitor: StoredInhibitor) -> Result<u32, String> {
        // find an insert a new cookie. we're locked so this should be gucci
        let cookie = loop {
            let cookie = fastrand::u32(..);
            if !self.inhibitors_by_cookie
                .lock().map_err(|e| format!("{:?}", e))?
                .contains_key(&cookie)
            {
                break cookie;
            }
        };
        self.inhibitors_by_cookie
            .lock().map_err(|e| format!("{:?}", e))?
            .insert(cookie, inhibitor);
        Ok(cookie)
    }
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl OrgFreedesktopScreenSaverServer {
    async fn inhibit(
        &self,
        #[zbus(header)]
        hdr: Header<'_>,
        application_name: String,
        reason_for_inhibit: String,
    ) -> fdo::Result<u32> {
        let Some(sender) = hdr.sender().map(|x| x.to_owned()) else {
            log::error!("No sender provided");
            return Err(fdo::Error::Failed("No sender provided".to_string()));
        };

        let inhibitor = self.inhibit_manager.create_inhibitor()
            .map_err(|e| {
                log::error!("Failed to create inhibitor: {:?}", e);
                fdo::Error::Failed(format!("Failed to create inhibitor: {:?}", e))
            })?;


        let cookie = self.insert_inhibitor(StoredInhibitor {
            inhibitor,
            name: application_name.clone(),
            reason: reason_for_inhibit.clone(),
            sender,
        }).map_err(|e| fdo::Error::Failed(format!("Failed to insert inhibitor: {}", e)))?;
        log::info!(
            "Inhibiting screensaver for {:?} because {:?}. Inhibitor cookie is {:?}.",
            application_name,
            reason_for_inhibit,
            cookie,
        );

        Ok(cookie)
    }

    async fn un_inhibit(
        &self,
        #[zbus(header)]
        hdr: Header<'_>,
        cookie: u32
    ) -> fdo::Result<()> {
        log::info!("Uninhibiting {:?}", cookie);
        let Some(sender) = hdr.sender().map(|x| x.to_owned()) else {
            log::error!("No sender provided");
            return Err(fdo::Error::Failed("No sender provided".to_string()));
        };

        let mut inhibitors_by_cookie = self.inhibitors_by_cookie.lock()
            .map_err(|e| fdo::Error::Failed(format!("Failed to insert inhibitor: {:?}", e)))?;
        match inhibitors_by_cookie.entry(cookie) {
            std::collections::hash_map::Entry::Occupied(e) => if e.get().sender.as_ref() == sender {
                let inhibitor = e.remove();
                match self.inhibit_manager.destroy_inhibitor(inhibitor.inhibitor) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        log::error!("Failed to destroy inhibitor: {:?}", e);
                        Err(fdo::Error::Failed(format!("Failed to destroy inhibitor: {:?}", e)))
                    }
                }
            } else {
                Err(fdo::Error::Failed(format!("Sender does not match for cookie {}, '{:?}' != '{:?}'",
                                               cookie, e.get().sender, hdr.sender())))
            },
            std::collections::hash_map::Entry::Vacant(_) => {
                Err(fdo::Error::Failed(format!("No inhibitor with cookie {}", cookie)))
            },
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
pub async fn main() -> anyhow::Result<()> {
    // configure logger to print thread id
    let mut log_builder = pretty_env_logger::formatted_builder();
    log_builder.format(|buf, record| {
        use std::io::Write;
        writeln!(
            buf,
            "[{:?}][{}] {}",
            std::thread::current().id(),
            record.level(),
            record.args()
        )
    });
    log_builder.filter_level(log::LevelFilter::Info);
    log_builder.init();

    let (terminator_tx, mut terminator_rx) = watch::channel(false);
    let heartbeat_terminator = terminator_tx.subscribe();
    let terminator = terminator_tx.clone();
    ctrlc::set_handler(move || {
        if let Err(e) = terminator.send(true) {
            log::error!("Sending termination signal failed: {:?}", e);
        }
    }).context("signal handler")?;

    log::info!("Starting screensaver bridge");

    log::info!("Waiting for wayland compositor");
    let inhibit_manager = Arc::new(wayland::get_inhibit_manager().await?);

    let inhibitors_by_cookie = Arc::new(Mutex::new(HashMap::new()));
    let screen_saver = OrgFreedesktopScreenSaverServer {
        inhibit_manager: inhibit_manager.clone(),
        inhibitors_by_cookie: inhibitors_by_cookie.clone(),
    };

    log::log!(log::Level::Info, "Starting ScreenSaver to Wayland bridge");
    let connection = zbus::connection::Builder::session()?
        .name("org.freedesktop.ScreenSaver")?
        .serve_at("/org/freedesktop/ScreenSaver", screen_saver)?
        .build().await?;

    let inhibit_manager_ref = inhibit_manager.clone();
    let inhibitors_ref = inhibitors_by_cookie.clone();
    let connection_ref = connection.clone();
    let heartbeat_handle = tokio::spawn(async move {
        heartbeat(heartbeat_terminator, inhibit_manager_ref, inhibitors_ref, connection_ref).await
    });

    // Run until SIGTERM/SIGHUP/SIGINT
    terminator_rx.changed().await?;

    // Clean up inhibitor heartbeat.
    heartbeat_handle.await??;

    log::info!("Stopping screensaver bridge, cleaning up any left over inhibitors...");
    // This should also close the ObjectServer? We don't want to accept any new inhibitors no more.
    if let Err(e) = connection.close().await {
        log::error!("Error closing D-Bus connection: {:?}", e);
    }

    let mut inhibitors = inhibitors_by_cookie.lock()
        .expect("Could not obtain lock on inhibitors map for clean up");

    for (cookie, inhibitor) in inhibitors.drain() {
        log::info!("Uninhibiting {:?}", cookie);
        match inhibit_manager.destroy_inhibitor(inhibitor.inhibitor.clone()) {
            Ok(_) => (),
            Err(e) => {
                log::error!("Failed to destroy inhibitor: {:?}", e);
            }
        }
    }

    Ok(())
}

// Shamelessly copied from https://github.com/bdwalton/inhibit-bridge, try to make sure we don't leave any
// stale inhibitors active.
async fn heartbeat(
    mut terminator: watch::Receiver<bool>,
    inhibit_manager: Arc<InhibitorManager>,
    inhibitors_by_cookie: Arc<Mutex<HashMap<u32, StoredInhibitor>>>,
    connection: zbus::Connection
) -> anyhow::Result<()> {
    log::info!("Starting inhibitor heartbeat poller");
    let mut interval = time::interval(Duration::from_secs(10));

    let proxy = fdo::DBusProxy::new(&connection).await?;
    loop {
        tokio::select! {
            biased;
            _ = terminator.changed() => {
                break
            }
            _ = interval.tick() => if inhibitors_by_cookie.try_lock().is_ok_and(|xs| !xs.is_empty()) {
                let names: HashSet<UniqueName<'static>> = proxy.list_names().await?
                    .into_iter()
                    .filter_map(|x| match x.into_inner() {
                        zbus::names::BusName::Unique(x) => Some(x),
                        _ => None,
                    })
                    .collect();

                match inhibitors_by_cookie.try_lock() {
                    Ok(mut inhibitors) => {
                        inhibitors.retain(|cookie, inhibitor| {
                            if names.contains(&inhibitor.sender) {
                                true
                            } else {
                                log::info!("Missing sender '{}', uninhibiting {:?}", inhibitor.sender, cookie);
                                match inhibit_manager.destroy_inhibitor(inhibitor.inhibitor.clone()) {
                                    Ok(_) => false,
                                    Err(e) => {
                                        log::error!("Failed to destroy inhibitor: {:?}", e);
                                        false
                                    }
                                }
                            }
                        });
                    },
                    Err(std::sync::TryLockError::WouldBlock) => {
                        log::debug!("Inhibitors map already locked, trying again later...");
                        continue
                    },
                    Err(e) => {
                        log::error!("Terminating heartbeat checker: {:?}", e);
                        anyhow::bail!(format!("Inhibitors map lock error: {:?}", e))
                    },
                };
            }
        }
    }

    log::info!("Stopping inhibitor heartbeat poller");
    Ok(())
}
