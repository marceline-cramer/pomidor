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

use std::{
    fmt::{Debug, Display},
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use flume::{unbounded, Receiver, Sender};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

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
            Command::Daemon => Daemon::run(config, &socket).await,
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

        Self {
            text,
            class,
            alt: "".to_string(),     // TODO: ?
            tooltip: "".to_string(), // TODO: show number of breaks left and custom reminders
            percentage: 0,           // TODO: how to use this effectively?
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

#[derive(Clone)]
pub struct Daemon {
    state_change_tx: broadcast::Sender<TimerState>,
    message_tx: Sender<ClientMessage>,
}

impl Daemon {
    pub async fn run(config: Config, socket: &PathBuf) -> anyhow::Result<()> {
        // test if there is already a daemon running on the given socket
        use std::io::ErrorKind;
        match UnixStream::connect(&socket).await {
            Ok(_) => bail!("another instance of Pomidor is already running on the given socket"),
            Err(ref err) if err.kind() == ErrorKind::ConnectionRefused => {
                eprintln!("Found leftover socket; removing.");
                std::fs::remove_file(socket).context("failed to remove old socket")?;
            }
            Err(ref err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => bail!("error testing if old daemon is running: {err:?}"),
        }

        // bind to the socket ourselves
        let listener = UnixListener::bind(socket).context("failed to bind to socket")?;

        // create channels to talk to the main daemon thread
        let (state_change_tx, _) = broadcast::channel(128);
        let (message_tx, message_rx) = unbounded();

        // spawn the main daemon thread
        tokio::spawn(Self::run_inner(
            config,
            state_change_tx.clone(),
            message_rx,
            socket.clone(),
        ));

        // create a base daemon to respond to clients with
        let daemon = Self {
            state_change_tx,
            message_tx,
        };

        // TODO: gracefully handle interrupt signals
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let (rx, tx) = stream.into_split();
                    let conn = Connection::<DaemonMessage, ClientMessage>::new(rx, tx);
                    let daemon = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(err) = daemon.on_client(conn).await {
                            eprintln!("client error: {err:#?}");
                        }
                    });
                }
                Err(err) => {
                    eprintln!("connection error: {err:?}");
                }
            }
        }
    }

    async fn run_inner(
        mut config: Config,
        state_change_tx: broadcast::Sender<TimerState>,
        message_rx: Receiver<ClientMessage>,
        socket: PathBuf,
    ) {
        let mut timer = TimerState::new(&config);
        let mut old_timer = timer.clone();

        loop {
            match timer.deadline.as_mut() {
                // if no deadline is set, just wait for message
                None => match message_rx.recv_async().await {
                    Ok(msg) => {
                        Self::on_message(&mut config, &state_change_tx, &mut timer, msg).await;
                    }
                    // connection closed; abort
                    Err(_) => break,
                },
                // otherwise, select soonest from deadline and rx
                Some(deadline) => {
                    let now = Local::now();
                    if now > *deadline {
                        timer.on_deadline(&config);
                    } else {
                        let since = deadline.signed_duration_since(now);
                        let now = std::time::Instant::now();
                        let until = now + since.to_std().expect("failed to calculate deadline");
                        let sleep = tokio::time::sleep_until(until.into());
                        let msg = message_rx.recv_async();
                        tokio::select! {
                            _ = sleep => timer.on_deadline(&config),
                            msg = msg => {
                                let Ok(msg) = msg else {
                                    break;
                                };

                                Self::on_message(
                                    &mut config,
                                    &state_change_tx,
                                    &mut timer,
                                    msg,
                                ).await;
                            }
                        }
                    }
                }
            }

            // always broadcast timer changes
            if timer != old_timer {
                let _ = state_change_tx.send(timer.clone());

                if timer.mode != old_timer.mode {
                    if let Some(msg) = config.notification.for_mode(timer.mode) {
                        let result = notify_rust::Notification::new()
                            .summary(&timer.mode.to_string())
                            .body(msg)
                            .timeout(-1)
                            .show_async()
                            .await;

                        if let Err(err) = result {
                            eprintln!("failed to show notification: {err:?}");
                        }
                    }
                }

                old_timer = timer.clone();
            }
        }

        // remove old socket
        let _ = std::fs::remove_file(socket);
    }

    pub async fn on_message(
        config: &mut Config,
        state_change_tx: &broadcast::Sender<TimerState>,
        timer: &mut TimerState,
        msg: ClientMessage,
    ) {
        use ClientMessage::*;
        match msg {
            Ping => state_change_tx.send(timer.clone()).map(|_| ()).unwrap(),
            Update(update) => timer.on_update(config, update),
            ReloadConfig => {}   // TODO
            _ => unreachable!(), // responded to by on_client
        }
    }

    pub async fn on_client(
        &self,
        conn: Connection<DaemonMessage, ClientMessage>,
    ) -> anyhow::Result<()> {
        while let Ok(msg) = conn.rx.recv_async().await {
            conn.tx
                .send(DaemonMessage::Ack)
                .context("failed to send ack")?;

            use ClientMessage::*;
            match msg {
                Ping => {} // already ack'd
                Watch => {
                    self.message_tx
                        .send(ClientMessage::Ping)
                        .context("failed to ping state changes")?;

                    let mut rx = self.state_change_tx.subscribe();
                    while let Ok(state) = rx.recv().await {
                        conn.tx
                            .send(DaemonMessage::StateChange(state))
                            .context("failed to send state change to client")?;
                    }

                    // can keep receiving pending rx messages after tx closes
                    break;
                }
                msg => {
                    self.message_tx
                        .send(msg)
                        .context("failed to relay message to daemon thread")?;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ClientMessage {
    Ping,
    Watch,
    Update(TimerUpdate),

    // TODO: live config file updates?
    ReloadConfig,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum DaemonMessage {
    Ack,
    StateChange(TimerState),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TimerState {
    pub deadline: Option<DateTime<Local>>,
    pub short_breaks: u8,
    pub mode: TimerMode,
}

impl TimerState {
    pub fn new(config: &Config) -> Self {
        Self {
            deadline: None,
            short_breaks: config.timer.short_breaks,
            mode: TimerMode::Pestering,
        }
    }

    pub fn on_deadline(&mut self, config: &Config) {
        let now = Local::now();

        use TimerMode::*;
        match self.mode {
            Pestering => {
                // will not be reached by deadline but can occur on update
                self.short_breaks = config.timer.short_breaks;
                self.deadline = Some(now + config.timer.focus_duration);
                self.mode = Focus;
            }
            Snoozing => {
                self.deadline = None;
                self.mode = Pestering;
            }
            ShortBreak | LongBreak => {
                self.deadline = Some(now + config.timer.focus_duration);
                self.mode = Focus;
            }
            Focus => {
                if self.short_breaks == 0 {
                    self.short_breaks = config.timer.short_breaks;
                    self.deadline = Some(now + config.timer.long_break_duration);
                    self.mode = LongBreak;
                } else {
                    self.short_breaks -= 1;
                    self.deadline = Some(now + config.timer.short_break_duration);
                    self.mode = ShortBreak;
                }
            }
        }
    }

    pub fn on_update(&mut self, config: &Config, update: TimerUpdate) {
        use TimerMode::*;
        use TimerUpdate::*;
        match (update, &self.mode) {
            (Skip, _) => self.on_deadline(config),
            (Reset, _) => {
                self.short_breaks = config.timer.short_breaks;
                self.deadline = None;
                self.mode = Pestering;
            }
            (Snooze { time }, Pestering) => {
                let now = Local::now();
                self.deadline = Some(now + time);
                self.mode = Snoozing;
            }
            (Snooze { time }, Snoozing) => {
                let deadline = self.deadline.unwrap_or(Local::now());
                self.deadline = Some(deadline + time);
            }
            (Snooze { .. }, _) => {}
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum TimerMode {
    /// Hey. You should be using the Pomodoro Technique.
    Pestering,

    /// Okay, I guess you can use me later...
    Snoozing,

    /// Short break period.
    ShortBreak,

    /// Long break period.
    LongBreak,

    /// Time to focus!
    Focus,
}

impl Display for TimerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use TimerMode::*;
        let name = match self {
            Pestering => "Pestering",
            Snoozing => "Snoozing",
            ShortBreak => "Short break",
            LongBreak => "Long break",
            Focus => "Focus",
        };

        write!(f, "{name}")
    }
}

#[derive(Debug, Deserialize, Serialize, Subcommand)]
pub enum TimerUpdate {
    /// Skips this timer and goes to the next one.
    Skip,

    /// Resets the timer to its default state (pestering with all breaks).
    Reset,

    /// Adds snooze time to pester or snooze modes.
    Snooze {
        /// The amount of time to snooze for.
        #[serde(with = "humantime_serde")]
        #[clap(value_parser = humantime::parse_duration)]
        time: std::time::Duration,
    },
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

        let config_path = self
            .config
            .clone()
            .or_else(|| {
                dirs::config_local_dir().map(|path| path.join("pomidor").join("pomidor.toml"))
            })
            .context("failed to locate config directory")?;

        if config_path.exists() {
            let toml =
                std::fs::read_to_string(config_path).context("failed to read config file")?;
            toml::from_str(&toml).context("failed to deserialize config file")
        } else {
            // TODO: create a parent directory if necessary
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
    pub fn for_mode(&self, mode: TimerMode) -> Option<&String> {
        use TimerMode::*;
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

pub struct Connection<T, R> {
    pub tx: Sender<T>,
    pub rx: Receiver<R>,
}

impl<T: Debug + Serialize + Send + 'static, R: Debug + DeserializeOwned + Send + 'static>
    Connection<T, R>
{
    pub fn new(
        mut rx: impl AsyncRead + Unpin + Send + 'static,
        mut tx: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let (outgoing_tx, outgoing_rx) = unbounded();
        let (incoming_tx, incoming_rx) = unbounded();

        tokio::spawn(async move {
            while let Ok(op) = outgoing_rx.recv_async().await {
                let payload = serde_json::to_vec(&op).unwrap();
                let len = payload.len() as u32;
                tx.write_u32_le(len).await.unwrap();
                tx.write_all(&payload).await.unwrap();
                tx.flush().await.unwrap();
            }
        });

        #[allow(clippy::read_zero_byte_vec)]
        tokio::spawn(async move {
            let mut buf = Vec::new();
            while let Ok(len) = rx.read_u32_le().await {
                buf.resize(len as usize, 0);
                rx.read_exact(&mut buf).await.unwrap();
                let op = serde_json::from_slice(&buf).unwrap();
                if incoming_tx.send(op).is_err() {
                    break;
                }
            }
        });

        Self {
            tx: outgoing_tx,
            rx: incoming_rx,
        }
    }
}

impl Connection<ClientMessage, DaemonMessage> {
    pub async fn expect_ack(&self) -> anyhow::Result<()> {
        // TODO: support timeout
        let msg = self
            .rx
            .recv_async()
            .await
            .context("failed to receive ACK")?;

        if msg != DaemonMessage::Ack {
            bail!("expected ACK from server, got {msg:?}");
        } else {
            Ok(())
        }
    }
}
