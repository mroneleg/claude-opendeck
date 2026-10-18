//! Desktop notifications for threshold alerts. Best effort: failures are logged.

#[cfg(any(target_os = "linux", target_os = "windows"))]
const APP_NAME: &str = "Claude Code Command Deck";

pub async fn send(title: &str, body: &str) {
    if let Err(error) = imp::send(title, body).await {
        log::warn!("desktop notification failed: {error}");
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use zbus::zvariant::Value;

    /// org.freedesktop.Notifications, which every Linux desktop provides.
    pub async fn send(title: &str, body: &str) -> anyhow::Result<()> {
        let connection = zbus::Connection::session().await?;
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        connection
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    super::APP_NAME,
                    0u32,
                    "dialog-warning",
                    title,
                    body,
                    Vec::<&str>::new(),
                    hints,
                    -1i32,
                ),
            )
            .await?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    /// AppleScript string literal.
    fn quote(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub async fn send(title: &str, body: &str) -> anyhow::Result<()> {
        let script = format!("display notification {} with title {}", quote(body), quote(title));
        let status = tokio::process::Command::new("osascript")
            .args(["-e", &script])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "osascript exited with {status}");
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod imp {
    pub async fn send(title: &str, body: &str) -> anyhow::Result<()> {
        let (title, body) = (title.to_owned(), body.to_owned());
        tokio::task::spawn_blocking(move || {
            notify_rust::Notification::new()
                .appname(super::APP_NAME)
                .summary(&title)
                .body(&body)
                .show()
                .map(|_| ())
        })
        .await??;
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    pub async fn send(_title: &str, _body: &str) -> anyhow::Result<()> {
        anyhow::bail!("not supported on this platform")
    }
}
