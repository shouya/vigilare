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
use tracing::info;
use zbus::{
  export::futures_util::stream::StreamExt, object_server::InterfaceRef,
};
use zbus::{fdo, zvariant::Type};

#[derive(Parser)]
struct Cli {
  #[clap(subcommand)]
  cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the daemon
  Daemon,

  /// Subscribe to status updates
  Monitor,

  /// Control the daemon
  Msg {
    /// Set wake mode (idle or sleep)
    #[clap(short, long)]
    mode: Option<WakeMode>,

    /// Update the duration of the wake guard. Prefix with "+" to add, "-" to subtract.
    /// Duration syntax: "1h", "30m", "1d", etc.
    #[clap(value_parser = parse_duration_update, allow_hyphen_values = true)]
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
  Default,
  PartialEq,
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

impl std::fmt::Display for WakeMode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      WakeMode::Idle => write!(f, "idle"),
      WakeMode::Sleep => write!(f, "sleep"),
    }
  }
}

impl WakeMode {
  async fn stay_awake(&self) -> Option<WakeToken> {
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

struct WakeGuard {
  mode: WakeMode,
  until: Option<Instant>,
  // Keep an opaque token used by some inhibitors. We expect dropping
  // the token releases the inhibitor.
  token: Option<WakeToken>,
}

impl WakeGuard {
  fn new(mode: WakeMode, until: Option<Instant>) -> Self {
    Self {
      mode,
      until,
      token: None,
    }
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
}

#[derive(Default)]
struct Inner {
  report: StatusReport,
  wake_guard: Option<WakeGuard>,
}

struct DbusService {
  signal: mpsc::Sender<Option<WakeGuard>>,
  inner: Arc<RwLock<Inner>>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
struct StatusReport {
  active: bool,
  mode: Option<WakeMode>,
  remaining_seconds: Option<u64>,
  message: String,
}

impl StatusReport {
  fn json(&self) -> String {
    serde_json::to_string(&self).expect("failed to serialize report")
  }
}

impl Inner {
  fn report(&self) -> StatusReport {
    let message = match &self.wake_guard {
      None => String::new(),
      Some(wake_guard) => wake_guard.remaining_message(),
    };
    let remaining_seconds = self
      .wake_guard
      .as_ref()
      .and_then(|wg| wg.remaining().map(|d| d.as_secs()));

    StatusReport {
      active: self.wake_guard.is_some(),
      mode: self.wake_guard.as_ref().map(|wg| wg.mode),
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
      return;
    };

    let now = Instant::now();
    match wg.until {
      None => return,
      Some(until) if until < now => {
        inner.wake_guard.take();
        return;
      }
      _ => {}
    }

    wg.token = wg.mode.stay_awake().await;
  }

  async fn update_report(&mut self, iface_ref: &InterfaceRef<DbusService>) {
    let mut inner = self.inner.write().await;
    let new_report = inner.report();
    if inner.report == new_report {
      return;
    }
    inner.report = new_report;
    drop(inner);

    let iface = iface_ref.get_mut().await;
    iface
      .status_report_changed(iface_ref.signal_context())
      .await
      .expect("failed to send change signal");
  }

  async fn run(&mut self) -> Result<(), zbus::Error> {
    let (signal, mut receiver) = mpsc::channel(1);
    let dbus_service = DbusService {
      inner: self.inner.clone(),
      signal,
    };
    let conn = zbus::connection::Builder::session()?
      .name("org.shou.WakeGuard")?
      .serve_at("/org/shou/WakeGuard", dbus_service)?
      .build()
      .await?;

    info!("Daemon started at {:?}", conn.unique_name());

    let iface_ref = conn
      .object_server()
      .interface::<_, DbusService>("/org/shou/WakeGuard")
      .await?;

    loop {
      self.stay_awake().await;
      self.update_report(&iface_ref).await;

      let sleep_duration = self.sleep_duration().await;
      tokio::select! {
        v = receiver.recv() => {
          if let Some(wake_guard) = v {
            let mut inner = self.inner.write().await;
            inner.wake_guard = wake_guard;
          }
        }
        _ = tokio::time::sleep(sleep_duration) => {}
      }
    }
  }
}

#[zbus::interface(name = "org.shou.WakeGuard")]
impl DbusService {
  async fn update(
    &self,
    mode: String,
    update: DurationUpdate,
  ) -> zbus::fdo::Result<()> {
    use DurationUpdate::{Add, Set, Sub};
    let mode = match mode.as_ref() {
      "" => None,
      _ => Some(WakeMode::from_str(&mode).map_err(fdo::Error::InvalidArgs)?),
    };

    let inner = self.inner.read().await;
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
      self.signal.send(None).await.expect("failed to send signal");
      return Ok(());
    }

    let mode = mode.or_else(|| wg.map(|wg| wg.mode)).unwrap_or_default();
    let wake_guard = WakeGuard::new(mode, new_until);
    self
      .signal
      .send(Some(wake_guard))
      .await
      .expect("failed to send signal");
    Ok(())
  }

  #[zbus(property, name = "StatusReport")]
  async fn status_report(&self) -> String {
    self.inner.read().await.report.json()
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
    mode: String,
    update: DurationUpdate,
  ) -> zbus::Result<()>;

  #[zbus(property)]
  fn status_report(&self) -> zbus::Result<String>;
}

async fn msg(
  mode: Option<WakeMode>,
  update: DurationUpdate,
) -> Result<(), zbus::Error> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusWakeGuardProxy::new(&conn).await?;
  let mode = mode.map(|m| m.to_string()).unwrap_or_default();
  proxy.update(mode, update).await?;
  Ok(())
}

async fn monitor() -> zbus::Result<()> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusWakeGuardProxy::new(&conn).await?;
  let report = proxy.status_report().await?;
  println!("{}", report);

  let mut stream = proxy.receive_status_report_changed().await;
  while let Some(changed) = stream.next().await {
    let report = changed.get().await?;
    println!("{}", report);
  }

  Ok(())
}

async fn monitor_forever() -> zbus::Result<()> {
  loop {
    match monitor().await {
      Ok(_) => continue,
      Err(zbus::Error::MethodError(_, _, _)) => {
        tokio::time::sleep(Duration::from_secs(5)).await
      }
      Err(e) => return Err(e),
    }
  }
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt::init();

  let cli = Cli::parse();

  match cli.cmd {
    Commands::Daemon => {
      Daemon::default().run().await.expect("Failed to run daemon");
    }
    Commands::Msg { mode, update } => {
      msg(mode, update).await.expect("Failed to update");
    }
    Commands::Monitor => {
      monitor_forever().await.expect("Failed to monitor");
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
