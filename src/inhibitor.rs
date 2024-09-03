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
  #[serde(alias = "xfce", alias = "xfce4")]
  Xfce4PowerManager,
  /// Inhibit sleep from xfce4-screensaver
  Xfce4Screensaver,
  /// Inhibit sleep with `systemd-inhibit`
  #[serde(alias = "systemd")]
  Logind,
  /// Reset the XScreenSaver time with `xset s reset`
  #[serde(alias = "xset")]
  Xscreensaver,
  /// Inhibit sleep with occasional mouse jitter
  MouseJitter,
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
