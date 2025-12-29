use anyhow::Result;
use bytes::Bytes;
use russh::client::Msg;
use russh::Channel;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::*;
use uuid::Uuid;
use warpgate_common::SessionId;

use super::error::SshClientError;
use crate::{ChannelOperation, RCEvent};

use tokio::process::Child;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub enum SessionChannelBackend {
    Russh(Channel<Msg>),
    Process(Child),
}

pub struct SessionChannel {
    backend: SessionChannelBackend,
    channel_id: Uuid,
    ops_rx: UnboundedReceiver<ChannelOperation>,
    events_tx: UnboundedSender<RCEvent>,
    session_id: SessionId,
    closed: bool,
}

impl SessionChannel {
    pub fn new(
        backend: SessionChannelBackend,
        channel_id: Uuid,
        ops_rx: UnboundedReceiver<ChannelOperation>,
        events_tx: UnboundedSender<RCEvent>,
        session_id: SessionId,
    ) -> Self {
        SessionChannel {
            backend,
            channel_id,
            ops_rx,
            events_tx,
            session_id,
            closed: false,
        }
    }

    pub async fn run(mut self) -> Result<(), SshClientError> {
        let mut stdin = None;
        let mut stdout = None;
        let mut stderr = None;

        if let SessionChannelBackend::Process(ref mut child) = self.backend {
            stdin = child.stdin.take();
            stdout = child.stdout.take();
            stderr = child.stderr.take();
        }

        loop {
            tokio::select! {
                incoming_data = self.ops_rx.recv() => {
                    match incoming_data {
                        Some(ChannelOperation::Data(data)) => {
                            match &mut self.backend {
                                SessionChannelBackend::Russh(ch) => { ch.data(&*data).await?; }
                                SessionChannelBackend::Process(_) => {
                                    if let Some(stdin) = &mut stdin {
                                        stdin.write_all(&data).await?;
                                    }
                                }
                            }
                        }
                        Some(ChannelOperation::ExtendedData { ext, data }) => {
                            match &mut self.backend {
                                SessionChannelBackend::Russh(ch) => { ch.extended_data(ext, &*data).await?; }
                                SessionChannelBackend::Process(_) => {} // Stderr usually implies stream 1?
                            }
                        }
                        // ... PTY requests etc (ignored for process or partially supported)
                        Some(ChannelOperation::RequestPty(request)) => {
                            if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.request_pty(
                                    true,
                                    &request.term,
                                    request.col_width,
                                    request.row_height,
                                    request.pix_width,
                                    request.pix_height,
                                    &request.modes,
                                ).await?;
                            }
                        }
                        Some(ChannelOperation::ResizePty(request)) => {
                             if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.window_change(
                                    request.col_width,
                                    request.row_height,
                                    request.pix_width,
                                    request.pix_height,
                                ).await?;
                             }
                        },
                        Some(ChannelOperation::RequestShell) => {
                             if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.request_shell(true).await?;
                             }
                        },
                        Some(ChannelOperation::RequestEnv(name, value)) => {
                              if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.set_env(false, name, value).await?;
                              }
                        },
                        Some(ChannelOperation::RequestExec(command)) => {
                              if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.exec(false, command).await?;
                              }
                        },
                         Some(ChannelOperation::RequestSubsystem(name)) => {
                               if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.request_subsystem(false, &name).await?;
                               }
                        },
                        Some(ChannelOperation::Eof) => {
                             if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.eof().await?;
                             }
                        },
                        Some(ChannelOperation::Signal(signal)) => {
                              if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.signal(signal).await?;
                              }
                        },
                         // ...
                        Some(ChannelOperation::RequestX11(request)) => {
                             if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.request_x11(
                                    true,
                                    request.single_conection,
                                    request.x11_auth_protocol,
                                    request.x11_auth_cookie,
                                    request.x11_screen_number,
                                ).await?;
                             }
                        },
                        Some(ChannelOperation::AgentForward) => {
                             if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                                ch.agent_forward(true).await?;
                             }
                        }
                        Some(ChannelOperation::Close) => break,
                        None => break,
                         _ => {}
                    }
                }
                // Handle Backend Events
                // For Russh
                 channel_event = async {
                    if let SessionChannelBackend::Russh(ch) = &mut self.backend {
                        ch.wait().await
                    } else {
                        futures::future::pending().await
                    }
                 } => {
                     // ... existing russh handling ...
                     match channel_event {
                        Some(russh::ChannelMsg::Data { data }) => {
                            let bytes: &[u8] = &data;
                            debug!("channel data: {bytes:?}");
                            self.events_tx.send(RCEvent::Output(
                                self.channel_id,
                                Bytes::from(bytes.to_vec()),
                            )).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(russh::ChannelMsg::Close) => {
                            break;
                        },
                        Some(russh::ChannelMsg::Success) => {
                            self.events_tx.send(RCEvent::Success(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        },
                        Some(russh::ChannelMsg::Failure) => {
                            self.events_tx.send(RCEvent::ChannelFailure(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        },
                        Some(russh::ChannelMsg::Eof) => {
                            self.events_tx.send(RCEvent::Eof(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            self.events_tx.send(RCEvent::ExitStatus(self.channel_id, exit_status)).map_err(|_| SshClientError::MpscError)?;
                        }
                        // ...
                        Some(russh::ChannelMsg::ExtendedData { data, ext }) => {
                            let data: &[u8] = &data;
                            self.events_tx.send(RCEvent::ExtendedData {
                                channel: self.channel_id,
                                data: Bytes::from(data.to_vec()),
                                ext,
                            }).map_err(|_| SshClientError::MpscError)?;
                        }
                        _ => {}
                     }
                 }
                 // Handle Stdout for Process
                 read_result = async {
                     if let Some(stdout) = &mut stdout {
                         let mut buf = [0u8; 4096];
                         stdout.read(&mut buf).await.map(|n| (n, buf))
                     } else {
                         futures::future::pending().await
                     }
                 } => {
                     match read_result {
                         Ok((n, buf)) if n > 0 => {
                             self.events_tx.send(RCEvent::Output(self.channel_id, Bytes::from(buf[..n].to_vec()))).map_err(|_| SshClientError::MpscError)?;
                         }
                         _ => break, // EOF or error
                     }
                 }
                 // Handle Stderr for Process
                  read_err_result = async {
                     if let Some(stderr) = &mut stderr {
                         let mut buf = [0u8; 4096];
                         stderr.read(&mut buf).await.map(|n| (n, buf))
                     } else {
                         futures::future::pending().await
                     }
                 } => {
                      match read_err_result {
                         Ok((n, buf)) if n > 0 => {
                             // Send as ExtendedData?
                              self.events_tx.send(RCEvent::ExtendedData {
                                channel: self.channel_id,
                                data: Bytes::from(buf[..n].to_vec()),
                                ext: 1,
                            }).map_err(|_| SshClientError::MpscError)?;
                         }
                         _ => {}
                     }
                 }
            }
        }
        self.close()?;
        Ok(())
    }
            channel_id,
            ops_rx,
            events_tx,
            session_id,
            closed: false,
        }
    }

    pub async fn run(mut self) -> Result<(), SshClientError> {
        loop {
            tokio::select! {
                incoming_data = self.ops_rx.recv() => {
                    match incoming_data {
                        Some(ChannelOperation::Data(data)) => {
                            self.client_channel.data(&*data).await?;
                        }
                        Some(ChannelOperation::ExtendedData { ext, data }) => {
                            self.client_channel.extended_data(ext, &*data).await?;
                        }
                        Some(ChannelOperation::RequestPty(request)) => {
                            self.client_channel.request_pty(
                                true,
                                &request.term,
                                request.col_width,
                                request.row_height,
                                request.pix_width,
                                request.pix_height,
                                &request.modes,
                            ).await?;
                        }
                        Some(ChannelOperation::ResizePty(request)) => {
                            self.client_channel.window_change(
                                request.col_width,
                                request.row_height,
                                request.pix_width,
                                request.pix_height,
                            ).await?;
                        },
                        Some(ChannelOperation::RequestShell) => {
                            self.client_channel.request_shell(true).await?;
                        },
                        Some(ChannelOperation::RequestEnv(name, value)) => {
                            self.client_channel.set_env(false, name, value).await?;
                        },
                        Some(ChannelOperation::RequestExec(command)) => {
                            self.client_channel.exec(false, command).await?;
                        },
                        Some(ChannelOperation::RequestSubsystem(name)) => {
                            self.client_channel.request_subsystem(false, &name).await?;
                        },
                        Some(ChannelOperation::Eof) => {
                            self.client_channel.eof().await?;
                        },
                        Some(ChannelOperation::Signal(signal)) => {
                            self.client_channel.signal(signal).await?;
                        },
                        Some(ChannelOperation::OpenShell) => unreachable!(),
                        Some(ChannelOperation::OpenDirectTCPIP { .. }) => unreachable!(),
                        Some(ChannelOperation::OpenDirectStreamlocal { .. }) => unreachable!(),
                        Some(ChannelOperation::OpenX11 { .. }) => unreachable!(),
                        Some(ChannelOperation::RequestX11(request)) => {
                            self.client_channel.request_x11(
                                true,
                                request.single_conection,
                                request.x11_auth_protocol,
                                request.x11_auth_cookie,
                                request.x11_screen_number,
                            ).await?;
                        },
                        Some(ChannelOperation::AgentForward) => {
                            self.client_channel.agent_forward(
                                true,
                            ).await?;
                        }
                        Some(ChannelOperation::Close) => break,
                        None => break,
                    }
                }
                channel_event = self.client_channel.wait() => {
                    match channel_event {
                        Some(russh::ChannelMsg::Data { data }) => {
                            let bytes: &[u8] = &data;
                            debug!("channel data: {bytes:?}");
                            self.events_tx.send(RCEvent::Output(
                                self.channel_id,
                                Bytes::from(bytes.to_vec()),
                            )).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(russh::ChannelMsg::Close) => {
                            break;
                        },
                        Some(russh::ChannelMsg::Success) => {
                            self.events_tx.send(RCEvent::Success(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        },
                        Some(russh::ChannelMsg::Failure) => {
                            self.events_tx.send(RCEvent::ChannelFailure(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        },
                        Some(russh::ChannelMsg::Eof) => {
                            self.events_tx.send(RCEvent::Eof(self.channel_id)).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            self.events_tx.send(RCEvent::ExitStatus(self.channel_id, exit_status)).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(russh::ChannelMsg::WindowAdjusted { .. }) => { },
                        Some(russh::ChannelMsg::ExitSignal {
                            core_dumped, error_message, lang_tag, signal_name
                        }) => {
                            self.events_tx.send(RCEvent::ExitSignal {
                                channel: self.channel_id, core_dumped, error_message, lang_tag, signal_name
                            }).map_err(|_| SshClientError::MpscError)?;
                        },
                        Some(russh::ChannelMsg::XonXoff { client_can_do: _ }) => {
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, ext }) => {
                            let data: &[u8] = &data;
                            self.events_tx.send(RCEvent::ExtendedData {
                                channel: self.channel_id,
                                data: Bytes::from(data.to_vec()),
                                ext,
                            }).map_err(|_| SshClientError::MpscError)?;
                        }
                        Some(msg) => {
                            warn!("unhandled channel message: {:?}", msg);
                        }
                        None => {
                            break
                        },
                    }
                }
            }
        }
        self.close()?;
        Ok(())
    }

    fn close(&mut self) -> Result<(), SshClientError> {
        if !self.closed {
            let _ = self
                .events_tx
                .send(RCEvent::Close(self.channel_id))
                .map_err(|_| SshClientError::MpscError);
            self.closed = true;
        }
        Ok(())
    }
}

impl Drop for SessionChannel {
    fn drop(&mut self) {
        let _ = self.close();
        info!(channel=%self.channel_id, session=%self.session_id, "Closed");
    }
}
