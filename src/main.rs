use std::{
  str::FromStr,
  sync::Arc,
  time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use duration_string::DurationString;
use serde::{Deserialize, Serialize};
use tokio::{
  process::Command,
  sync::{mpsc, RwLock},
};
use zbus::zvariant::{Optional, Type};

#[derive(Parser)]
struct Cli {
  #[clap(subcommand)]
  cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the daemon
  Daemon,

  /// Control the daemon
  Msg {
    /// Set wake mode (idle or sleep)
    #[arg(short, long)]
    mode: Option<WakeMode>,

    /// Update the duration of the wake guard. Prefix with "+" to add, "-" to subtract.
    /// Duration syntax: "1h", "30m", "1d", etc.
    #[arg(value_parser = parse_duration_update, allow_hyphen_values = true)]
    update: DurationUpdate,
  },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
enum DurationUpdate {
  Add(Duration),
  Sub(Duration),
  Set(Duration),
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
enum WakeMode {
  #[default]
  Idle,
  Sleep,
}

impl WakeMode {
  async fn keep_await(&self) -> Option<WakeToken> {
    match self {
      WakeMode::Idle => {
        // run `xset s reset` to reset the idle timer
        Command::new("xset")
          .arg("s")
          .arg("reset")
          .output()
          .await
          .expect("failed to run xset s reset");

        None
      }

      WakeMode::Sleep => {
        let conn = zbus::Connection::system()
          .await
          .expect("failed to connect to session bus");

        let proxy = LogindManagerProxy::new(&conn)
          .await
          .expect("failed to create proxy");

        let fd = proxy
          .inhibit("sleep", "WakeGuard", "user requested", "block")
          .await
          .expect("failed to inhibit sleep");

        Some(WakeToken::Logind(fd))
      }
    }
  }
}

impl FromStr for WakeMode {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "idle" => Ok(WakeMode::Idle),
      "sleep" => Ok(WakeMode::Sleep),
      _ => Err("invalid mode".to_string()),
    }
  }
}

enum WakeToken {
  Logind(#[allow(unused)] zbus::zvariant::OwnedFd),
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
struct WakeGuard {
  mode: WakeMode,
  until: Option<Instant>,
}

impl WakeGuard {
  fn new(mode: WakeMode, until: Option<Instant>) -> Self {
    Self { mode, until }
  }

  fn remaining(&self) -> Option<Duration> {
    self.until.map(|until| until.duration_since(Instant::now()))
  }

  fn remaining_message(&self) -> String {
    match self.remaining() {
      None => "".to_string(),
      Some(remaining) => {
        let min = remaining.as_secs() as f32 / 60.0;
        format!("{}m", min.ceil() as u64)
      }
    }
  }
}

#[derive(Default)]
struct Daemon {
  inner: Arc<RwLock<Inner>>,
  previous_report: Option<StatusReport>,
  wake_token: Option<WakeToken>,
}

#[derive(Default, Debug)]
struct Inner {
  wake_guard: Option<WakeGuard>,
}

struct DbusService {
  signal: mpsc::Sender<()>,
  inner: Arc<RwLock<Inner>>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
struct StatusReport {
  active: bool,
  mode: Option<WakeMode>,
  remaining_seconds: Option<u64>,
  message: String,
}

impl Inner {
  fn report(&self) -> StatusReport {
    let message = match self.wake_guard {
      None => String::new(),
      Some(wake_guard) => {
        format!("\u{2615}{}", wake_guard.remaining_message())
      }
    };
    let remaining_seconds = self
      .wake_guard
      .and_then(|wg| wg.remaining().map(|d| d.as_secs()));

    StatusReport {
      active: self.wake_guard.is_some(),
      mode: self.wake_guard.map(|wg| wg.mode),
      remaining_seconds,
      message,
    }
  }
}

impl Daemon {
  async fn sleep_duration(&self) -> Duration {
    let inner = self.inner.read().await;
    let remaining = inner
      .wake_guard
      .as_ref()
      .and_then(|wg| wg.remaining())
      .filter(|wg| wg < &UPDATE_DURATION);

    remaining.unwrap_or(UPDATE_DURATION)
  }

  async fn stay_awake(&mut self) {
    let mut inner = self.inner.write().await;
    let Some(wg) = inner.wake_guard.as_mut() else {
      self.wake_token.take();
      return;
    };

    let now = Instant::now();
    match wg.until {
      None => return,
      Some(until) if until < now => {
        inner.wake_guard.take();
        self.wake_token.take();
        return;
      }
      _ => {}
    }

    let wake_token = wg.mode.keep_await().await;
    self.wake_token = wake_token;
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
    let dbus_service = DbusService {
      inner: self.inner.clone(),
      signal,
    };
    let _conn = zbus::connection::Builder::session()?
      .name("org.shou.WakeGuard")?
      .serve_at("/org/shou/WakeGuard", dbus_service)?
      .build()
      .await?;

    loop {
      self.stay_awake().await;
      self.send_report().await;

      let sleep_duration = self.sleep_duration().await;
      tokio::select! {
        _ = receiver.recv() => {}
        _ = tokio::time::sleep(sleep_duration) => {}
      }
    }
  }
}

#[zbus::interface(name = "org.shou.WakeGuard")]
impl DbusService {
  async fn update(&self, mode: Optional<WakeMode>, update: DurationUpdate) {
    use DurationUpdate::{Add, Set, Sub};

    let mut inner = self.inner.write().await;
    let wg = inner.wake_guard.as_ref();
    let now = Instant::now();

    let new_until = match (wg, update) {
      (Some(wg), Add(duration)) => wg.until.map(|until| until + duration),
      (Some(wg), Sub(duration)) => wg.until.map(|until| until - duration),
      (Some(_wg), Set(duration)) => Some(now + duration),
      (None, Add(duration)) => Some(now + duration),
      (None, Set(duration)) => Some(now + duration),
      (None, Sub(_)) => Some(now),
    };

    // deactivate if the new until is in the past
    if new_until.is_some_and(|t| t <= now) {
      inner.wake_guard.take();
      self.signal.send(()).await.expect("failed to send signal");
      return;
    }

    let mode = mode.or_else(|| wg.map(|wg| wg.mode)).unwrap_or_default();
    inner.wake_guard.replace(WakeGuard::new(mode, new_until));
    self.signal.send(()).await.expect("failed to send signal");
  }
}

#[zbus::proxy(
  interface = "org.shou.WakeGuard",
  default_service = "org.shou.WakeGuard",
  default_path = "/org/shou/WakeGuard"
)]
trait DbusWakeGuard {
  async fn update(
    &self,
    mode: Optional<WakeMode>,
    update: DurationUpdate,
  ) -> zbus::Result<()>;
}

async fn update_wake_guard(
  mode: Option<WakeMode>,
  update: DurationUpdate,
) -> Result<(), zbus::Error> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusWakeGuardProxy::new(&conn).await?;
  proxy.update(mode.into(), update).await?;
  Ok(())
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.cmd {
    Commands::Daemon => {
      Daemon::default().run().await.expect("Failed to run daemon");
    }
    Commands::Msg { mode, update } => {
      update_wake_guard(mode, update)
        .await
        .expect("Failed to update");
    }
  }
}

fn parse_duration_update(s: &str) -> Result<DurationUpdate, String> {
  match &s[..1] {
    "+" => {
      let duration = DurationString::from_str(&s[1..])?.into();
      Ok(DurationUpdate::Add(duration))
    }
    "-" => {
      let duration = DurationString::from_str(&s[1..])?.into();
      Ok(DurationUpdate::Sub(duration))
    }
    "0" => Ok(DurationUpdate::Set(Duration::ZERO)),
    _ => {
      let duration = DurationString::from_str(s)?.into();
      Ok(DurationUpdate::Set(duration))
    }
  }
}
