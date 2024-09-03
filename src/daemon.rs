use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use tokio::sync::{mpsc, oneshot};
use tracing::info;
use zbus::object_server::InterfaceRef;

use crate::{
  inhibitor::{self, InhibitMode, Inhibitor},
  protocol::{DurationUpdate, Status},
  signals,
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
  ExitSignal,
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
    exit_signals: &mut signals::ExitSignals,
  ) -> DaemonEvent {
    let sleep = deadline
      .map(|d| tokio::time::sleep_until(d.into()))
      .unwrap_or_else(|| tokio::time::sleep(Duration::MAX));

    tokio::select! {
      _ = exit_signals.recv() => {
        DaemonEvent::ExitSignal
      }

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
      .name("org.shou.Vigilare")?
      .serve_at("/org/shou/Vigilare", dbus_service)?
      .build()
      .await?;

    let iface: InterfaceRef<DbusService> =
      conn.object_server().interface("/org/shou/Vigilare").await?;

    let status_changed = || async {
      let signal_ctx = iface.signal_context();
      let iface = iface.get().await;
      iface
        .status_invalidate(signal_ctx)
        .await
        .expect("Failed to emit status changed");
    };

    let mut exit_signals = signals::ExitSignals::new();

    info!(
      "Daemon started at {}",
      conn.unique_name().expect("Failed to get unique name")
    );
    status_changed().await;

    loop {
      let event =
        Self::get_event(&mut receiver, &self.wake_until, &mut exit_signals);

      match event.await {
        DaemonEvent::DurationUpdate(update) => {
          self.update_duration(update)?;
          self.update_inhibitor().await?;
          status_changed().await;
        }
        DaemonEvent::StatusRequest(sender) => {
          sender.send(self.status()).ok();
        }
        DaemonEvent::Deadline => {
          self.wake_until = None;
          self.update_inhibitor().await?;
          status_changed().await;
        }
        DaemonEvent::ExitSignal => {
          info!("Received exit signal, exiting");
          break;
        }
        DaemonEvent::DbusServiceExit => {
          info!("Dbus service exited");
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
      None => {
        info!("Uninhibiting");
        self.inhibitor.uninhibit().await?
      }
      Some(_wake_until) => {
        info!("Inhibiting");
        self.inhibitor.inhibit().await?
      }
    }

    Ok(())
  }

  fn status(&self) -> Status {
    if self.wake_until.is_none() {
      return Status {
        wake_until: 0,
        active: false,
      };
    }

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
      active: true,
    }
  }
}

struct DbusService {
  sender: mpsc::Sender<DaemonMessage>,
}

#[zbus::interface(name = "org.shou.Vigilare")]
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
