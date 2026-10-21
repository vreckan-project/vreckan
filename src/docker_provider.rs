//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Docker application provider.
//!
//! Ports `server/providers/docker_provider` (and the `BaseProvider`
//! interface in `base_provider`) onto the `bollard` crate, which speaks the
//! Docker Engine API directly over the unix socket / TCP.
//!
//! The provider is constructed per-launch (mirroring the Python, which builds a
//! fresh `DockerProvider(app_config)` for every launch/stop). It knows how to:
//!
//! * pull an image (optionally streaming progress),
//! * launch a container from an app's `provider_config` (env, volumes, devices,
//!   GPU, docker overrides) and wait until the app inside answers a health
//!   check, and
//! * stop + remove a container.
//!
//! The heavy launch orchestration (env/volume/GPU *policy*) lives in
//! `catalog.rs`; this module is the thin Docker-API layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bollard::models::{
    ContainerCreateBody, DeviceMapping, DeviceRequest, HostConfig, Mount, MountType,
    NetworkingConfig, RestartPolicy, RestartPolicyNameEnum, ResourcesUlimits,
};
use bollard::query_parameters::{CreateContainerOptions, CreateImageOptions, StopContainerOptions};
use base64::Engine;
use bollard::Docker;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};

/// A GPU device detected on the host (mirrors an entry of `api.AVAILABLE_GPUS`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuInfo {
    pub device: String,
    pub driver: String,
    /// "nvidia" or "dri3".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Index for nvidia GPUs (0-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
}

/// A single bind mount: host path (the map key) -> container path + mode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub bind: String,
    /// "rw" or "ro".
    pub mode: String,
}

/// The result of a successful container launch.
#[derive(Clone, Debug)]
pub struct LaunchResult {
    pub instance_id: String,
    pub ip: String,
    pub port: i64,
}

/// A launch failure carrying the HTTP status to surface to the client.
#[derive(Debug)]
pub struct LaunchError {
    pub status: u16,
    pub detail: String,
}

impl LaunchError {
    fn new(status: u16, detail: impl Into<String>) -> Self {
        Self { status, detail: detail.into() }
    }
}

/// A Docker application provider.
pub struct DockerProvider {
    client: Docker,
    app_config: Value,
}

/// Consecutive (subfolder 404, root 200 HTML) polls required before the
/// readiness gate accepts a legacy (guacamole-lite) container (ANT-450,
/// F-P5). The repeat requirement keeps a transient early-boot answer (e.g. a
/// mid-config nginx on a selkies image) from passing the gate.
const LEGACY_READY_STRIKES: usize = 3;

/// The legacy-tier root-page readiness check (ANT-450, F-P5): the
/// guacamole-lite gclient serves its page at `/` and 404s on the session
/// subfolder, so a ready legacy container answers 200 `text/html` at the
/// root while the subfolder stays 404.
async fn legacy_root_ok(client: &reqwest::Client, ip: &str, port: i64) -> bool {
    match client.get(format!("http://{ip}:{port}/")).send().await {
        Ok(resp) => {
            resp.status() == reqwest::StatusCode::OK
                && resp.headers().get(reqwest::header::CONTENT_TYPE).is_some_and(|ct| {
                    ct.to_str().is_ok_and(|ct| ct.contains("text/html"))
                })
        }
        Err(_) => false,
    }
}

impl DockerProvider {
    /// Connect to the Docker daemon and bind the provider to an app config.
    /// Mirrors `DockerProvider.__init__` (`docker.from_env()` + ping).
    pub fn new(app_config: Value) -> Result<Self, LaunchError> {
        let client = Docker::connect_with_local_defaults()
            .map_err(|e| LaunchError::new(500, format!("Failed to connect to Docker daemon: {e}")))?;
        Ok(Self { client, app_config })
    }

    fn provider_config(&self) -> &Value {
        self.app_config
            .get("provider_config")
            .unwrap_or(&Value::Null)
    }

    /// Pull the app's image, then cache the pulled image's metadata (sha +
    /// digests) into `state.image_metadata` so the installed-apps table is
    /// accurate without admin action (ANT-269). Mirrors `initialize()` (called
    /// on-demand at launch when the image is missing locally).
    pub async fn initialize(&self, state: &crate::state::AppState) -> Result<(), LaunchError> {
        let image = self
            .provider_config()
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        info!(
            "[{}] Initializing Docker provider...",
            self.app_config.get("name").and_then(|v| v.as_str()).unwrap_or(&image)
        );
        self.pull_image(&image, None).await?;
        crate::catalog::cache_local_image_metadata(state, &image).await;
        Ok(())
    }

    /// Inspect a locally-present image. Returns `None` if it is not found.
    /// `short_id` is the Docker-style short identifier: the image ID (minus
    /// its `sha256:` prefix) truncated to 12 chars (ANT-269).
    /// Mirrors `get_local_image_info`.
    pub async fn get_local_image_info(&self, image_name: &str) -> Option<Value> {
        match self.client.inspect_image(image_name).await {
            Ok(img) => {
                let id = img.id.clone().unwrap_or_default();
                Some(serde_json::json!({
                    "id": id,
                    "short_id": short_id(&id),
                    "digests": img.repo_digests,
                }))
            }
            Err(e) => {
                debug!("Local image '{image_name}' not found: {e}");
                None
            }
        }
    }

    /// Get the digest of the latest image from the remote registry.
    /// Mirrors `get_remote_image_digest`.
    pub async fn get_remote_image_digest(&self, image_name: &str) -> Option<String> {
        match self.client.inspect_registry_image(image_name, None).await {
            Ok(dist) => dist.descriptor.digest,
            Err(e) => {
                warn!("Remote image digest lookup failed for '{image_name}': {e}");
                None
            }
        }
    }

    /// Pull an image from the registry. When `progress_cb` is provided it is
    /// invoked for every progress event the daemon streams, so callers can
    /// surface live download progress. Mirrors `pull_image`.
    pub async fn pull_image(
        &self,
        image_name: &str,
        progress_cb: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    ) -> Result<(), LaunchError> {
        info!("Pulling latest image for '{image_name}'...");
        let options = CreateImageOptions {
            from_image: Some(image_name.to_string()),
            ..Default::default()
        };
        // `create_image` is synchronous and returns a stream of progress events.
        let mut stream = self.client.create_image(Some(options), None, None);
        while let Some(event) = stream.next().await {
            match event {
                Ok(info) => {
                    if let Some(cb) = &progress_cb {
                        cb(serde_json::to_value(&info).unwrap_or(Value::Null));
                    }
                }
                Err(e) => {
                    return Err(LaunchError::new(
                        500,
                        format!("Failed to pull image '{image_name}': {e}"),
                    ));
                }
            }
        }
        info!("Successfully pulled '{image_name}'.");
        Ok(())
    }

    /// Launch a container for the application and wait until it is ready.
    /// Mirrors `launch()`.
    ///
    /// `ephemeral` marks short-lived (cleanroom) session containers: they get
    /// the `no` restart policy instead of `unless-stopped`, so a Docker daemon
    /// restart cannot resurrect them after the server that owns the session is
    /// gone (ANT-378).
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        &self,
        session_id: &str,
        env_vars: HashMap<String, String>,
        volumes: HashMap<String, VolumeSpec>,
        path_prefix_map: &HashMap<String, String>,
        gpu_config: Option<GpuInfo>,
        network: Option<String>,
        is_collaboration: bool,
        master_token: Option<String>,
        initial_tokens: Option<Value>,
        ephemeral: bool,
    ) -> Result<LaunchResult, LaunchError> {
        let config = self.provider_config();
        let image = config
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Ensure the image is present locally, pulling it if not.
        if self.client.inspect_image(&image).await.is_err() {
            info!("[{session_id}] Image '{image}' not found locally, pulling...");
            self.pull_image(&image, None).await?;
        }

        // --- Build the container config -------------------------------------
        let mut env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        let mut mounts = self.build_mounts(&volumes);

        // Device specs accumulate as raw "host:container[:mode]" strings and are
        // converted to `DeviceMapping` at the end (nvidia also adds a
        // `/dev/nvidia-modeset` device and a device request).
        let mut device_specs: Vec<String> = config
            .get("devices")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let mut device_requests: Vec<DeviceRequest> = Vec::new();
        let mut group_add: Vec<String> = Vec::new();
        let mut privileged: Option<bool> = None;
        let mut cap_add: Vec<String> = Vec::new();
        let mut cap_drop: Vec<String> = Vec::new();
        let mut security_opt: Vec<String> = Vec::new();
        let mut dns: Vec<String> = Vec::new();
        let mut extra_hosts: Vec<String> = Vec::new();
        let mut sysctls: HashMap<String, String> = HashMap::new();
        let mut ulimits: Vec<ResourcesUlimits> = Vec::new();
        let mut tmpfs: HashMap<String, String> = HashMap::new();
        let mut shm_size: Option<i64> = config
            .get("shm_size")
            .and_then(|v| v.as_str())
            .map(parse_size)
            .or_else(|| Some(parse_size("1g")));
        let mut memory: Option<i64> = None;
        let mut cpu_shares: Option<i64> = None;
        let mut nano_cpus: Option<i64> = None;
        let mut network_mode = network;
        let mut ipc_mode: Option<String> = None;
        let mut pid_mode: Option<String> = None;
        // The container command (from a `command` docker override). Mirrors the
        // Python `run_kwargs.update(overrides)` passing `command` through.
        let mut cmd: Option<Vec<String>> = None;
        let mut runtime: Option<String> = None;

        // Apply the app's docker_overrides (from its YAML definition).
        if let Some(overrides) = config.get("docker_overrides") {
            self.apply_docker_overrides(
                overrides,
                &mut env,
                &mut mounts,
                &mut device_specs,
                &mut group_add,
                &mut privileged,
                &mut cap_add,
                &mut cap_drop,
                &mut security_opt,
                &mut dns,
                &mut extra_hosts,
                &mut sysctls,
                &mut ulimits,
                &mut tmpfs,
                &mut shm_size,
                &mut memory,
                &mut cpu_shares,
                &mut nano_cpus,
                &mut network_mode,
                &mut ipc_mode,
                &mut pid_mode,
                &mut runtime,
                &mut cmd,
            );
        }

        // GPU configuration.
        if let Some(gpu) = &gpu_config {
            match gpu.r#type.as_deref() {
                Some("nvidia") => {
                    runtime = Some("nvidia".to_string());
                    let index = gpu.index.unwrap_or(0);
                    device_requests.push(DeviceRequest {
                        driver: None,
                        count: None,
                        device_ids: Some(vec![index.to_string()]),
                        capabilities: Some(vec![vec![
                            "compute".into(),
                            "video".into(),
                            "graphics".into(),
                            "utility".into(),
                            "gpu".into(),
                            "display".into(),
                        ]]),
                        options: None,
                    });
                    device_specs.push("/dev/nvidia-modeset:/dev/nvidia-modeset".to_string());
                    info!(
                        "[{session_id}] Configuring container with Nvidia GPU index {index}"
                    );
                }
                Some("dri3") => {
                    device_specs.push(dri3_device_spec(path_prefix_map, &gpu.device));
                    // Forward this process's supplementary groups (e.g. the host
                    // render/video group) so the app (running as PUID) can open
                    // the render node.
                    if let Ok(groups) = read_supplementary_groups() {
                        let mut merged = group_add.clone();
                        for g in groups {
                            let gs = g.to_string();
                            if !merged.contains(&gs) {
                                merged.push(gs);
                            }
                        }
                        merged.sort();
                        group_add = merged;
                    } else {
                        warn!(
                            "[{session_id}] DRI3 GPU {} requested, but this process has no \
                             supplementary groups to forward.",
                            gpu.device
                        );
                    }
                    info!("[{session_id}] Configuring container with DRI3 device {}", gpu.device);
                }
                _ => {}
            }
        }

        let devices: Vec<DeviceMapping> = device_specs
            .iter()
            .filter_map(|spec| parse_device_spec(spec))
            .collect();

        let host_config = HostConfig {
            shm_size,
            memory,
            cpu_shares,
            nano_cpus,
            devices: Some(devices),
            device_requests: if device_requests.is_empty() {
                None
            } else {
                Some(device_requests)
            },
            mounts: Some(mounts),
            restart_policy: Some(RestartPolicy {
                // Ephemeral (cleanroom) session containers must not carry
                // `unless-stopped`: they outlive the session and a daemon
                // restart resurrects them, making them effectively immortal
                // (ANT-378). Persistent app containers keep `unless-stopped`
                // so they survive daemon restarts and are re-attached when
                // the server comes back.
                name: Some(if ephemeral {
                    RestartPolicyNameEnum::NO
                } else {
                    RestartPolicyNameEnum::UNLESS_STOPPED
                }),
                maximum_retry_count: None,
            }),
            auto_remove: Some(false),
            network_mode,
            ipc_mode,
            pid_mode,
            runtime,
            privileged,
            cap_add: if cap_add.is_empty() { None } else { Some(cap_add) },
            cap_drop: if cap_drop.is_empty() { None } else { Some(cap_drop) },
            security_opt: if security_opt.is_empty() { None } else { Some(security_opt) },
            dns: if dns.is_empty() { None } else { Some(dns) },
            extra_hosts: if extra_hosts.is_empty() { None } else { Some(extra_hosts) },
            sysctls: if sysctls.is_empty() { None } else { Some(sysctls) },
            ulimits: if ulimits.is_empty() { None } else { Some(ulimits) },
            tmpfs: if tmpfs.is_empty() { None } else { Some(tmpfs) },
            group_add: if group_add.is_empty() { None } else { Some(group_add) },
            ..Default::default()
        };

        // Clone `env`/`host_config` into the body so the originals remain
        // available for the nvidia-modeset retry below.
        let body = ContainerCreateBody {
            image: Some(image.clone()),
            env: Some(env.clone()),
            cmd: cmd.clone(),
            host_config: Some(host_config.clone()),
            networking_config: Some(NetworkingConfig::default()),
            ..Default::default()
        };

        // --- Create + start (with the nvidia-modeset retry) ------------------
        let create_options = CreateContainerOptions::default();
        let container_id = match self
            .client
            .create_container(Some(create_options.clone()), body)
            .await
        {
            Ok(resp) => resp.id,
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("/dev/nvidia-modeset") && msg.contains("no such file or directory") {
                    // Retry without the modeset device.
                    let retried = self
                        .retry_without_modeset(&create_options, &image, &env, &host_config)
                        .await;
                    match retried {
                        Ok(id) => id,
                        Err(_) => return Err(LaunchError::new(500, format!("Docker error: {e}"))),
                    }
                } else {
                    return Err(self.map_launch_error(&e, image.as_str()));
                }
            }
        };

        self.client
            .start_container(&container_id, None)
            .await
            .map_err(|e| self.map_launch_error(&e, &image))?;

        info!(
            "[{session_id}] Launched container {} from image {}.",
            &container_id[..container_id.len().min(12)],
            image
        );

        // --- Wait for the app inside the container to become ready ----------
        let subfolder = env_vars.get("SUBFOLDER").cloned().unwrap_or_default();
        let ip = self
            .wait_for_container_ready(
                &container_id,
                session_id,
                &subfolder,
                &env_vars,
                is_collaboration,
                master_token.as_deref(),
                initial_tokens.as_ref(),
            )
            .await?;

        let port = config
            .get("port")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        Ok(LaunchResult { instance_id: container_id, ip, port })
    }

    /// Retry a create that failed on a missing `/dev/nvidia-modeset` device.
    async fn retry_without_modeset(
        &self,
        options: &CreateContainerOptions,
        image: &str,
        env: &[String],
        host_config: &HostConfig,
    ) -> Result<String, LaunchError> {
        let devices: Vec<DeviceMapping> = host_config
            .devices
            .as_ref()
            .map(|d| {
                d.iter()
                    .filter(|m| m.path_on_host.as_deref() != Some("/dev/nvidia-modeset"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let mut hc = host_config.clone();
        hc.devices = Some(devices);
        let body = ContainerCreateBody {
            image: Some(image.to_string()),
            env: Some(env.to_vec()),
            host_config: Some(hc),
            networking_config: Some(NetworkingConfig::default()),
            ..Default::default()
        };
        self.client
            .create_container(Some(options.clone()), body)
            .await
            .map(|r| r.id)
            .map_err(|e| self.map_launch_error(&e, image))
    }

    /// Map a bollard error to a client-facing launch error (mirrors the
    /// `except APIError` branches in the Python `launch`).
    fn map_launch_error(&self, e: &bollard::errors::Error, image: &str) -> LaunchError {
        let msg = e.to_string();
        if msg.contains("could not select device driver") || msg.contains("nvidia-container-runtime")
        {
            return LaunchError::new(
                500,
                "Nvidia runtime error on host. Is nvidia-container-toolkit installed and configured?",
            );
        }
        if msg.contains("not found") {
            return LaunchError::new(500, format!("Application image '{image}' not found on host."));
        }
        LaunchError::new(500, format!("Docker error: {msg}"))
    }

    /// Stop and remove a container (session teardown). Mirrors `stop()`.
    pub async fn stop(&self, instance_id: &str) {
        match self.client.inspect_container(instance_id, None).await {
            Ok(ci) => {
                let running = ci
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if running {
                    info!("Stopping container {}...", short_id(instance_id));
                    let opts = StopContainerOptions { signal: None, t: Some(5) };
                    self.client
                        .stop_container(instance_id, Some(opts))
                        .await
                        .ok();
                    info!("Stopped container {}.", short_id(instance_id));
                }
                match self.client.remove_container(instance_id, None).await {
                    Ok(()) => info!("Removed container {}.", short_id(instance_id)),
                    Err(e) => debug!("Removing container {} failed: {e}", short_id(instance_id)),
                }
            }
            Err(_) => {
                warn!("Attempted to stop container {instance_id}, but it was not found.");
            }
        }
    }

    /// Run a command inside a running container and collect its output.
    /// Returns `(exit_code, combined stdout+stderr)`. `None` when the exec
    /// infrastructure itself fails (daemon error, no output stream), which
    /// callers treat as "inconclusive" rather than "command failed"
    /// (ANT-438 launch-time capability probe, step 2).
    pub async fn container_exec(&self, container_id: &str, cmd: &[String]) -> Option<(i64, String)> {
        use bollard::container::LogOutput;
        use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};

        let created = self
            .client
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .ok()?;
        let results = self
            .client
            .start_exec(
                &created.id,
                Some(StartExecOptions { detach: false, tty: false, output_capacity: None }),
            )
            .await
            .ok()?;
        let mut output = match results {
            StartExecResults::Attached { output, .. } => output,
            StartExecResults::Detached => return None,
        };
        let mut combined = String::new();
        while let Some(Ok(entry)) = output.next().await {
            let message = match entry {
                LogOutput::StdErr { message } | LogOutput::StdOut { message } => message,
                LogOutput::StdIn { message } | LogOutput::Console { message } => message,
            };
            combined.push_str(&String::from_utf8_lossy(&message));
        }
        let inspect = self.client.inspect_exec(&created.id).await.ok()?;
        Some((inspect.exit_code.unwrap_or(-1), combined))
    }

    /// Poll the container until the app inside answers a health check.
    /// Mirrors `_wait_for_container_ready`.
    #[allow(clippy::too_many_arguments)]
    async fn wait_for_container_ready(
        &self,
        container_id: &str,
        session_id: &str,
        subfolder: &str,
        env_vars: &HashMap<String, String>,
        is_collaboration: bool,
        master_token: Option<&str>,
        initial_tokens: Option<&Value>,
    ) -> Result<String, LaunchError> {
        let port = self
            .provider_config()
            .get("port")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        // Optional basic auth for the health check (some apps gate their UI).
        let auth_header = match (env_vars.get("CUSTOM_USER"), env_vars.get("PASSWORD")) {
            (Some(u), Some(p)) => {
                let b64 = base64::engine::general_purpose::STANDARD
                    .encode(format!("{u}:{p}"));
                Some(format!("Basic {b64}"))
            }
            _ => None,
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        let mut health_check_passed = false;
        // Consecutive (subfolder 404, root 200 HTML) polls that pass the
        // readiness gate for a legacy (guacamole-lite) container — see the
        // health-check loop below (ANT-450, F-P5).
        let mut legacy_root_strikes = 0usize;
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(60);

        while start.elapsed() < timeout {
            let ip = match self.container_ip(container_id).await {
                Some(ip) => ip,
                None => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };

            if !health_check_passed {
                let url = format!("http://{ip}:{port}{subfolder}");
                let mut req = client.get(&url);
                if let Some(h) = &auth_header {
                    req = req.header(reqwest::header::AUTHORIZATION, h);
                }
                match req.send().await {
                    Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                        info!("[{session_id}] Basic health check passed for {url}");
                        health_check_passed = true;
                        if !is_collaboration {
                            return Ok(ip);
                        }
                    }
                    Ok(resp) => {
                        debug!(
                            "[{session_id}] Health check pending (HTTP {})",
                            resp.status()
                        );
                        // Legacy (pre-2.0 guacamole-lite) tier: the gclient
                        // serves its page at the exact `/` route and 404s on
                        // the session subfolder, so the subfolder check never
                        // passes on the raw image (ANT-450, F-P5) — the
                        // readiness gate used to fail with a 504 and the only
                        // workaround was a one-line image shim. Three
                        // consecutive (subfolder 404, root 200 HTML) polls
                        // pass the gate; the launch-time capability probe
                        // that follows classifies the session, and its
                        // `selkies --version` exec confirms a secure image if
                        // the subfolder is merely mid-boot.
                        legacy_root_strikes = if resp.status() == reqwest::StatusCode::NOT_FOUND
                            && legacy_root_ok(&client, &ip, port).await
                        {
                            legacy_root_strikes + 1
                        } else {
                            0
                        };
                        if legacy_root_strikes >= LEGACY_READY_STRIKES {
                            info!(
                                "[{session_id}] Legacy-tier health check passed (subfolder 404, root 200 HTML)."
                            );
                            health_check_passed = true;
                            if !is_collaboration {
                                return Ok(ip);
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Err(_) => {
                        legacy_root_strikes = 0;
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                }
            }

            if health_check_passed && is_collaboration {
                if let Some(ip) = self.collaboration_health_check(
                    &client,
                    &ip,
                    session_id,
                    port,
                    subfolder,
                    master_token,
                    initial_tokens,
                    auth_header.as_deref(),
                )
                .await
                {
                    return Ok(ip);
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        error!(
            "[{session_id}] Container {} failed to become ready in time.",
            short_id(container_id)
        );
        self.stop(container_id).await;
        Err(LaunchError::new(504, "Container failed to become ready in time."))
    }

    /// Collaboration readiness: post the initial tokens to the downstream app's
    /// control plane. Returns the IP on success. Mirrors the collaboration
    /// branch of `_wait_for_container_ready`.
    #[allow(clippy::too_many_arguments)]
    async fn collaboration_health_check(
        &self,
        client: &reqwest::Client,
        ip: &str,
        session_id: &str,
        port: i64,
        subfolder: &str,
        master_token: Option<&str>,
        initial_tokens: Option<&Value>,
        basic_auth: Option<&str>,
    ) -> Option<String> {
        let master_token = master_token?;
        let mut stacked: HashMap<String, String> = HashMap::new();
        stacked.insert("Selkies-Authorization".to_string(), format!("Bearer {master_token}"));
        if let Some(b) = basic_auth {
            stacked.insert("Authorization".to_string(), b.to_string());
        }
        let trimmed = subfolder.trim_end_matches('/');
        let targets = vec![
            (
                format!("http://{0}:{1}{2}/api/tokens", ip, port, trimmed),
                stacked.clone(),
            ),
            (
                format!("http://{}:8083/tokens", ip),
                HashMap::from([(
                    "Authorization".to_string(),
                    format!("Bearer {master_token}"),
                )]),
            ),
        ];
        for (url, headers) in targets {
            let mut req = client.post(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(tokens) = initial_tokens {
                req = req.json(tokens);
            }
            match req.send().await {
                Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                    info!(
                        "[{session_id}] Collaboration health check passed. Initial tokens set."
                    );
                    return Some(ip.to_string());
                }
                Ok(resp) => {
                    warn!(
                        "[{session_id}] Collaboration health check failed (HTTP {}) at {url}",
                        resp.status()
                    );
                }
                Err(e) => debug!("[{session_id}] Control plane not reachable at {url}: {e}"),
            }
        }
        warn!("[{session_id}] Collaboration health check failed on all endpoints.");
        None
    }

    /// Resolve a container's IP address from its network settings.
    /// Mirrors `_get_container_ip`.
    async fn container_ip(&self, container_id: &str) -> Option<String> {
        let ci = self.client.inspect_container(container_id, None).await.ok()?;
        let networks = ci
            .network_settings
            .as_ref()?
            .networks
            .as_ref()?
            .iter()
            .collect::<Vec<_>>();
        // Prefer the "bridge" network, else the first network with an IP.
        if let Some((_, ep)) = networks.iter().find(|(name, _)| name.as_str() == "bridge") {
            if let Some(ip) = &ep.ip_address {
                if !ip.is_empty() {
                    return Some(ip.clone());
                }
            }
        }
        for (_, ep) in networks {
            if let Some(ip) = &ep.ip_address {
                if !ip.is_empty() {
                    return Some(ip.clone());
                }
            }
        }
        None
    }

    /// Convert a `host -> VolumeSpec` map into bollard `Mount`s.
    fn build_mounts(&self, volumes: &HashMap<String, VolumeSpec>) -> Vec<Mount> {
        volumes
            .iter()
            .map(|(host, spec)| Mount {
                source: Some(host.clone()),
                target: Some(spec.bind.clone()),
                typ: Some(MountType::BIND),
                read_only: Some(spec.mode == "ro"),
                ..Default::default()
            })
            .collect()
    }

    /// Apply an app's `docker_overrides` dict onto the in-progress launch
    /// config. Mirrors the `run_kwargs.update(overrides)` + template-volume
    /// merge in the Python `launch`.
    #[allow(clippy::too_many_arguments)]
    fn apply_docker_overrides(
        &self,
        overrides: &Value,
        env: &mut Vec<String>,
        mounts: &mut Vec<Mount>,
        device_specs: &mut Vec<String>,
        group_add: &mut Vec<String>,
        privileged: &mut Option<bool>,
        cap_add: &mut Vec<String>,
        cap_drop: &mut Vec<String>,
        security_opt: &mut Vec<String>,
        dns: &mut Vec<String>,
        extra_hosts: &mut Vec<String>,
        sysctls: &mut HashMap<String, String>,
        ulimits: &mut Vec<ResourcesUlimits>,
        tmpfs: &mut HashMap<String, String>,
        shm_size: &mut Option<i64>,
        memory: &mut Option<i64>,
        cpu_shares: &mut Option<i64>,
        nano_cpus: &mut Option<i64>,
        network_mode: &mut Option<String>,
        ipc_mode: &mut Option<String>,
        pid_mode: &mut Option<String>,
        runtime: &mut Option<String>,
        cmd: &mut Option<Vec<String>>,
    ) {
        let obj = match overrides.as_object() {
            Some(o) => o,
            None => return,
        };
        for (k, v) in obj {
            match k.as_str() {
                "privileged" => *privileged = v.as_bool(),
                "command" => {
                    if let Some(a) = v.as_array() {
                        *cmd = Some(
                            a.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect(),
                        );
                    }
                }
                "cap_add" => push_str_list(v, cap_add),
                "cap_drop" => push_str_list(v, cap_drop),
                "security_opt" => push_str_list(v, security_opt),
                "devices" => push_str_list(v, device_specs),
                "dns" => push_str_list(v, dns),
                "extra_hosts" => push_str_list(v, extra_hosts),
                "group_add" => push_str_list(v, group_add),
                "shm_size" => {
                    if let Some(s) = v.as_str() {
                        *shm_size = Some(parse_size(s));
                    }
                }
                "mem_limit" => {
                    if let Some(s) = v.as_str() {
                        *memory = Some(parse_size(s));
                    }
                }
                "cpu_shares" => *cpu_shares = v.as_i64(),
                "nano_cpus" => *nano_cpus = v.as_i64(),
                "network_mode" => *network_mode = v.as_str().map(String::from),
                "ipc_mode" => *ipc_mode = v.as_str().map(String::from),
                "pid_mode" => *pid_mode = v.as_str().map(String::from),
                "runtime" => *runtime = v.as_str().map(String::from),
                "sysctls" => {
                    if let Some(o) = v.as_object() {
                        for (sk, sv) in o {
                            if let Some(s) = sv.as_str() {
                                sysctls.insert(sk.clone(), s.to_string());
                            }
                        }
                    }
                }
                "tmpfs" => {
                    if let Some(o) = v.as_object() {
                        for (sk, sv) in o {
                            if let Some(s) = sv.as_str() {
                                tmpfs.insert(sk.clone(), s.to_string());
                            }
                        }
                    }
                }
                "ulimits" => {
                    if let Some(a) = v.as_array() {
                        for item in a {
                            if let Some(s) = item.as_str() {
                                if let Some(u) = parse_ulimit(s) {
                                    ulimits.push(u);
                                }
                            }
                        }
                    }
                }
                "environment" => {
                    if let Some(o) = v.as_object() {
                        for (ek, ev) in o {
                            if let Some(s) = ev.as_str() {
                                env.push(format!("{ek}={s}"));
                            }
                        }
                    }
                }
                "volumes" => {
                    // Template bind mounts arrive as "host:container[:ro]" strings;
                    // merge them into the existing mounts (do not clobber).
                    if let Some(a) = v.as_array() {
                        for spec in a.iter().filter_map(|s| s.as_str()) {
                            if let Some(m) = parse_bind_mount(spec) {
                                if !mounts.iter().any(|x| x.source.as_deref() == m.source.as_deref())
                                {
                                    mounts.push(m);
                                }
                            }
                        }
                    }
                }
                _ => debug!("Ignoring unknown docker override key '{k}'"),
            }
        }
    }
}

/// Build the `host:container` device spec for a DRI3 render node. The
/// container side keeps the detected in-container path; the host side goes
/// through the same container->host prefix remap that volume binds use
/// (identity when the server runs directly on the host, where the map is
/// empty).
fn dri3_device_spec(prefix_map: &HashMap<String, String>, device: &str) -> String {
    format!(
        "{}:{}",
        crate::catalog::translate_path_to_host(prefix_map, device),
        device
    )
}

/// Parse a "host:container[:mode]" device spec into a `DeviceMapping`.
fn parse_device_spec(spec: &str) -> Option<DeviceMapping> {
    let (host, cont) = spec.split_once(':')?;
    Some(DeviceMapping {
        path_on_host: Some(host.to_string()),
        path_in_container: Some(cont.to_string()),
        cgroup_permissions: Some("rwm".to_string()),
    })
}

/// Parse a "host:container[:ro]" bind-mount spec into a `Mount`.
fn parse_bind_mount(spec: &str) -> Option<Mount> {
    let (host, rest) = spec.split_once(':')?;
    let (cont, ro) = match rest.rsplit_once(':') {
        Some((c, "ro")) => (c, true),
        _ => (rest, false),
    };
    if host.is_empty() {
        return None;
    }
    Some(Mount {
        source: Some(host.to_string()),
        target: Some(cont.to_string()),
        typ: Some(MountType::BIND),
        read_only: Some(ro),
        ..Default::default()
    })
}

/// Append the string items of a JSON array to `out`.
fn push_str_list(v: &Value, out: &mut Vec<String>) {
    if let Some(a) = v.as_array() {
        for item in a.iter().filter_map(|s| s.as_str()) {
            out.push(item.to_string());
        }
    }
}

/// Parse a docker size string ("1g", "512m", "1024") into bytes.
fn parse_size(s: &str) -> i64 {
    let s = s.trim();
    let (num, unit) = match s.as_bytes().last() {
        Some(b) if b.is_ascii_alphabetic() => (&s[..s.len() - 1], *b as char),
        _ => (s, 'b'),
    };
    let num: f64 = num.parse().unwrap_or(1.0);
    let mult = match unit {
        'b' => 1.0,
        'k' => 1024.0,
        'm' => 1024.0 * 1024.0,
        'g' => 1024.0 * 1024.0 * 1024.0,
        't' => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (num * mult) as i64
}

/// Parse a "name=soft:hard" (or "name=limit") ulimit spec.
fn parse_ulimit(s: &str) -> Option<ResourcesUlimits> {
    let (name, limit) = s.split_once('=')?;
    let (soft, hard) = match limit.split_once(':') {
        Some((soft, hard)) => (soft, hard),
        None => (limit, limit),
    };
    Some(ResourcesUlimits {
        name: Some(name.trim().to_string()),
        soft: soft.trim().parse().ok(),
        hard: hard.trim().parse().ok(),
    })
}

/// Read the current process's supplementary group GIDs (for DRI3 group
/// forwarding). Returns an empty vec if unavailable.
fn read_supplementary_groups() -> Result<Vec<u64>, ()> {
    // On Linux, /proc/self/status exposes "Groups:" as a space-separated list.
    let status = std::fs::read_to_string("/proc/self/status").map_err(|_| ())?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Groups:") {
            let groups = rest
                .split_whitespace()
                .filter_map(|g| g.parse::<u64>().ok())
                .filter(|g| *g != 0)
                .collect();
            return Ok(groups);
        }
    }
    Err(())
}

/// A short, Docker-style identifier: the ID (minus any `sha256:` prefix)
/// truncated to its first 12 characters. Used for container IDs in logs and
/// for image IDs in `get_local_image_info` (ANT-269).
fn short_id(id: &str) -> &str {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    &hex[..hex.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the host there is no prefix map, so the spec is the detected path
    /// on both sides (the historical behavior).
    #[test]
    fn dri3_device_spec_identity_without_prefix_map() {
        assert_eq!(
            dri3_device_spec(&HashMap::new(), "/dev/dri/renderD128"),
            "/dev/dri/renderD128:/dev/dri/renderD128"
        );
    }

    /// When the server's `/dev/dri` is a bind mount at a different host path,
    /// the host side of the spec is remapped while the container side keeps
    /// the detected path (ANT-234 bug 2).
    #[test]
    fn dri3_device_spec_remaps_host_side() {
        let mut map = HashMap::new();
        map.insert("/dev/dri".to_string(), "/mnt/host-dev/dri".to_string());
        assert_eq!(
            dri3_device_spec(&map, "/dev/dri/renderD128"),
            "/mnt/host-dev/dri/renderD128:/dev/dri/renderD128"
        );
    }

    /// Image IDs arrive with a `sha256:` digest prefix; the short form
    /// strips it and truncates to 12 chars, Docker-style (ANT-269).
    #[test]
    fn short_id_strips_digest_prefix_and_truncates() {
        assert_eq!(
            short_id("sha256:0123456789abcdef0123456789abcdef"),
            "0123456789ab"
        );
        assert_eq!(short_id("ctr1234567890abcdef"), "ctr123456789");
        assert_eq!(short_id("short"), "short");
    }
}
