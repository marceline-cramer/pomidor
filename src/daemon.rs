// Copyright (c) 2025 Marceline Cramer
// Copyright (c) 2025 Nea Roux
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
    fs::File,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{bail, Context};
use chrono::{DateTime, Local};
use clap::Subcommand;
use flume::{unbounded, Receiver, Sender};
use rodio::{
    cpal::{self, traits::HostTrait},
    DeviceTrait, OutputStream, OutputStreamHandle,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

use crate::{Config, Connection};

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum DaemonMessage {
    Ack,
    StateChange(TimerState),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ClientMessage {
    Ping,
    Watch,
    Update(TimerUpdate),

    // TODO: live config file updates?
    ReloadConfig,
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

        // initialize sound device from configured device or use default
        let (_, stream_handle) = if let Some(device) = config.sound.device.clone() {
            cpal::default_host()
                .output_devices()
                .unwrap()
                .find(|x| x.name().unwrap() == device)
                .map(|device| OutputStream::try_from_device(&device).unwrap())
                .unwrap_or_else(|| OutputStream::try_default().unwrap())
        } else {
            OutputStream::try_default().unwrap()
        };

        // spawn the main daemon thread
        tokio::spawn(Self::run_inner(
            config,
            state_change_tx.clone(),
            message_rx,
            socket.clone(),
            Arc::new(stream_handle),
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
        stream_handle: Arc<OutputStreamHandle>,
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
                    Self::on_state_change(&config, stream_handle.clone(), timer.clone()).await;
                }

                old_timer = timer.clone();
            }
        }

        // remove old socket
        let _ = std::fs::remove_file(socket);
    }

    async fn play_sound(stream_handle: Arc<OutputStreamHandle>, source: PathBuf) {
        let file = File::open(source).unwrap();
        stream_handle.play_once(file).unwrap().sleep_until_end();
    }

    async fn on_state_change(
        config: &Config,
        stream_handle: Arc<OutputStreamHandle>,
        timer: TimerState,
    ) {
        if let Some(msg) = config.notification.for_mode(timer.mode) {
            let timeout = config
                .notification
                .timeout
                .map(|d| d.as_millis() as i32)
                .unwrap_or(-1);

            let result = notify_rust::Notification::new()
                .summary(&timer.mode.to_string())
                .body(msg)
                .timeout(timeout)
                .show_async()
                .await;

            if let Err(err) = result {
                eprintln!("failed to show notification: {err:?}");
            }
        }

        if let Some(sound_path) = config.sound.sound.clone() {
            tokio::spawn(Self::play_sound(stream_handle, sound_path));
        }
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
