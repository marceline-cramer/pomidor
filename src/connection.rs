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

use anyhow::{bail, Context};
use flume::{Receiver, Sender};

use crate::daemon::{ClientMessage, DaemonMessage};

pub struct Connection<T, R> {
    pub tx: Sender<T>,
    pub rx: Receiver<R>,
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
