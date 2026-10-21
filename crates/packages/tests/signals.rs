//! Stopping `simien x` / `simien dev` stops the program they run: the
//! signal is passed on instead of leaving the program orphaned.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

#[tokio::test]
async fn a_sigterm_reaches_the_program() {
    let mut child = tokio::process::Command::new("sleep").arg("30").spawn().expect("sleep");
    let waiting = tokio::spawn(async move { packages::command::wait_forwarding_signals(&mut child).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // SAFETY: signalling this test process; the forwarder has installed its handler.
    unsafe { libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM) };
    let status = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the program stops within 5 s")
        .expect("join")
        .expect("wait");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
}
