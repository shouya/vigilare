use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::info;
use zbus::zvariant::Type;

use crate::{
  inhibitor::{self, Inhibitor},
  protocol::{DurationUpdate, Status},
};

pub struct Daemon {
  // None: computer is free to sleep
  wake_until: Option<Instant>,
  inhibitor: Box<dyn Inhibitor>,
}

enum DaemonEvent {
  DurationUpdate(DurationUpdate),
  StatusRequest(oneshot::Sender<Status>),
  Deadline,
  DbusServiceExit,
}

impl Daemon {
  pub async fn new(mode: InhibitMode) -> Result<Self> {
    let inhibitor = inhibitor::from_mode(mode)
      .await
      .expect("Failed to create inhibitor");

    Ok(Self {
      wake_until: None,
      inhibitor,
    })
  }

  async fn get_event(
    receiver: &mut mpsc::Receiver<DaemonMessage>,
    deadline: &Option<Instant>,
  ) -> DaemonEvent {
    let sleep = deadline
      .map(|d| tokio::time::sleep_until(d.into()))
      .unwrap_or_else(|| tokio::time::sleep(Duration::MAX));

    tokio::select! {
      msg = receiver.recv() => {
        match msg {
          Some(DaemonMessage::DurationUpdate(update)) => {
            DaemonEvent::DurationUpdate(update)
          }
          Some(DaemonMessage::StatusRequest(sender)) => {
            DaemonEvent::StatusRequest(sender)
          }
          None => {
            DaemonEvent::DbusServiceExit
          }
        }
      }
      _ = sleep => {
        DaemonEvent::Deadline
      }
    }
  }

  pub async fn run(&mut self) -> Result<()> {
    let (sender, mut receiver) = mpsc::channel(1);
    let dbus_service = DbusService { sender };
    let conn = zbus::connection::Builder::session()?
      .name("org.shou.WakeGuard")?
      .serve_at("/WakeGuard", dbus_service)?
      .build()
      .await?;

    info!("Daemon started at {:?}", conn.unique_name());

    loop {
      match Self::get_event(&mut receiver, &self.wake_until).await {
        DaemonEvent::DurationUpdate(update) => {
          self.update_duration(update)?;
          self.update_inhibitor().await?;
        }
        DaemonEvent::StatusRequest(sender) => {
          sender.send(self.status()).ok();
        }
        DaemonEvent::Deadline => {
          self.wake_until = None;
          self.update_inhibitor().await?;
        }
        DaemonEvent::DbusServiceExit => {
          println!("Dbus service exited");
          break;
        }
      }
    }

    Ok(())
  }

  fn update_duration(&mut self, update: DurationUpdate) -> Result<()> {
    let now = Instant::now();
    let wake_until = self.wake_until.unwrap_or(now);

    let new_wake_until = match update {
      DurationUpdate::Add(duration) => wake_until + duration,
      DurationUpdate::Sub(duration) => wake_until - duration,
      DurationUpdate::Set(duration) => now + duration,
    };

    if new_wake_until <= now {
      self.wake_until = None;
    } else {
      self.wake_until = Some(new_wake_until);
    }

    Ok(())
  }

  async fn update_inhibitor(&mut self) -> Result<()> {
    match self.wake_until {
      None => self.inhibitor.uninhibit().await?,
      Some(_wake_until) => self.inhibitor.inhibit().await?,
    }

    Ok(())
  }

  fn status(&self) -> Status {
    let now = Instant::now();
    let wake_until = self.wake_until.unwrap_or(now);
    let wake_after = wake_until.saturating_duration_since(now);
    let now_system = SystemTime::now();
    let wake_until_system = now_system + wake_after;
    let unix_epoch = wake_until_system
      .duration_since(SystemTime::UNIX_EPOCH)
      .expect("Failed to convert to UNIX epoch time")
      .as_secs();

    Status {
      wake_until: unix_epoch,
      active: self.wake_until.is_some(),
    }
  }
}

struct DbusService {
  sender: mpsc::Sender<DaemonMessage>,
}

#[zbus::interface(name = "org.shou.WakeGuard")]
impl DbusService {
  async fn update(&self, update: DurationUpdate) -> zbus::fdo::Result<()> {
    self
      .sender
      .send(DaemonMessage::DurationUpdate(update))
      .await
      .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
    Ok(())
  }

  #[zbus(property)]
  async fn status(&self) -> zbus::fdo::Result<Status> {
    let (sender, receiver) = oneshot::channel();
    self
      .sender
      .send(DaemonMessage::StatusRequest(sender))
      .await
      .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

    let status = receiver
      .await
      .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

    Ok(status)
  }
}

enum DaemonMessage {
  DurationUpdate(DurationUpdate),
  StatusRequest(oneshot::Sender<Status>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum InhibitMode {
  /// Reset the XScreenSaver time with `xset s reset`
  XSet,
  /// Inhibit sleep with `systemd-inhibit`
  Logind,
  /// Inhibit sleep from xfce4-power-manager
  Xfce4,
  /// Inhibit sleep from xfce4-screensaver
  Xfce4Screensaver,
  /// Inhibit sleep with occasional mouse jitter
  MouseJitter,
}
