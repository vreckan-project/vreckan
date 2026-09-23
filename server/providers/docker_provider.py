import asyncio
import os
import time
import logging
from typing import Callable, Dict, Optional

import docker
from docker.errors import NotFound, ImageNotFound, APIError, DockerException
from docker.types import DeviceRequest
from fastapi import HTTPException

from .base_provider import BaseProvider
from ..settings import settings

logger = logging.getLogger(__name__)


class DockerProvider(BaseProvider):
    """A provider for launching applications in Docker containers."""

    def __init__(self, app_config: Dict):
        super().__init__(app_config)
        try:
            self.client = docker.from_env()
            self.client.ping()
        except Exception as e:
            logger.error(f"Could not connect to Docker daemon: {e}")
            raise RuntimeError("Failed to connect to Docker daemon.") from e

    async def initialize(self):
        """Pulls the required image. Can be called on-demand."""
        image_name = self.app_config["provider_config"]["image"]
        logger.info(
            f"[{self.app_config.get('name', image_name)}] Initializing Docker provider..."
        )
        await self.pull_image(image_name)

    async def get_local_image_info(self, image_name: str) -> Optional[Dict]:
        """Gets information about a locally available image."""
        try:
            image = await asyncio.to_thread(self.client.images.get, image_name)
            return {
                "id": image.id,
                "short_id": image.short_id.split(":")[-1],
                "digests": image.attrs.get("RepoDigests", []),
            }
        except ImageNotFound:
            return None
        except APIError as e:
            logger.error(
                f"Docker API error getting local image info for '{image_name}': {e}"
            )
            return None

    async def get_remote_image_digest(self, image_name: str) -> Optional[str]:
        """Gets the digest of the latest image from the remote registry."""
        try:
            api_client = self.client.api
            distribution_info = await asyncio.to_thread(
                api_client.inspect_distribution, image_name
            )
            return distribution_info["Descriptor"]["digest"]
        except APIError as e:
            if e.response.status_code == 404:
                logger.warning(f"Image '{image_name}' not found in remote registry.")
            else:
                logger.error(
                    f"Docker API error inspecting remote image '{image_name}': {e}"
                )
            return None
        except DockerException as e:
            logger.error(f"Docker error inspecting remote image '{image_name}': {e}")
            return None

    async def pull_image(
        self,
        image_name: str,
        progress_cb: Optional[Callable[[dict], None]] = None,
    ):
        """Pulls an image from the registry and returns the new image object.

        When ``progress_cb`` is provided, the pull uses the streaming Docker
        API and the callback is invoked (from a worker thread) for every
        progress event the daemon reports, so callers can surface live
        download progress (per-layer current/total bytes, status text).
        """
        def _pull():
            if progress_cb is None:
                return self.client.images.pull(image_name)
            response = self.client.api.pull(image_name, stream=True, decode=True)
            for line in response:
                progress_cb(line)
            return self.client.images.get(image_name)

        try:
            logger.info(f"Pulling latest image for '{image_name}'...")
            image = await asyncio.to_thread(_pull)
            logger.info(f"Successfully pulled '{image_name}'.")
            return image
        except APIError as e:
            logger.error(f"Failed to pull image '{image_name}': {e}")
            raise

    async def launch(
        self,
        session_id: str,
        env_vars: Dict,
        volumes: Optional[Dict] = None,
        gpu_config: Optional[Dict] = None,
        network: Optional[str] = None,
        is_collaboration: bool = False,
        master_token: Optional[str] = None,
        initial_tokens: Optional[Dict] = None,
    ) -> Dict:
        """Launches a Docker container for the application."""
        config = self.app_config["provider_config"]
        image = config["image"]

        try:
            await asyncio.to_thread(self.client.images.get, image)
        except ImageNotFound:
            logger.info(f"[{session_id}] Image '{image}' not found locally, pulling...")
            await self.pull_image(image)

        run_kwargs = {
            "image": image,
            "detach": True,
            "shm_size": config.get("shm_size", "1g"),
            "environment": env_vars,
            "volumes": volumes or {},
            "devices": list(config.get("devices", [])),
            # Session containers must survive a host reboot: do not auto-remove
            # them on exit and give them a restart policy so the Docker daemon
            # brings them back automatically. They are removed explicitly when a
            # session ends (see stop()).
            "remove": False,
            # docker-py expects the Docker API RestartPolicy object (a dict),
            # not a bare string.
            "restart_policy": {"Name": "unless-stopped"},
            "network": network,
        }

        if config.get("docker_overrides"):
            overrides = dict(config["docker_overrides"])
            # Template bind mounts (DOCKER_BIND_MOUNTS) arrive as a list of
            # "host:container[:ro]" strings. Merge them into the caller-built
            # volumes dict (home dir + external mounts) instead of replacing it
            # — a plain update() would clobber those mounts.
            template_volumes = overrides.pop("volumes", None)
            run_kwargs.update(overrides)
            if template_volumes:
                merged = dict(run_kwargs.get("volumes") or {})
                for spec in template_volumes:
                    if not isinstance(spec, str) or ":" not in spec:
                        continue
                    host, _, cont = spec.partition(":")
                    mode = "rw"
                    if cont.endswith(":ro"):
                        mode = "ro"
                        cont = cont[:-3]
                    if host:
                        merged[host] = {"bind": cont, "mode": mode}
                run_kwargs["volumes"] = merged

        if gpu_config:
            if gpu_config["type"] == "nvidia":
                run_kwargs["runtime"] = "nvidia"
                run_kwargs["device_requests"] = [
                    DeviceRequest(
                        device_ids=[str(gpu_config["index"])],
                        capabilities=[
                            ["compute", "video", "graphics", "utility", "gpu", "display"]
                        ],
                    )
                ]
                run_kwargs["devices"].append("/dev/nvidia-modeset:/dev/nvidia-modeset")
                logger.info(
                    f"[{session_id}] Configuring container with Nvidia GPU index {gpu_config['index']}"
                )
            elif gpu_config["type"] == "dri3":
                device_path = gpu_config["device"]
                run_kwargs["devices"].append(f"{device_path}:{device_path}")
                # The render node is owned by the host's render group, and the
                # app runs as PUID (not root), so it cannot open the device
                # unless it is a member of that group. Forward this API
                # container's own supplementary groups (configured via its
                # group_add, e.g. render/video) into the app container so the
                # app can access the GPU. No GIDs are hard-coded: whatever
                # groups the operator gave the API container are propagated.
                extra_groups = {g for g in os.getgroups() if g != 0}
                if extra_groups:
                    run_kwargs["group_add"] = sorted(
                        set(run_kwargs.get("group_add") or []) | extra_groups
                    )
                else:
                    logger.warning(
                        f"[{session_id}] DRI3 GPU {device_path} requested, but "
                        "this API process has no supplementary groups to forward "
                        "- the app will not be able to open the render node. Add "
                        "the host render/video group GIDs to the API container's "
                        "group_add."
                    )
                logger.info(
                    f"[{session_id}] Configuring container with DRI3 device {device_path}"
                )

        try:
            try:
                container = await asyncio.to_thread(
                    self.client.containers.run, **run_kwargs
                )
            except APIError as e:
                error_msg = str(e).lower()
                if "/dev/nvidia-modeset" in error_msg and "no such file or directory" in error_msg:
                    run_kwargs["devices"].remove("/dev/nvidia-modeset:/dev/nvidia-modeset")
                    container = await asyncio.to_thread(
                        self.client.containers.run, **run_kwargs
                    )
                else:
                    raise e

            logger.info(
                f"[{session_id}] Launched container {container.short_id} from image {image}."
            )
        except ImageNotFound:
            logger.error(
                f"[{session_id}] Image '{image}' not found after pull attempt."
            )
            raise HTTPException(
                status_code=500,
                detail=f"Application image '{image}' not found on host.",
            )
        except APIError as e:
            logger.error(f"[{session_id}] Docker API error on launch: {e}")
            if "could not select device driver" in str(
                e
            ) or "nvidia-container-runtime" in str(e):
                raise HTTPException(
                    status_code=500,
                    detail="Nvidia runtime error on host. Is nvidia-container-toolkit installed and configured?",
                )
            raise HTTPException(
                status_code=500, detail=f"Docker error: {e.explanation}"
            )

        ip_address = await self._wait_for_container_ready(
            container,
            session_id,
            env_vars.get("SUBFOLDER"),
            env_vars,
            is_collaboration=is_collaboration,
            master_token=master_token,
            initial_tokens=initial_tokens,
        )

        return {"instance_id": container.id, "ip": ip_address, "port": config["port"]}

    async def stop(self, instance_id: str):
        """Stops and removes a Docker container (session teardown).

        Session containers are created with a restart policy so they survive a
        host reboot; they are therefore no longer auto-removed on exit and must
        be removed explicitly here when a session ends.
        """
        try:
            container = await asyncio.to_thread(self.client.containers.get, instance_id)
            if container.status == 'running':
                logger.info(f"Stopping container {container.short_id}...")
                await asyncio.to_thread(container.stop, timeout=5)
                logger.info(f"Stopped container {container.short_id}.")
            # Remove the container now that it is stopped (no auto-remove, since
            # session containers persist across reboots via a restart policy).
            try:
                await asyncio.to_thread(container.remove)
                logger.info(f"Removed container {container.short_id}.")
            except APIError as e:
                if e.response.status_code != 409:
                    raise
        except NotFound:
            logger.warning(
                f"Attempted to stop container {instance_id}, but it was not found."
            )
        except Exception as e:
            logger.error(f"Error stopping container {instance_id}: {e}")

    async def _wait_for_container_ready(
        self,
        container,
        session_id,
        subfolder,
        env_vars,
        timeout=60,
        is_collaboration: bool = False,
        master_token: Optional[str] = None,
        initial_tokens: Optional[Dict] = None,
    ):
        import httpx2
        import base64

        auth_header = None
        if "CUSTOM_USER" in env_vars and "PASSWORD" in env_vars:
            auth_str = f"{env_vars['CUSTOM_USER']}:{env_vars['PASSWORD']}"
            auth_b64 = base64.b64encode(auth_str.encode()).decode()
            auth_header = {"Authorization": f"Basic {auth_b64}"}

        health_check_passed = False
        start_time = time.time()
        while time.time() - start_time < timeout:
            try:
                await asyncio.to_thread(container.reload)
                ip_address = self._get_container_ip(container.attrs)
                if not ip_address:
                    await asyncio.sleep(0.5)
                    continue

                if not health_check_passed:
                    health_check_url = f"http://{ip_address}:{self.app_config['provider_config']['port']}{subfolder}"
                    async with httpx2.AsyncClient(
                        timeout=2.0, follow_redirects=True, headers=auth_header
                    ) as client:
                        response = await client.get(health_check_url)
                        if response.status_code == 200:
                            logger.info(
                                f"[{session_id}] Basic health check passed for {health_check_url}"
                            )
                            health_check_passed = True
                            if not is_collaboration:
                                return ip_address
                        else:
                            await asyncio.sleep(2)
                            continue
                
                if health_check_passed and is_collaboration:
                    logger.info(f"[{session_id}] Performing collaboration health check...")
                    stacked_headers = {"Selkies-Authorization": f"Bearer {master_token}"}
                    if auth_header:
                        stacked_headers.update(auth_header)
                    control_plane_targets = [
                        (
                            f"http://{ip_address}:{self.app_config['provider_config']['port']}{subfolder.rstrip('/')}/api/tokens",
                            stacked_headers,
                        ),
                        (
                            f"http://{ip_address}:8083/tokens",
                            {"Authorization": f"Bearer {master_token}"},
                        ),
                    ]
                    async with httpx2.AsyncClient(timeout=5.0) as client:
                        for control_plane_url, control_plane_headers in control_plane_targets:
                            try:
                                response = await client.post(
                                    control_plane_url,
                                    json=initial_tokens,
                                    headers=control_plane_headers
                                )
                                if response.status_code == 200:
                                    from ..collaboration import TOKEN_ENDPOINT_CACHE
                                    TOKEN_ENDPOINT_CACHE[ip_address] = control_plane_url
                                    logger.info(f"[{session_id}] Collaboration health check passed. Initial tokens set.")
                                    return ip_address
                                else:
                                    logger.warning(f"[{session_id}] Collaboration health check failed with status {response.status_code} at {control_plane_url}: {response.text}")
                            except httpx2.RequestError as e:
                                logger.debug(f"[{session_id}] Collaboration control plane not reachable at {control_plane_url}: {e}")
                    logger.warning(f"[{session_id}] Collaboration health check failed on all endpoints.")

            except httpx2.ConnectError:
                logger.debug(
                    f"[{session_id}] Health check pending for {container.short_id}..."
                )
            except Exception as e:
                logger.warning(f"[{session_id}] Error during readiness check: {e}")
            await asyncio.sleep(2)

        logger.error(
            f"[{session_id}] Container {container.short_id} failed to become ready in time."
        )
        await self.stop(container.id)
        raise HTTPException(
            status_code=504, detail="Container failed to become ready in time."
        )

    def _get_container_ip(self, container_attrs):
        networks = container_attrs.get("NetworkSettings", {}).get("Networks", {})
        if not networks:
            return None
        if "bridge" in networks:
            return networks["bridge"].get("IPAddress")
        return next(
            (net.get("IPAddress") for net in networks.values() if net.get("IPAddress")),
            None,
        )

