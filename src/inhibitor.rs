use std::{str::FromStr, time::Duration};

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

#[async_trait::async_trait]
pub trait Inhibitor {
  // Result::Err(_) is equivalent to Ok(false)
  async fn available(&self) -> Result<bool>;
  async fn inhibit(&mut self) -> Result<()>;
  async fn uninhibit(&mut self) -> Result<()>;
}

#[derive(
  Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Type, ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InhibitMode {
  /// Inhibit sleep from xfce4-power-manager
  #[value(alias = "xfce", alias = "xfce4")]
  Xfce4PowerManager,
  /// Inhibit sleep from xfce4-screensaver
  Xfce4Screensaver,
  /// Inhibit sleep with `systemd-inhibit`
  #[value(alias = "systemd")]
  Logind,
  /// Reset the XScreenSaver time with `xset s reset`
  #[value(alias = "xset")]
  Xscreensaver,
  /// Inhibit sleep with occasional mouse jitter
  MouseJitter,
  /// Inhibit sleep with a dummy audio playback
  #[value(alias = "audio")]
  AudioPlayback,
}

pub async fn available_modes() -> Vec<InhibitMode> {
  let mut modes = Vec::new();
  for mode in InhibitMode::value_variants() {
    let inhibitor = from_mode(*mode).await;

    if let Ok(inhibitor) = inhibitor {
      if inhibitor.available().await.unwrap_or(false) {
        modes.push(*mode);
      }
    }
  }

  modes
}

impl FromStr for InhibitMode {
  type Err = anyhow::Error;

  fn from_str(s: &str) -> Result<Self> {
    match s {
      "xscreensaver" => Ok(Self::Xscreensaver),
      "xset" => Ok(Self::Xscreensaver),
      "logind" => Ok(Self::Logind),
      "xfce4-power-manager" => Ok(Self::Xfce4PowerManager),
      "xfce" => Ok(Self::Xfce4PowerManager),
      "xfce4" => Ok(Self::Xfce4PowerManager),
      "xfce4-screensaver" => Ok(Self::Xfce4Screensaver),
      "mouse-jitter" => Ok(Self::MouseJitter),
      "mouse" => Ok(Self::MouseJitter),
      "audio" => Ok(Self::AudioPlayback),
      _ => Err(anyhow::anyhow!("unknown mechanism: {}", s)),
    }
  }
}

pub async fn from_mode(mode: InhibitMode) -> Result<Box<dyn Inhibitor>> {
  use InhibitMode::*;

  fn ok(inhibitor: impl Inhibitor + 'static) -> Result<Box<dyn Inhibitor>> {
    Ok(Box::new(inhibitor))
  }

  match mode {
    Xscreensaver => {
      ok(xscreensaver::XScreensaver::new(Duration::from_secs(60)))
    }
    Logind => {
      let conn = zbus::Connection::system().await?;
      ok(logind::LogindInhibit::new(conn))
    }
    Xfce4PowerManager => {
      let conn = zbus::Connection::session().await?;
      ok(xfce_power_manager::XfcePowerManager::new(conn))
    }
    Xfce4Screensaver => {
      let conn = zbus::Connection::session().await?;
      ok(xfce_screen_saver::XfceScreenSaver::new(conn))
    }
    MouseJitter => ok(mouse_jitter::MouseJitter::new(Duration::from_secs(60))),
    AudioPlayback => ok(audio::AudioPlayback::new()),
  }
}

mod xscreensaver {
  use std::time::Duration;

  use tokio::process::Command;

  use super::*;

  pub struct XScreensaver {
    interval: Duration,
    task: Option<tokio::task::JoinHandle<()>>,
  }

  impl XScreensaver {
    pub fn new(interval: Duration) -> Self {
      Self {
        interval,
        task: None,
      }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for XScreensaver {
    async fn available(&self) -> Result<bool> {
      // available if xset binary is found in PATH
      let output = Command::new("which").arg("xset").output().await?;

      Ok(output.status.success())
    }

    async fn inhibit(&mut self) -> Result<()> {
      if self.task.is_some() {
        return Ok(());
      }

      let reset_duration = self.interval;
      let task = tokio::spawn(async move {
        loop {
          tokio::time::sleep(reset_duration).await;
          Command::new("xset")
            .arg("s")
            .arg("reset")
            .output()
            .await
            .expect("failed to run xset s reset");
        }
      });
      self.task = Some(task);
      Ok(())
    }

    async fn uninhibit(&mut self) -> Result<()> {
      if let Some(task) = self.task.take() {
        task.abort();
      }
      Ok(())
    }
  }
}

mod logind {
  use zbus::Connection;

  use super::*;

  #[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
  )]
  trait LogindManager {
    /// Inhibit method
    fn inhibit(
      &self,
      what: &str,
      who: &str,
      why: &str,
      mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
  }

  pub struct LogindInhibit {
    conn: Connection,
    fd: Option<zbus::zvariant::OwnedFd>,
  }

  impl LogindInhibit {
    pub fn new(conn: Connection) -> Self {
      Self { conn, fd: None }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for LogindInhibit {
    async fn available(&self) -> Result<bool> {
      let proxy = LogindManagerProxy::new(&self.conn).await?;
      Ok(proxy.0.introspect().await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      if self.fd.is_some() {
        return Ok(());
      }

      let manager = LogindManagerProxy::new(&self.conn).await?;

      let fd = manager
        .inhibit("sleep", "vigilare", "user request", "block")
        .await?;

      self.fd = Some(fd);
      Ok(())
    }

    async fn uninhibit(&mut self) -> Result<()> {
      // dropping the fd closes it, releasing the inhibition
      self.fd.take();
      Ok(())
    }
  }
}

mod xfce_power_manager {
  use zbus::Connection;

  use super::*;

  #[zbus::proxy(
    interface = "org.freedesktop.PowerManagement.Inhibit",
    default_service = "org.xfce.PowerManager",
    default_path = "/org/freedesktop/PowerManagement/Inhibit"
  )]
  trait XfcePowerManager {
    fn inhibit(&self, application: &str, reason: &str) -> zbus::Result<u32>;
    #[zbus(name = "UnInhibit")]
    fn uninhibit(&self, cookie: u32) -> zbus::Result<()>;
  }

  pub struct XfcePowerManager {
    conn: Connection,
    cookie: Option<u32>,
  }

  impl XfcePowerManager {
    pub fn new(conn: Connection) -> Self {
      Self { conn, cookie: None }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for XfcePowerManager {
    async fn available(&self) -> Result<bool> {
      let proxy = XfcePowerManagerProxy::new(&self.conn).await?;
      Ok(proxy.0.introspect().await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      if self.cookie.is_some() {
        return Ok(());
      }

      let manager = XfcePowerManagerProxy::new(&self.conn).await?;
      let cookie = manager.inhibit("vigilare", "stay awake").await?;
      self.cookie = Some(cookie);
      Ok(())
    }

    async fn uninhibit(&mut self) -> Result<()> {
      if let Some(cookie) = self.cookie.take() {
        let manager = XfcePowerManagerProxy::new(&self.conn).await?;
        manager.uninhibit(cookie).await?;
      }
      Ok(())
    }
  }
}

mod xfce_screen_saver {
  use zbus::Connection;

  use super::*;

  #[zbus::proxy(
    interface = "org.xfce.ScreenSaver",
    default_service = "org.xfce.ScreenSaver",
    default_path = "/"
  )]
  trait XfceScreenSaver {
    fn inhibit(&self, application: &str, reason: &str) -> zbus::Result<u32>;
    #[zbus(name = "UnInhibit")]
    fn uninhibit(&self, cookie: u32) -> zbus::Result<()>;
  }

  pub struct XfceScreenSaver {
    conn: Connection,
    cookie: Option<u32>,
  }

  impl XfceScreenSaver {
    pub fn new(conn: Connection) -> Self {
      Self { conn, cookie: None }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for XfceScreenSaver {
    async fn available(&self) -> Result<bool> {
      let proxy = XfceScreenSaverProxy::new(&self.conn).await?;
      Ok(proxy.0.introspect().await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      if self.cookie.is_some() {
        return Ok(());
      }

      let manager = XfceScreenSaverProxy::new(&self.conn).await?;
      let cookie = manager.inhibit("vigilare", "stay awake").await?;
      self.cookie = Some(cookie);
      Ok(())
    }

    async fn uninhibit(&mut self) -> Result<()> {
      if let Some(cookie) = self.cookie.take() {
        let manager = XfceScreenSaverProxy::new(&self.conn).await?;
        manager.uninhibit(cookie).await?;
      }
      Ok(())
    }
  }
}

mod mouse_jitter {
  use std::time::Duration;

  use enigo::{Coordinate, Enigo, Mouse as _};

  use super::*;

  pub struct MouseJitter {
    interval: Duration,
    task: Option<tokio::task::JoinHandle<()>>,
  }

  impl MouseJitter {
    pub fn new(jitter_interval: Duration) -> Self {
      Self {
        interval: jitter_interval,
        task: None,
      }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for MouseJitter {
    async fn available(&self) -> Result<bool> {
      let mouse = Enigo::new(&Default::default())?;
      Ok(mouse.location().is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      if self.task.is_some() {
        return Ok(());
      }

      let interval = self.interval;
      let history_len = (60.0 / interval.as_secs_f32()).ceil() as usize + 1;
      let mut history = Vec::with_capacity(history_len + 1);
      let mut mouse = Enigo::new(&Default::default())?;

      let task = tokio::spawn(async move {
        loop {
          tokio::time::sleep(interval).await;

          let Ok(pos) = mouse.location() else {
            break;
          };
          history.push(pos);

          // we record the history of the cursor position
          while history.len() > history_len {
            history.remove(0);
          }

          if !history.iter().all(|&p| p == pos) {
            // the cursor moved, no need to jitter
            continue;
          };

          // now let's jitter it just a little bit
          mouse
            .move_mouse(0, 1, Coordinate::Rel)
            .expect("failed to move mouse");
          mouse
            .move_mouse(pos.0, pos.1, Coordinate::Abs)
            .expect("failed to move mouse");
        }
      });
      self.task = Some(task);

      Ok(())
    }

    async fn uninhibit(&mut self) -> Result<()> {
      if let Some(task) = self.task.take() {
        task.abort();
      }
      Ok(())
    }
  }
}

mod audio {
  use super::*;
  use std::sync::atomic::AtomicU8;
  use tokio::sync::mpsc::{channel, Receiver, Sender};

  pub(super) struct AudioPlayback {
    handle: PlayerHandle,
  }

  struct PlayerHandle {
    available: AtomicU8, // 0: unknown, 1: available, 2: not available
    control: Sender<bool>,
    handle: tokio::task::JoinHandle<Result<()>>,
  }

  impl PlayerHandle {
    fn available(&self) -> bool {
      match self.available.load(std::sync::atomic::Ordering::SeqCst) {
        1 => return true,
        2 => return false,
        0 => {}
        _ => unreachable!("Invalid state"),
      }

      // if the thread is not running, we assume it's available
      if self.control.is_closed() && self.handle.is_finished() {
        self.available.store(2, std::sync::atomic::Ordering::SeqCst);
        return false;
      }

      // otherwise, we assume it's available
      self.available.store(1, std::sync::atomic::Ordering::SeqCst);
      true
    }

    async fn inhibit(&self) -> Result<()> {
      if !self.available() {
        return Err(anyhow::anyhow!("Audio playback is not available"));
      }

      // send a signal to start playback
      self.control.send(true).await?;
      Ok(())
    }

    async fn uninhibit(&self) -> Result<()> {
      if !self.available() {
        return Err(anyhow::anyhow!("Audio playback is not available"));
      }

      // send a signal to stop playback
      self.control.send(false).await?;
      Ok(())
    }

    fn start() -> Self {
      let (control_tx, control_rx) = channel(1);
      let handle = tokio::task::spawn_blocking(|| Self::run(control_rx));
      Self {
        available: AtomicU8::new(0), // 0: unknown
        control: control_tx,
        handle
      }
    }

    fn run(mut control_rx: Receiver<bool>) -> Result<()> {
      // try to get the default output stream, if fails, return an error
      drop(rodio::OutputStream::try_default()?);
      let mut holder = Option::None;

      while let Some(play) = control_rx.blocking_recv() {
        if !play {
          drop(holder.take());
          continue;
        }

        if play && holder.is_some() {
          continue; // already playing
        }

        let _ = holder.insert(rodio::OutputStream::try_default()?);
      }

      Ok(())
    }
  }

  impl AudioPlayback {
    pub fn new() -> Self {
      Self {  handle: PlayerHandle::start() }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for AudioPlayback {
    async fn available(&self) -> Result<bool> {
      Ok(self.handle.available())
    }

    async fn inhibit(&mut self) -> Result<()> {
      self.handle.inhibit().await
    }
    async fn uninhibit(&mut self) -> Result<()> {
      self.handle.uninhibit().await
    }
  }
}
