use std::{str::FromStr as _, time::Duration};

use duration_string::DurationString;

use crate::protocol::DurationUpdate;

pub fn parse_duration_update(s: &str) -> Result<DurationUpdate, String> {
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
