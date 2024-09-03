use tokio::signal::unix::SignalKind;

pub struct ExitSignals {
  sigint: tokio::signal::unix::Signal,
  sigterm: tokio::signal::unix::Signal,
}

impl ExitSignals {
  pub fn new() -> Self {
    use tokio::signal::unix::{signal, SignalKind};

    let sigterm = signal(SignalKind::terminate())
      .expect("failed to install SIGTERM handler");
    let sigint = signal(SignalKind::interrupt())
      .expect("failed to install SIGINT handler");

    Self { sigint, sigterm }
  }

  pub async fn recv(&mut self) -> SignalKind {
    tokio::select! {
      _ = self.sigterm.recv() => { SignalKind::terminate() }
      _ = self.sigint.recv() => { SignalKind::interrupt() }
    }
  }
}
