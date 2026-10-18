//! "Launch Claude": open a terminal running Claude Code in a project folder,
//! using an editable command template where `{dir}` is the quoted folder.

use std::path::Path;

/// Default terminal command for this platform.
pub fn default_command() -> &'static str {
    if cfg!(target_os = "macos") {
        r#"osascript -e "tell application \"Terminal\" to do script \"cd {dir} && claude\"" -e "tell application \"Terminal\" to activate""#
    } else if cfg!(target_os = "windows") {
        "wt -d {dir} claude"
    } else {
        "konsole --workdir {dir} -e claude"
    }
}

/// Quotes a path for the shell that runs the template: POSIX single quotes,
/// or double quotes for cmd.exe on Windows.
pub fn quote_dir(dir: &str, windows: bool) -> String {
    if windows {
        format!("\"{dir}\"")
    } else {
        format!("'{}'", dir.replace('\'', r"'\''"))
    }
}

pub fn expand(template: &str, dir: &Path, windows: bool) -> String {
    let template = if template.trim().is_empty() {
        default_command()
    } else {
        template
    };
    template.replace("{dir}", &quote_dir(&dir.to_string_lossy(), windows))
}

/// Starts the command detached from the plugin.
pub fn launch(template: &str, dir: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(dir.is_dir(), "no such directory: {}", dir.display());
    let command = expand(template, dir, cfg!(windows));
    let mut child = shell(&command);
    child
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut spawned = child.spawn()?;
    // Reap it whenever it exits so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = spawned.wait();
    });
    Ok(())
}

#[cfg(unix)]
fn shell(command: &str) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sh");
    child.arg("-c").arg(command);
    // Own process group, so it survives the plugin restarting.
    child.process_group(0);
    child
}

#[cfg(windows)]
fn shell(command: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = std::process::Command::new("cmd");
    child.raw_arg(format!("/C {command}")).creation_flags(CREATE_NO_WINDOW);
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_quoted_dir() {
        assert_eq!(
            expand(
                "konsole --workdir {dir} -e claude",
                Path::new("/home/me/it's here"),
                false
            ),
            r"konsole --workdir '/home/me/it'\''s here' -e claude"
        );
        assert_eq!(
            expand("wt -d {dir} claude", Path::new(r"C:\My Code"), true),
            r#"wt -d "C:\My Code" claude"#
        );
        assert!(expand("", Path::new("/x"), false).contains("'/x'"));
    }

    #[cfg(unix)]
    #[test]
    fn launches_template_in_dir() {
        let dir = std::env::temp_dir().join(format!("claude-opendeck-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        launch(&format!("echo {{dir}} > '{}'", marker.display()), &dir).unwrap();
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), dir.to_string_lossy());
        assert!(launch("true", &dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
