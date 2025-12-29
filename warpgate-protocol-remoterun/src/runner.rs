use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::{Child, Command};
use warpgate_common::{RemoteRunMode, TargetRemoteRunOptions};

pub struct RemoteRunner {
    options: TargetRemoteRunOptions,
}

impl RemoteRunner {
    pub fn new(options: TargetRemoteRunOptions) -> Self {
        Self { options }
    }

    pub fn get_pre_login_commands(&self) -> Vec<String> {
        match &self.options.mode {
            RemoteRunMode::Bash(opts) => opts.pre_login_commands.clone().unwrap_or_default(),
            _ => vec![],
        }
    }

    pub async fn provision(&self) -> Result<(String, u16, String)> {
        match &self.options.mode {
            RemoteRunMode::Provision(opts) => {
                let url = &opts.provision_url;

                let client = reqwest::Client::new();
                let resp = client
                    .get(url)
                    .send()
                    .await?
                    .json::<serde_json::Value>()
                    .await?;

                let host = resp["host"]
                    .as_str()
                    .context("Missing host in provision response")?
                    .to_string();
                let port = resp["port"].as_u64().unwrap_or(22) as u16;
                let username = resp["username"]
                    .as_str()
                    .unwrap_or("root")
                    .to_string();
                    
                Ok((host, port, username))
            }
            _ => anyhow::bail!("Not in provision mode"),
        }
    }

    pub async fn start_kubectl(&self) -> Result<Child> {
        match &self.options.mode {
            RemoteRunMode::Kubernetes(opts) => {
                let kubeconfig = &opts.kubeconfig;
                let selector = &opts.pod_selector;

                // 1. Find pod name
                let pod_output = Command::new("kubectl")
                    .arg("--kubeconfig")
                    .arg(kubeconfig)
                    .arg("get")
                    .arg("pods")
                    .arg("-l")
                    .arg(selector)
                    .arg("-o")
                    .arg("jsonpath={.items[0].metadata.name}")
                    .output()
                    .await?;

                if !pod_output.status.success() {
                     anyhow::bail!("Failed to find pod: {}", String::from_utf8_lossy(&pod_output.stderr));
                }
                
                let pod_name = String::from_utf8_lossy(&pod_output.stdout).to_string();

                // 2. Exec into pod
                let kid = Command::new("kubectl")
                    .arg("--kubeconfig")
                    .arg(kubeconfig)
                    .arg("exec")
                    .arg("-it")
                    .arg(&pod_name)
                    .arg("--")
                    .arg("bash")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                    
                Ok(kid)
            }
            _ => anyhow::bail!("Not in kubernetes mode"),
        }
    }
}
