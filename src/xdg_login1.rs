use zbus_macros::proxy;
use zbus::{fdo, zvariant};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_path = "/org/freedesktop/login1",
    async_name = "Login1",
)]
trait OrgFreedesktopLogin1 {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> fdo::Result<zvariant::OwnedFd>;
}

#[derive(Debug)]
pub(crate) struct Login1Client {
    proxy: Login1<'static>,
}

impl Login1Client {
    pub async fn new() -> fdo::Result<Self> {
        let connection = zbus::Connection::system().await?;
        let proxy = Login1::new(&connection, "org.freedesktop.login1").await?;
        Ok(Self {
            proxy,
        })
    }

    pub async fn inhibit_idle(&self, who: &str, why: &str) -> fdo::Result<zvariant::OwnedFd> {
        self.proxy.inhibit("idle", who, why, "block").await
    }
}
