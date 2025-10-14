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

use std::fmt::{Display, Formatter};

use chrono::{DateTime, Local};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TimerState {
    pub started: DateTime<Local>,
    pub deadline: Option<DateTime<Local>>,
    pub short_breaks: u8,
    pub mode: TimerMode,
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
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
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
