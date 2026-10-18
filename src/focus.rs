//! Raising the window a Claude Code session runs in.
//!
//! Linux (KDE Plasma, Wayland or X11): a one-shot KWin script loaded over
//! D-Bus finds the window owned by one of the session's processes. Several
//! windows can share a pid (every VS Code window is one process), so it prefers
//! the one whose title names the project. Other platforms aren't supported yet.

/// The KWin script for `pids` (nearest ancestor first) and `project`.
pub fn kwin_script(pids: &[u32], project: &str) -> String {
    format!(
        r#"
const pids = {pids};
const project = {project};
const candidates = workspace.windowList().filter((w) => w.normalWindow && pids.includes(w.pid));
// Nearest ancestor first: pids are ordered from the session outwards.
candidates.sort((a, b) => pids.indexOf(a.pid) - pids.indexOf(b.pid));
// Titles are "<file> - <folder> - Visual Studio Code" and the like: prefer a
// window whose folder part is exactly the project (ignoring " [SSH: host]"),
// so "app" doesn't match "app-v2"; fall back to a substring match.
const segments = (caption) => caption.split(/ [-—] /).map((part) => part.replace(/\s*\[[^\]]*\]$/, '').trim());
const target = candidates.find((w) => project && segments(w.caption).includes(project))
  || candidates.find((w) => project && w.caption.includes(project))
  || candidates[0];
if (target) {{
  if (target.minimized) target.minimized = false;
  workspace.activeWindow = target;
}}
// Shows up in `journalctl --user` for troubleshooting.
print("claude-opendeck focus: " + (target ? target.pid + " " + target.caption : "no window for pids " + pids.join(",")));
"#,
        pids = serde_json::to_string(pids).unwrap_or_else(|_| "[]".into()),
        project = serde_json::to_string(project).unwrap_or_else(|_| "\"\"".into()),
    )
}

#[cfg(target_os = "linux")]
pub async fn focus_window(pids: &[u32], project: &str) -> anyhow::Result<()> {
    use zbus::Connection;

    let name = format!(
        "claude-opendeck-focus-{}-{}",
        std::process::id(),
        crate::paths::now_ms()
    );
    let file = std::env::temp_dir().join(format!("{name}.js"));
    tokio::fs::write(&file, kwin_script(pids, project)).await?;

    let result = async {
        let connection = Connection::session().await?;
        let reply = connection
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(file.to_string_lossy().as_ref(), name.as_str()),
            )
            .await?;
        let id: i32 = reply.body().deserialize()?;
        anyhow::ensure!(id >= 0, "KWin rejected the focus script");
        connection
            .call_method(
                Some("org.kde.KWin"),
                format!("/Scripting/Script{id}").as_str(),
                Some("org.kde.kwin.Script"),
                "run",
                &(),
            )
            .await?;
        anyhow::Ok(connection)
    }
    .await;

    // Give KWin a moment to run it before unloading and cleaning up.
    let cleanup_connection = result.as_ref().ok().cloned();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Some(connection) = cleanup_connection {
            let _ = connection
                .call_method(
                    Some("org.kde.KWin"),
                    "/Scripting",
                    Some("org.kde.kwin.Scripting"),
                    "unloadScript",
                    &(name.as_str(),),
                )
                .await;
        }
        let _ = tokio::fs::remove_file(&file).await;
    });
    result.map(|_| ())
}

#[cfg(not(target_os = "linux"))]
pub async fn focus_window(_pids: &[u32], _project: &str) -> anyhow::Result<()> {
    anyhow::bail!("focusing windows isn't supported on this platform yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the generated script against fake windows with Node, if available.
    fn pick(captions: &[&str], project: &str) -> Option<String> {
        let windows: Vec<_> = captions
            .iter()
            .map(|c| serde_json::json!({"pid": 10, "caption": c, "normalWindow": true, "minimized": false}))
            .collect();
        let harness = format!(
            "let picked = null; const workspace = {{ windowList: () => {}, set activeWindow(w) {{ picked = w.caption; }} }};\n\
             const print = () => {{}};\n{}\nconsole.log(JSON.stringify(picked));",
            serde_json::to_string(&windows).unwrap(),
            kwin_script(&[999, 10], project)
        );
        let output = std::process::Command::new("node")
            .args(["-e", &harness])
            .output()
            .ok()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).ok()
    }

    #[test]
    fn prefers_exact_folder_segment() {
        let captions = [
            "main.ts - app-v2 - Visual Studio Code",
            "index.ts - app - Visual Studio Code",
            "brent-reqs.md - eaie-process [SSH: datasite-laptop] - Visual Studio Code",
            "Claude Code - sandvico - Visual Studio Code",
        ];
        let Some(app) = pick(&captions, "app") else {
            eprintln!("node not available; skipping KWin script test");
            return;
        };
        assert_eq!(app, "index.ts - app - Visual Studio Code");
        assert_eq!(pick(&captions, "eaie-process").unwrap(), captions[2]);
        assert_eq!(pick(&captions, "sandvico").unwrap(), captions[3]);
        // Unknown project: first candidate.
        assert_eq!(pick(&captions, "nope").unwrap(), captions[0]);
    }

    #[test]
    fn script_embeds_inputs_as_json() {
        let script = kwin_script(&[1, 2], "a\"b");
        assert!(script.contains("const pids = [1,2];"));
        assert!(script.contains(r#"const project = "a\"b";"#));
    }
}
