use anyhow::{Context as _, Result, anyhow};
use gpui::{Context, Entity};
use remote::{RemoteClient, SshPortForwardOption};
use smol::process::Child;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use util::command::new_smol_command;

use crate::persistence::ConnectionDb;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortForwardState {
    Active,
    Pending,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardStatus {
    pub spec: SshPortForwardOption,
    pub state: PortForwardState,
    pub message: Option<String>,
}

pub struct PortForwardManager {
    remote_client: Entity<RemoteClient>,
    connection_profile_id: Option<i64>,
    desired_forwards: Vec<SshPortForwardOption>,
    status_by_forward: Vec<PortForwardStatus>,
    active_child: Option<Child>,
    suspended: bool,
    last_error: Option<String>,
}

impl PortForwardManager {
    pub fn new(
        remote_client: Entity<RemoteClient>,
        connection_profile_id: Option<i64>,
        desired_forwards: Vec<SshPortForwardOption>,
    ) -> Self {
        let status_by_forward = desired_forwards
            .iter()
            .cloned()
            .map(|spec| PortForwardStatus {
                spec,
                state: PortForwardState::Pending,
                message: None,
            })
            .collect();
        Self {
            remote_client,
            connection_profile_id,
            desired_forwards,
            status_by_forward,
            active_child: None,
            suspended: false,
            last_error: None,
        }
    }

    pub fn set_suspended(&mut self, suspended: bool, cx: &mut Context<Self>) {
        if self.suspended == suspended {
            return;
        }
        self.suspended = suspended;
        if suspended {
            self.stop_active_child();
            self.status_by_forward = self
                .desired_forwards
                .iter()
                .cloned()
                .map(|spec| PortForwardStatus {
                    spec,
                    state: PortForwardState::Pending,
                    message: Some("Paused while app is backgrounded".to_string()),
                })
                .collect();
            cx.notify();
        } else {
            self.restart(cx);
        }
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let desired = self.desired_forwards.clone();
        self.apply_forwards(desired, cx);
    }

    pub fn apply_forwards(&mut self, desired: Vec<SshPortForwardOption>, cx: &mut Context<Self>) {
        let previous_desired = self.desired_forwards.clone();
        self.desired_forwards = desired.clone();
        self.last_error = None;
        self.status_by_forward = desired
            .iter()
            .cloned()
            .map(|spec| PortForwardStatus {
                spec,
                state: PortForwardState::Pending,
                message: Some("Applying...".to_string()),
            })
            .collect();
        self.stop_active_child();
        let remote_client = self.remote_client.clone();
        let connection_profile_id = self.connection_profile_id;

        if self.suspended || desired.is_empty() {
            self.status_by_forward = desired
                .iter()
                .cloned()
                .map(|spec| PortForwardStatus {
                    spec,
                    state: PortForwardState::Pending,
                    message: if self.suspended {
                        Some("Paused while app is backgrounded".to_string())
                    } else {
                        None
                    },
                })
                .collect();
            cx.notify();
            return;
        }

        cx.spawn(async move |this, cx| {
            let apply_result = validate_forwards(&desired).and_then(|validated| {
                let tuples = validated
                    .iter()
                    .map(|forward| {
                        (
                            forward.local_port,
                            forward
                                .remote_host
                                .clone()
                                .unwrap_or_else(|| "localhost".to_string()),
                            forward.remote_port,
                        )
                    })
                    .collect::<Vec<_>>();
                let command = remote_client
                    .read_with(cx, |client, _| client.build_forward_ports_command(tuples))??;
                let child = new_smol_command(command.program)
                    .args(command.args)
                    .envs(command.env)
                    .spawn()
                    .context("spawning forwarded-port process")?;
                Ok::<_, anyhow::Error>(child)
            });

            this.update(cx, |this, cx| match apply_result {
                Ok(child) => {
                    this.active_child = Some(child);
                    this.status_by_forward = this
                        .desired_forwards
                        .iter()
                        .cloned()
                        .map(|spec| PortForwardStatus {
                            spec,
                            state: PortForwardState::Active,
                            message: None,
                        })
                        .collect();
                    this.last_error = None;

                    if let Some(connection_profile_id) = connection_profile_id
                        && let Err(err) = ConnectionDb::open().and_then(|db| {
                            db.update_connection_profile_port_forwards(
                                connection_profile_id,
                                &this.desired_forwards,
                            )
                        })
                    {
                        log::warn!("[Zed iOS] Failed to persist updated port forwards: {err}");
                    }

                    cx.notify();
                }
                Err(err) => {
                    let error_message = format!("{err:#}");
                    this.desired_forwards = previous_desired.clone();
                    this.status_by_forward = previous_desired
                        .iter()
                        .cloned()
                        .map(|spec| PortForwardStatus {
                            spec,
                            state: PortForwardState::Failed,
                            message: Some(error_message.clone()),
                        })
                        .collect();
                    this.last_error = Some(error_message);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn stop_active_child(&mut self) {
        if let Some(mut child) = self.active_child.take() {
            let _ = child.kill();
        }
    }
}

fn validate_forwards(forwards: &[SshPortForwardOption]) -> Result<Vec<SshPortForwardOption>> {
    let mut seen_ports = std::collections::HashSet::new();
    for forward in forwards {
        if !seen_ports.insert((forward.local_host.clone(), forward.local_port)) {
            return Err(anyhow!(
                "Local port {} is configured more than once",
                forward.local_port
            ));
        }

        let local_host = forward
            .local_host
            .as_deref()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let socket_addr = SocketAddr::new(local_host, forward.local_port);
        TcpListener::bind(socket_addr)
            .with_context(|| format!("Local port {} is unavailable", forward.local_port))?;
    }
    Ok(forwards.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward(local_port: u16, remote_port: u16) -> SshPortForwardOption {
        SshPortForwardOption {
            local_host: None,
            local_port,
            remote_host: Some("localhost".to_string()),
            remote_port,
        }
    }

    #[test]
    fn validate_forwards_rejects_duplicate_local_ports() {
        let forwards = vec![forward(8080, 8080), forward(8080, 3000)];
        let error = validate_forwards(&forwards).expect_err("expected duplicate local port error");
        assert!(error.to_string().contains("configured more than once"));
    }

    #[test]
    fn validate_forwards_accepts_distinct_local_ports() {
        let forwards = vec![forward(38080, 8080), forward(39090, 9090)];
        let validated = validate_forwards(&forwards).expect("expected valid forwards");
        assert_eq!(validated, forwards);
    }
}
