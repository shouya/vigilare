use std::time::Duration;

use serde::{Deserialize, Serialize};
use zbus::zvariant::{self};

#[derive(Debug, Clone, Serialize, Deserialize, zvariant::Type)]
pub enum DurationUpdate {
  Add(Duration),
  Sub(Duration),
  Set(Duration),
}

#[derive(
  Debug,
  Clone,
  Serialize,
  Deserialize,
  zvariant::Type,
  zvariant::Value,
  zvariant::OwnedValue,
)]
pub struct Status {
  pub active: bool,
  // UNIX epoch time
  pub wake_until: u64,
}

#[zbus::proxy(
  interface = "org.shou.Vigilare",
  default_service = "org.shou.Vigilare",
  default_path = "/org/shou/Vigilare"
)]
trait DbusVigilare {
  async fn update(&self, update: DurationUpdate) -> zbus::Result<()>;

  #[zbus(property)]
  fn status(&self) -> zbus::Result<Status>;
}
