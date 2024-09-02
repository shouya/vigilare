use std::time::Duration;

use anyhow::Result;

use crate::InhibitMode;

#[async_trait::async_trait]
pub trait Inhibitor {
  async fn available(&self) -> Result<bool>;
  async fn inhibit(&mut self) -> Result<()>;
  async fn uninhibit(&mut self) -> Result<()>;
}

pub async fn from_string(s: &str) -> Result<Box<dyn Inhibitor>> {
  match s {
    "xset" => from_mode(InhibitMode::XSet).await,
    _ => Err(anyhow::anyhow!("unknown mechanism: {}", s)),
  }
}

pub async fn from_mode(mode: InhibitMode) -> Result<Box<dyn Inhibitor>> {
  use InhibitMode::*;

  fn ok(inhibitor: impl Inhibitor + 'static) -> Result<Box<dyn Inhibitor>> {
    Ok(Box::new(inhibitor))
  }

  match mode {
    XSet => ok(xset::XSet::new(Duration::from_secs(60))),
    _ => Err(anyhow::anyhow!("unknown mode: {:?}", mode)),
  }
}

mod xset {
  use std::time::Duration;

  use tokio::process::Command;

  use super::*;

  pub struct XSet {
    interval: Duration,
    task: Option<tokio::task::JoinHandle<()>>,
  }

  impl XSet {
    pub fn new(interval: Duration) -> Self {
      Self {
        interval,
        task: None,
      }
    }
  }

  #[async_trait::async_trait]
  impl Inhibitor for XSet {
    async fn available(&self) -> Result<bool> {
      // available if xset binary is found in PATH
      let output = Command::new("which").arg("xset").output().await?;

      Ok(output.status.success())
    }

    async fn inhibit(&mut self) -> Result<()> {
      self.uninhibit().await?;

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
      Ok(LogindManagerProxy::new(&self.conn).await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      let manager = LogindManagerProxy::new(&self.conn).await?;

      let fd = manager
        .inhibit("sleep", "wake-guard", "user request", "block")
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
      Ok(XfcePowerManagerProxy::new(&self.conn).await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      self.uninhibit().await?;

      let manager = XfcePowerManagerProxy::new(&self.conn).await?;
      let cookie = manager.inhibit("wake-guard", "stay awake").await?;
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
      Ok(XfceScreenSaverProxy::new(&self.conn).await.is_ok())
    }

    async fn inhibit(&mut self) -> Result<()> {
      self.uninhibit().await?;

      let manager = XfceScreenSaverProxy::new(&self.conn).await?;
      let cookie = manager.inhibit("wake-guard", "stay awake").await?;
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
    fn new(jitter_interval: Duration) -> Self {
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
      self.uninhibit().await?;
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
