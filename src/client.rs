use std::time::{Duration, SystemTime};

use futures::StreamExt as _;
use serde::Serialize;

use crate::protocol::{DbusVigilareProxy, DurationUpdate, Status};

pub async fn msg(update: DurationUpdate) -> Result<(), zbus::Error> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusVigilareProxy::new(&conn).await?;
  proxy.update(update).await?;
  Ok(())
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
struct StatusReport {
  active: bool,
  remaining_seconds: Option<u64>,
  message: String,
}

impl StatusReport {
  fn json(&self) -> String {
    serde_json::to_string(&self).expect("failed to serialize report")
  }

  fn from_status(msg: Status) -> Self {
    let epoch = Duration::from_secs(msg.wake_until);
    let now = SystemTime::now();
    let duration = (SystemTime::UNIX_EPOCH + epoch)
      .duration_since(now)
      .unwrap_or_default();

    let remaining_min = duration.as_secs_f32() / 60.0;
    let message = if msg.active {
      format!("{}m", remaining_min.ceil() as u64)
    } else {
      String::default()
    };

    let remaining_seconds = msg.active.then_some((remaining_min * 60.0) as u64);

    Self {
      active: msg.active,
      remaining_seconds,
      message,
    }
  }
}

async fn monitor() -> zbus::Result<()> {
  let conn = zbus::Connection::session().await?;
  let proxy = DbusVigilareProxy::new(&conn).await?;
  let status = proxy.status().await?;
  let report = StatusReport::from_status(status);
  let json = report.json();
  println!("{}", json);

  let mut stream = proxy.receive_status_changed().await;
  while let Some(changed) = stream.next().await {
    let status = changed.get().await?;
    let report = StatusReport::from_status(status);
    let json = report.json();
    println!("{}", json);
  }

  Ok(())
}

pub async fn monitor_forever() -> zbus::Result<()> {
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
