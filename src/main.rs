use std::{
  str::FromStr,
  sync::Arc,
  time::{Duration, SystemTime},
};

use clap::{Parser, Subcommand};
use duration_string::DurationString;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use zbus::zvariant::Optional;

#[derive(Parser)]
struct Cli {
  #[command(subcommand)]
  subcmd: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
  /// Prevent screensaver/lockscreen from showing by resetting X11 idle time
  Lock {
    mode: Option<LockMode>,

    #[clap(short, long, default_value = "1h", value_parser = parse_opt_duration)]
    duration: Option<Duration>,
  },
  Unlock,
}

const UPDATE_DURATION: Duration = Duration::from_secs(60);

#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Default,
  Serialize,
  Deserialize,
  zbus::zvariant::Type,
)]
#[serde(rename_all = "lowercase")]
enum LockMode {
  #[default]
  Idle,
  Sleep,
}

impl LockMode {
  async fn keep_await(&self) {
    println!("keeping awake: {:?}", self);
  }
}

impl FromStr for LockMode {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "idle" => Ok(LockMode::Idle),
      "sleep" => Ok(LockMode::Sleep),
      _ => Err("invalid mode".to_string()),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Lock {
  mode: LockMode,
  until: Option<SystemTime>,
}

impl Lock {
  fn remaining(&self) -> Option<Duration> {
    self
      .until
      .and_then(|until| until.duration_since(SystemTime::now()).ok())
  }

  fn remaining_message(&self) -> String {
    match self.remaining() {
      None => "".to_string(),
      Some(remaining) => {
        let min = remaining.as_secs() / 60;
        format!("{}m", min)
      }
    }
  }
}

#[derive(Default, Debug)]
struct Daemon {
  inner: Arc<RwLock<Inner>>,
  previous_report: Option<StatusReport>,
}

#[derive(Default, Debug)]
struct Inner {
  lock: Option<Lock>,
}

struct DbusIdleDaemon {
  signal: mpsc::Sender<()>,
  inner: Arc<RwLock<Inner>>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
struct StatusReport {
  locked: bool,
  mode: Option<LockMode>,
  remaining_seconds: Option<u64>,
  message: String,
}

impl Inner {
  fn report(&self) -> StatusReport {
    let message = match self.lock {
      None => String::new(),
      Some(lock) => {
        format!("\u{2615}{}", lock.remaining_message())
      }
    };
    let remaining_seconds = self
      .lock
      .and_then(|lock| lock.remaining().map(|d| d.as_secs()));

    StatusReport {
      locked: self.lock.is_some(),
      mode: self.lock.map(|lock| lock.mode),
      remaining_seconds,
      message,
    }
  }
}

impl Daemon {
  async fn keep_awake(&mut self) {
    let mut inner = self.inner.write().await;
    let Some(lock) = inner.lock.as_mut() else {
      return;
    };

    let now = SystemTime::now();
    match lock.until {
      None => return,
      Some(until) if until < now => {
        inner.lock.take();
        return;
      }
      _ => {}
    }

    lock.mode.keep_await().await;
  }

  async fn send_report(&mut self) {
    let report = self.inner.read().await.report();
    let should_update_report = self.previous_report.is_none()
      || self
        .previous_report
        .as_ref()
        .is_some_and(|previous_report| report != *previous_report);

    if should_update_report {
      println!("{}", serde_json::to_string(&report).unwrap());
      self.previous_report.replace(report);
    }
  }

  async fn run(&mut self) -> Result<(), zbus::Error> {
    let (signal, mut receiver) = mpsc::channel(1);
    let dbus_service = DbusIdleDaemon {
      inner: self.inner.clone(),
      signal,
    };
    let _conn = zbus::connection::Builder::session()?
      .name("org.shou.IdleDaemon")?
      .serve_at("/org/shou/IdleDaemon", dbus_service)?
      .build()
      .await?;

    loop {
      self.keep_awake().await;
      self.send_report().await;

      tokio::select! {
        _ = receiver.recv() => {}
        _ = tokio::time::sleep(UPDATE_DURATION) => {}
      }
    }
  }
}

#[zbus::interface(name = "org.shou.IdleDaemon")]
impl DbusIdleDaemon {
  async fn lock(&self, mode: LockMode, until_epoch: Optional<u64>) {
    let mut inner = self.inner.write().await;
    let until =
      until_epoch.map(|ts| SystemTime::UNIX_EPOCH + Duration::from_secs(ts));
    inner.lock.replace(Lock { mode, until });
    self.signal.send(()).await.expect("failed to send signal");
  }

  async fn unlock(&self) {
    let mut inner = self.inner.write().await;
    inner.lock.take();
    self.signal.send(()).await.expect("failed to send signal");
  }
}

#[zbus::proxy(
  interface = "org.shou.IdleDaemon",
  default_service = "org.shou.IdleDaemon",
  default_path = "/org/shou/IdleDaemon"
)]
trait DbusIdleDaemon {
  async fn lock(
    &self,
    mode: LockMode,
    until_epoch: Optional<u64>,
  ) -> zbus::Result<()>;
  async fn unlock(&self) -> zbus::Result<()>;
}

async fn lock(
  mode: LockMode,
  duration: Option<Duration>,
) -> Result<(), zbus::Error> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusIdleDaemonProxy::new(&conn).await?;
  let until = duration.map(|d| SystemTime::now() + d);
  let until_epoch = until.map(|until| {
    until
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_secs()
  });

  proxy.lock(mode, until_epoch.into()).await?;
  Ok(())
}

async fn unlock() -> Result<(), zbus::Error> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusIdleDaemonProxy::new(&conn).await?;
  proxy.unlock().await?;
  Ok(())
}

#[tokio::main]
async fn main() {
  use Commands::{Lock, Unlock};
  let cli = Cli::parse();

  match cli.subcmd {
    None => {
      Daemon::default().run().await.expect("Failed to run daemon");
    }
    Some(Lock { mode, duration }) => {
      let mode = mode.unwrap_or_default();
      lock(mode, duration).await.expect("Failed to lock");
    }
    Some(Unlock) => {
      unlock().await.expect("Failed to unlock");
    }
  }
}

fn parse_opt_duration(s: &str) -> Result<Duration, String> {
  Ok(DurationString::from_str(s)?.into())
}
