// Copyright (c) 2025 Marceline Cramer
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Pomidor is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version.
//
// Pomidor is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE.  See the GNU Affero General Public License for
// more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with Pomidor. If not, see <https://www.gnu.org/licenses/>.

use std::{fmt::Debug, path::PathBuf, time::Duration};

use anyhow::{bail, Context};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use connection::Connection;
use serde::{Deserialize, Serialize};

mod connection;
mod daemon;

use daemon::{ClientMessage, DaemonMessage, TimerState, TimerUpdate};

#[derive(Subcommand)]
pub enum Command {
    /// Runs the pomodoro daemon.
    Daemon,

    /// Connects to the pomodoro daemon and displays regular Waybar updates.
    Waybar,

    #[clap(flatten)]
    Update(TimerUpdate),
}

impl Command {
    pub async fn run(self, config: Config, socket: PathBuf) -> anyhow::Result<()> {
        let connect = async {
            tokio::net::UnixStream::connect(&socket)
                .await
                .context("failed to connect to socket")
                .map(|stream| {
                    let (rx, tx) = stream.into_split();
                    Connection::<ClientMessage, DaemonMessage>::new(rx, tx)
                })
        };

        match self {
            Command::Daemon => daemon::Daemon::run(config, &socket).await,
            Command::Waybar => {
                // TODO: display any encountered errors as waybar errors
                let conn = connect.await?;

                conn.tx
                    .send(ClientMessage::Watch)
                    .context("failed to send watch message")?;

                conn.expect_ack().await?;

                let init_msg = conn
                    .rx
                    .recv_async()
                    .await
                    .context("failed to receive initial timer state")?;

                let DaemonMessage::StateChange(mut timer) = init_msg else {
                    bail!("expected next daemon message to be timer state");
                };

                let mut old_output = None;
                let mut output;
                loop {
                    let timeout = std::time::Duration::from_millis(500);
                    if let Ok(msg) = tokio::time::timeout(timeout, conn.rx.recv_async()).await {
                        let Ok(msg) = msg else {
                            break;
                        };

                        if let DaemonMessage::StateChange(new_timer) = msg {
                            timer = new_timer;
                        }
                    }

                    let now = Local::now();
                    output = WaybarOutput::new(&config, &timer, now);

                    if Some(&output) != old_output.as_ref() {
                        output.print()?;
                        old_output = Some(output.clone());
                    }
                }

                WaybarOutput::error("connection to daemon closed").print()?;
                Ok(())
            }
            Command::Update(update) => {
                let conn = connect.await?;

                conn.tx
                    .send(ClientMessage::Update(update))
                    .context("failed to send update")?;

                conn.expect_ack().await
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct WaybarOutput {
    pub text: String,
    pub alt: String,
    pub tooltip: String,
    pub class: String,
    pub percentage: i64,
}

impl WaybarOutput {
    pub fn new(_config: &Config, timer: &TimerState, now: DateTime<Local>) -> Self {
        // TODO: show end time for snooze mode, not remaining time
        let remaining = timer.deadline.map(|deadline| {
            if now > deadline {
                return "00:00".to_string();
            }

            let since = deadline.signed_duration_since(now);
            let hours = since.num_hours();
            let minutes = since.num_minutes() % 60;
            let seconds = since.num_seconds() % 60;

            if hours > 0 {
                format!("{hours}:{minutes:02}:{seconds:02}")
            } else {
                format!("{minutes:02}:{seconds:02}")
            }
        });

        let mode = timer.mode.to_string();
        let text = match remaining {
            Some(remaining) => format!("{mode}: {remaining}"),
            None => mode,
        };

        // TODO: configurable? kebab-case?
        let class = format!("{:?}", timer.mode).to_lowercase();

        // TODO: configurable?
        let tooltip = format!("{} short breaks left", timer.short_breaks);

        Self {
            text,
            class,
            tooltip,
            alt: "".to_string(), // TODO: ?
            percentage: 0,       // TODO: how to use this effectively?
        }
    }

    pub fn error(msg: impl ToString) -> Self {
        Self {
            text: msg.to_string(),
            alt: "".to_string(),
            tooltip: "".to_string(),
            class: "error".to_string(),
            percentage: 0,
        }
    }

    pub fn print(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string(self).context("failed to serialize waybar output")?;
        println!("{json}");
        Ok(())
    }
}

#[derive(Parser)]
pub struct Args {
    /// A custom path to a configuration file.
    #[clap(long)]
    pub config: Option<PathBuf>,

    /// A custom path to the daemon socket.
    #[clap(long)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

impl Args {
    #[allow(unreachable_code)]
    pub fn load_config(&self) -> anyhow::Result<Config> {
        #[cfg(debug_assertions)]
        return Ok(Config::default());

        let config_dir = self
            .config
            .clone()
            .or_else(|| dirs::config_local_dir().map(|path| path.join("pomidor")))
            .context("failed to locate config directory")?;

        let config_path = config_dir.join("pomidor.toml");

        if config_path.exists() {
            let toml =
                std::fs::read_to_string(config_path).context("failed to read config file")?;
            toml::from_str(&toml).context("failed to deserialize config file")
        } else {
            // create a parent directory if necessary
            if !config_dir.exists() {
                std::fs::create_dir_all(&config_dir)
                    .context("failed to create config parent directory")?;
            }

            let config = Config::default();
            let toml =
                toml::to_string_pretty(&config).context("failed to serialize default config")?;
            std::fs::write(config_path, toml).context("failed to write to config file")?;
            Ok(config)
        }
    }

    pub fn find_socket(&self) -> anyhow::Result<PathBuf> {
        #[cfg(debug_assertions)]
        static SOCKET_NAME: &str = "pomidor-debug.sock";

        #[cfg(not(debug_assertions))]
        static SOCKET_NAME: &str = "pomidor.sock";

        self.socket
            .clone()
            .or_else(|| dirs::runtime_dir().map(|dir| dir.join(SOCKET_NAME)))
            .context("could not find socket directory")
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct Config {
    pub timer: TimerConfig,
    pub waybar: WaybarConfig,
    pub notification: NotificationConfig,
    pub sound: SoundConfig,
}

#[derive(Deserialize, Serialize)]
pub struct TimerConfig {
    /// How long each short break lasts.
    #[serde(with = "humantime_serde")]
    pub short_break_duration: Duration,

    /// How long each long break lasts.
    #[serde(with = "humantime_serde")]
    pub long_break_duration: Duration,

    /// How long the focus period lasts.
    #[serde(with = "humantime_serde")]
    pub focus_duration: Duration,

    /// The number of short breaks to take before a long break.
    pub short_breaks: u8,
}

#[derive(Default, Deserialize, Serialize)]
pub struct SoundConfig {
    #[serde(default)]
    pub sound: Option<PathBuf>,
    #[serde(default)]
    pub device: Option<String>,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            short_break_duration: Duration::from_secs(5 * 60),
            long_break_duration: Duration::from_secs(30 * 60),
            focus_duration: Duration::from_secs(25 * 60),
            short_breaks: 3,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
// TODO: per-mode labels? icons? classes?
pub struct WaybarConfig {}

#[derive(Deserialize, Serialize)]
pub struct NotificationConfig {
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,

    #[serde(default)]
    pub pestering: Option<String>,

    #[serde(default)]
    pub snoozing: Option<String>,

    #[serde(default)]
    pub short_break: Option<String>,

    #[serde(default)]
    pub long_break: Option<String>,

    #[serde(default)]
    pub focus: Option<String>,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            pestering: None,
            snoozing: None,
            short_break: Some(
                "Take a short break! Drink water, take notes, move around.".to_string(),
            ),
            long_break: Some(
                "Take a long break! Grab a snack, take notes, move around.".to_string(),
            ),
            focus: Some("Time to focus! No distractions.".to_string()),
        }
    }
}

impl NotificationConfig {
    pub fn for_mode(&self, mode: daemon::TimerMode) -> Option<&String> {
        use daemon::TimerMode::*;
        match mode {
            Pestering => self.pestering.as_ref(),
            Snoozing => self.snoozing.as_ref(),
            ShortBreak => self.short_break.as_ref(),
            LongBreak => self.long_break.as_ref(),
            Focus => self.focus.as_ref(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = args.load_config().context("failed to load configuration")?;

    let socket = args
        .find_socket()
        .context("failed to locate socket directory")?;

    args.command.run(config, socket).await
}
