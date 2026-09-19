use std::{io, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

pub async fn copy(text: String) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let commands: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    let commands: &[(&str, &[&str])] = &[("clip.exe", &[])];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut error = io::Error::new(
        io::ErrorKind::NotFound,
        "No clipboard command available; use terminal selection (Shift-drag) and its copy shortcut",
    );
    for (program, args) in commands {
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let mut child = Command::new(program)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(text.as_bytes()).await?;
            drop(stdin);
            if child.wait().await?.success() {
                Ok(())
            } else {
                Err(io::Error::other("Clipboard command failed"))
            }
        })
        .await;
        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => error = e,
            Err(_) => error = io::Error::new(io::ErrorKind::TimedOut, "Clipboard timed out"),
        }
    }
    Err(error)
}
