//! "Start at login" for the current user.

use anyhow::{Context, Result};

fn launch_args() -> Result<(String, Vec<String>)> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut args = vec!["--background".to_string()];
    if let Some(dir) = crate::DATA_DIR_ARG.get() {
        args.push("--data-dir".into());
        args.push(dir.clone());
    }
    Ok((exe, args))
}

#[cfg(windows)]
pub fn set(enabled: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut cmd = std::process::Command::new("reg");
    cmd.creation_flags(0x0800_0000);
    if enabled {
        let (exe, args) = launch_args()?;
        let quoted: Vec<String> = std::iter::once(exe).chain(args).map(|a| format!("\"{a}\"")).collect();
        cmd.args(["add", key, "/v", "SyncMe", "/t", "REG_SZ", "/f", "/d", &quoted.join(" ")]);
    } else {
        cmd.args(["delete", key, "/v", "SyncMe", "/f"]);
    }
    let st = cmd.status().context("running reg")?;
    if !st.success() && enabled {
        anyhow::bail!("reg exited with {st}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn set(enabled: bool) -> Result<()> {
    let dir = dirs::home_dir().context("no home")?.join("Library/LaunchAgents");
    let plist = dir.join("com.syncme.agent.plist");
    if !enabled {
        let _ = std::fs::remove_file(&plist);
        return Ok(());
    }
    let (exe, args) = launch_args()?;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let items: String = std::iter::once(exe).chain(args).map(|a| format!("<string>{}</string>", esc(&a))).collect();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.syncme.agent</string>
<key>ProgramArguments</key><array>{items}</array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#
        ),
    )?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn set(enabled: bool) -> Result<()> {
    let dir = dirs::config_dir().context("no config dir")?.join("autostart");
    let file = dir.join("syncme.desktop");
    if !enabled {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    let (exe, args) = launch_args()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&file, format!("[Desktop Entry]\nType=Application\nName=SyncMe\nExec=\"{exe}\" {}\n", args.join(" ")))?;
    Ok(())
}
