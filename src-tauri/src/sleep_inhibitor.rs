use anyhow::{Context, Result};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use dbus::arg::{PropMap, Variant};
    use dbus::blocking::{Connection, Proxy};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::dpms::{ConnectionExt as DpmsConnectionExt, DPMSMode};
    use x11rb::protocol::xproto::{ConnectionExt as XprotoConnectionExt, ScreenSaver};
    use x11rb::rust_connection::RustConnection;

    const DBUS_TIMEOUT: Duration = Duration::from_secs(2);
    const X11_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
    const PORTAL_INHIBIT_IDLE_FLAG: u32 = 8;
    const GNOME_INHIBIT_IDLE_FLAG: u32 = 8;

    pub struct SleepInhibitor {
        app_name: &'static str,
        reason: &'static str,
        session: Mutex<Option<Session>>,
    }

    enum Session {
        DesktopPortal {
            _connection: Connection,
            x11_keepalive: Option<X11KeepAlive>,
        },
        ScreenSaver {
            connection: Connection,
            cookie: u32,
            x11_keepalive: Option<X11KeepAlive>,
        },
        GnomeSession {
            connection: Connection,
            cookie: u32,
            x11_keepalive: Option<X11KeepAlive>,
        },
    }

    struct X11KeepAlive {
        stop_tx: mpsc::Sender<()>,
        join_handle: JoinHandle<()>,
    }

    impl SleepInhibitor {
        pub fn new(app_name: &'static str, reason: &'static str) -> Self {
            Self {
                app_name,
                reason,
                session: Mutex::new(None),
            }
        }

        pub fn set_active(&self, active: bool) -> Result<()> {
            if active {
                self.activate()
            } else {
                self.deactivate()
            }
        }

        fn activate(&self) -> Result<()> {
            let mut session = self
                .session
                .lock()
                .map_err(|_| anyhow::anyhow!("Sleep inhibitor lock poisoned"))?;
            if session.is_some() {
                return Ok(());
            }

            *session = Some(
                Session::acquire_portal(self.reason)
                    .or_else(|portal_error| {
                        tracing::warn!(
                            error = %portal_error,
                            "Portal inhibit failed; trying ScreenSaver API"
                        );
                        Session::acquire_screensaver(self.app_name, self.reason)
                    })
                    .or_else(|screen_saver_error| {
                        tracing::warn!(
                            error = %screen_saver_error,
                            "ScreenSaver inhibit failed; trying GNOME session manager"
                        );
                        Session::acquire_gnome(self.app_name, self.reason)
                    })?,
            );
            Ok(())
        }

        fn deactivate(&self) -> Result<()> {
            let session = {
                let mut guard = self
                    .session
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Sleep inhibitor lock poisoned"))?;
                guard.take()
            };

            if let Some(session) = session {
                session.release()?;
            }
            Ok(())
        }
    }

    impl Drop for SleepInhibitor {
        fn drop(&mut self) {
            if let Err(error) = self.deactivate() {
                tracing::warn!(error = %error, "Failed to release sleep inhibitor during shutdown");
            }
        }
    }

    impl Session {
        fn acquire_portal(reason: &str) -> Result<Self> {
            let connection =
                Connection::new_session().context("Failed to connect to session D-Bus")?;
            let proxy = Proxy::new(
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                DBUS_TIMEOUT,
                &connection,
            );
            let mut options: PropMap = HashMap::new();
            options.insert("reason".to_string(), Variant(Box::new(reason.to_string())));
            let (handle,): (dbus::Path<'static>,) = proxy
                .method_call(
                    "org.freedesktop.portal.Inhibit",
                    "Inhibit",
                    ("", PORTAL_INHIBIT_IDLE_FLAG, options),
                )
                .context("org.freedesktop.portal.Inhibit.Inhibit failed")?;
            tracing::info!(handle = %handle, "Acquired desktop portal inhibit");
            let x11_keepalive = X11KeepAlive::spawn_if_available();
            Ok(Self::DesktopPortal {
                _connection: connection,
                x11_keepalive,
            })
        }

        fn acquire_screensaver(app_name: &str, reason: &str) -> Result<Self> {
            let connection =
                Connection::new_session().context("Failed to connect to session D-Bus")?;
            let proxy = Proxy::new(
                "org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
                DBUS_TIMEOUT,
                &connection,
            );
            let (cookie,): (u32,) = proxy
                .method_call("org.freedesktop.ScreenSaver", "Inhibit", (app_name, reason))
                .context("org.freedesktop.ScreenSaver.Inhibit failed")?;
            tracing::info!(cookie, "Acquired ScreenSaver inhibit");
            let x11_keepalive = X11KeepAlive::spawn_if_available();
            Ok(Self::ScreenSaver {
                connection,
                cookie,
                x11_keepalive,
            })
        }

        fn acquire_gnome(app_name: &str, reason: &str) -> Result<Self> {
            let connection =
                Connection::new_session().context("Failed to connect to session D-Bus")?;
            let proxy = Proxy::new(
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                DBUS_TIMEOUT,
                &connection,
            );
            let (cookie,): (u32,) = proxy
                .method_call(
                    "org.gnome.SessionManager",
                    "Inhibit",
                    (app_name, 0u32, reason, GNOME_INHIBIT_IDLE_FLAG),
                )
                .context("org.gnome.SessionManager.Inhibit failed")?;
            tracing::info!(cookie, "Acquired GNOME session inhibit");
            let x11_keepalive = X11KeepAlive::spawn_if_available();
            Ok(Self::GnomeSession {
                connection,
                cookie,
                x11_keepalive,
            })
        }

        fn release(self) -> Result<()> {
            match self {
                Session::DesktopPortal { x11_keepalive, .. } => {
                    stop_x11_keepalive(x11_keepalive);
                    tracing::info!("Released desktop portal inhibit");
                }
                Session::ScreenSaver {
                    connection,
                    cookie,
                    x11_keepalive,
                } => {
                    stop_x11_keepalive(x11_keepalive);
                    let proxy = Proxy::new(
                        "org.freedesktop.ScreenSaver",
                        "/org/freedesktop/ScreenSaver",
                        DBUS_TIMEOUT,
                        &connection,
                    );
                    proxy
                        .method_call::<(), _, _, _>(
                            "org.freedesktop.ScreenSaver",
                            "UnInhibit",
                            (cookie,),
                        )
                        .context("org.freedesktop.ScreenSaver.UnInhibit failed")?;
                    tracing::info!(cookie, "Released ScreenSaver inhibit");
                }
                Session::GnomeSession {
                    connection,
                    cookie,
                    x11_keepalive,
                } => {
                    stop_x11_keepalive(x11_keepalive);
                    let proxy = Proxy::new(
                        "org.gnome.SessionManager",
                        "/org/gnome/SessionManager",
                        DBUS_TIMEOUT,
                        &connection,
                    );
                    proxy
                        .method_call::<(), _, _, _>(
                            "org.gnome.SessionManager",
                            "Uninhibit",
                            (cookie,),
                        )
                        .context("org.gnome.SessionManager.Uninhibit failed")?;
                    tracing::info!(cookie, "Released GNOME session inhibit");
                }
            }
            Ok(())
        }
    }

    impl X11KeepAlive {
        fn spawn_if_available() -> Option<Self> {
            if std::env::var_os("DISPLAY").is_none() {
                return None;
            }

            let (stop_tx, stop_rx) = mpsc::channel();
            let join_handle = thread::Builder::new()
                .name("x11-keepalive".to_string())
                .spawn(move || run_x11_keepalive(stop_rx))
                .ok()?;
            Some(Self {
                stop_tx,
                join_handle,
            })
        }
    }

    fn stop_x11_keepalive(x11_keepalive: Option<X11KeepAlive>) {
        if let Some(x11_keepalive) = x11_keepalive {
            let _ = x11_keepalive.stop_tx.send(());
            if let Err(error) = x11_keepalive.join_handle.join() {
                tracing::warn!(?error, "Failed to join X11 keepalive thread");
            }
        }
    }

    fn run_x11_keepalive(stop_rx: mpsc::Receiver<()>) {
        if let Err(error) = poke_x11_display() {
            tracing::warn!(error = %error, "Initial X11 keepalive poke failed");
        }

        loop {
            match stop_rx.recv_timeout(X11_KEEPALIVE_INTERVAL) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = poke_x11_display() {
                        tracing::warn!(error = %error, "X11 keepalive poke failed");
                    }
                }
            }
        }
    }

    fn poke_x11_display() -> Result<()> {
        let (connection, _) = x11rb::connect(None).context("Failed to connect to X11 display")?;
        reset_screen_saver(&connection)?;
        force_dpms_on(&connection)?;
        connection
            .flush()
            .context("Failed to flush X11 keepalive requests")?;
        tracing::debug!("Sent X11 screen saver / DPMS keepalive");
        Ok(())
    }

    fn reset_screen_saver(connection: &RustConnection) -> Result<()> {
        connection
            .force_screen_saver(ScreenSaver::RESET)?
            .check()
            .context("X11 ForceScreenSaver(Reset) failed")?;
        Ok(())
    }

    fn force_dpms_on(connection: &RustConnection) -> Result<()> {
        let dpms_capable = connection
            .dpms_capable()?
            .reply()
            .context("X11 DPMSCapable reply failed")?
            .capable;
        if !dpms_capable {
            return Ok(());
        }

        connection
            .dpms_force_level(DPMSMode::ON)?
            .check()
            .context("X11 DPMS ForceLevel(On) failed")?;
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub struct SleepInhibitor;

    impl SleepInhibitor {
        pub fn new(_app_name: &'static str, _reason: &'static str) -> Self {
            Self
        }

        pub fn set_active(&self, _active: bool) -> Result<()> {
            Ok(())
        }
    }
}

pub use imp::SleepInhibitor;
