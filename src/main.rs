#![forbid(unsafe_code)]
// Bridbe between the org.freedesktop.ScreenSaver interface and the Wayland idle
// inhibitor protocol.

mod wayland;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
}

#[derive(Debug)]
struct OrgFreedesktopScreenSaverServer {
    inhibit_manager: InhibitorManager,
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
        application_name: String,
        reason_for_inhibit: String,
    ) -> fdo::Result<u32> {

        let inhibitor = self.inhibit_manager.create_inhibitor()
            .map_err(|e| {
                log::error!("Failed to create inhibitor: {:?}", e);
                fdo::Error::Failed(format!("Failed to create inhibitor: {:?}", e))
            })?;


        let cookie = self.insert_inhibitor(StoredInhibitor {
            inhibitor,
            name: application_name.clone(),
            reason: reason_for_inhibit.clone(),
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
        cookie: u32
    ) -> fdo::Result<()> {
        log::info!("Uninhibiting {:?}", cookie);

        let inhibitor = self.inhibitors_by_cookie.lock()
            .map_err(|e| fdo::Error::Failed(format!("Failed to insert inhibitor: {:?}", e)))?
            .remove(&cookie);

        match inhibitor {
            None => Err(fdo::Error::Failed(format!("No inhibitor with cookie {}", cookie))),
            Some(inhibitor) => match self.inhibit_manager.destroy_inhibitor(inhibitor.inhibitor) {
                Ok(_) => Ok(()),
                Err(e) => {
                    log::error!("Failed to destroy inhibitor: {:?}", e);
                    Err(fdo::Error::Failed(format!("Failed to destroy inhibitor: {:?}", e)))
                }
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    log::info!("Starting screensaver bridge");

    log::info!("Waiting for wayland compositor");
    let inhibit_manager = wayland::get_inhibit_manager().await?;

    let inhibitors_by_cookie = Arc::new(Mutex::new(HashMap::new()));
    let screen_saver = OrgFreedesktopScreenSaverServer {
        inhibit_manager,
        inhibitors_by_cookie,
    };

    log::log!(log::Level::Info, "Starting ScreenSaver to Wayland bridge");
    let _connection = zbus::connection::Builder::session()?
        .name("org.freedesktop.ScreenSaver")?
        .serve_at("/org/freedesktop/ScreenSaver", screen_saver)?
        .build().await?;

    // Run forever.
    std::future::pending::<()>().await;
    unreachable!()






}
