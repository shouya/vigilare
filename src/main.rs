use clap::{Parser, Subcommand};

mod client;
mod daemon;
mod helper;
mod inhibitor;
mod protocol;

use inhibitor::InhibitMode;
use protocol::DurationUpdate;

pub use daemon::Daemon;

#[derive(Parser)]
struct Cli {
  #[clap(subcommand)]
  cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the daemon
  Daemon {
    /// Inhibit mechanism
    #[clap(short, long, default_value = "xset", value_enum)]
    mode: InhibitMode,
  },

  /// Subscribe to status updates
  Monitor,

  /// Control the daemon
  Msg {
    /// Update the vigil duration. Prefix with "+" to add, "-" to
    /// subtract.  Duration syntax: "1h", "30m", "1d", etc.
    #[clap(value_parser = helper::parse_duration_update, allow_hyphen_values = true)]
    update: DurationUpdate,
  },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt::init();

  let cli = Cli::parse();

  match cli.cmd {
    Commands::Daemon { mode } => {
      let mut daemon = daemon::Daemon::new(mode).await?;
      daemon.run().await.expect("Failed to run daemon");
    }
    Commands::Msg { update } => {
      client::msg(update).await.expect("Failed to update");
    }
    Commands::Monitor => {
      client::monitor_forever().await.expect("Failed to monitor");
    }
  }

  Ok(())
}
