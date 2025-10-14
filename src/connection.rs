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
use std::time::Duration;

use anyhow::{bail, Context};
use flume::unbounded;
use flume::{Receiver, Sender};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::{ClientMessage, DaemonMessage, TimerState};

pub struct Connection<T, R> {
    pub tx: Sender<T>,
    pub rx: Receiver<R>,
}

impl Connection<ClientMessage, DaemonMessage> {
    pub async fn expect_ack(&self) -> anyhow::Result<()> {
        // wrap receive future with timeout
        let timeout = std::time::Duration::from_secs(5);
        let fut = self.rx.recv_async();

        // receive message within timeout
        let msg = tokio::time::timeout(timeout, fut)
            .await
            .context("daemon ACK timed out")?
            .context("connection closed while waiting for ACK")?;

        // confirm it is an ACK message
        if msg != DaemonMessage::Ack {
            bail!("expected ACK from server, got {msg:?}");
        } else {
            Ok(())
        }
    }

    /// Subscribes to state changes and invokes a callback until the connection is dropped.
    pub async fn subscribe(&self, cb: impl FnMut(TimerState)) -> anyhow::Result<()> {
        // subscribe with no interval
        self.subscribe_interval(Duration::default(), cb).await
    }

    /// Invokes a callback in response to state changes or at least as often as the given interval.
    pub async fn subscribe_interval(
        &self,
        interval: std::time::Duration,
        mut cb: impl FnMut(TimerState),
    ) -> anyhow::Result<()> {
        // begin subscription
        self.watch().await?;

        // preserve state between loops
        let mut state = None;

        // watch for state changes until the connection is dropped
        loop {
            // create watch future
            let watch = self.next_change();

            // if interval is zero, just invoke callback immediately.
            // otherwise, wait for interval
            let result = if interval.is_zero() || state.is_none() {
                watch.await
            } else if let Ok(result) = tokio::time::timeout(interval, watch).await {
                result
            } else {
                // on timeout, invoke callback with current state and continue
                cb(state.clone().unwrap());
                continue;
            };

            // extract state handle disconnection
            let Some(new_state) = result? else {
                break;
            };

            // invoke callback
            cb(new_state.clone());

            // persist state
            state = Some(new_state);
        }

        // completed successfully
        Ok(())
    }

    /// Begins a watch operation on the daemon.
    pub async fn watch(&self) -> anyhow::Result<()> {
        // send watch message
        self.tx
            .send(ClientMessage::Watch)
            .context("failed to send watch message")?;

        // expect response
        self.expect_ack().await?;

        // great success!
        Ok(())
    }

    /// Listens for an incoming [DaemonMessage::StateChange] message and discards other messages.
    pub async fn next_change(&self) -> anyhow::Result<Option<TimerState>> {
        loop {
            // extract incoming message or handle connection close
            let msg = match self.rx.recv_async().await {
                Ok(msg) => msg,
                Err(flume::RecvError::Disconnected) => return Ok(None),
            };

            // discard non-state change messages
            if let DaemonMessage::StateChange(state) = msg {
                return Ok(Some(state));
            }
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
