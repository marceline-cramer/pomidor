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

use std::fmt::Debug;

use anyhow::{bail, Context};
use flume::unbounded;
use flume::{Receiver, Sender};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
