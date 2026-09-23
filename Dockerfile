# Vreckan API server image.
#
# Build from the repository root (the compose file sets the build context to
# this directory):
#
#     docker build -t vreckan-api .
#
# The image is intentionally thin: it installs the Python dependencies and
# copies the application. All persistent state lives in /data (mount a volume
# there) and is created on first run by the entrypoint.
FROM python:3.12-slim

# Keep Python from writing .pyc files and buffering stdout/stderr so `docker
# logs` shows output immediately.
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    WORK=/data

WORKDIR /app

# Install dependencies first (separate layer) so the image only rebuilds its
# dependency layer when requirements.txt changes. (JWT handling comes from
# python-jose in requirements.txt — no separate PyJWT package is needed.)
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# ecdsa is a declared python-jose dependency but is NEVER imported at runtime:
# jose 3.5.0 uses the cryptography backend for EC ops whenever cryptography is
# installed (see memory.md §13.19), and the live IdP signs RS256/RSA, so the
# app never enters the EC path. The package only exists to carry the "Minerva"
# timing-attack finding (CVE-2024-23342), so remove it to keep the security
# scanners quiet. Safe to drop: cryptography is a hard requirement of this
# image, so jose never falls back to the ecdsa backend. (pip check will report
# a missing python-jose requirement — accepted, see §13.19.)
RUN pip uninstall -y ecdsa

# starlette 1.6.0 (the latest release) still references the deprecated
# anyio.abc.BlockingPortal alias inside its TestClient. That file is only
# imported by the test suite, so repoint the alias at its canonical
# anyio.from_thread home to keep the suite warning-free. No effect on the
# production server.
RUN SITE=$(python3 -c "import site; print(site.getsitepackages()[0])") && \
    sed -i 's/anyio\.abc\.BlockingPortal/anyio.from_thread.BlockingPortal/g' \
    "$SITE/starlette/testclient.py"

# Copy the application (app/, entrypoints, ...).
# NOTE: the `data/` host directory is excluded via .dockerignore and is
# provided at runtime by a Docker volume instead.
COPY . ./

# /data holds all persistent state (keys, users, groups, apps, storage, ...).
# The entrypoint creates the sub-directory tree on first run. Mount a named
# volume (or bind mount) here to persist it.
RUN mkdir -p /data

EXPOSE 443

# TLS entrypoint. It (re)creates the /data tree, generates the server key,
# and serves the API over HTTPS on $VRECKAN_API_PORT (default 443). It uses
# the Let's Encrypt cert mounted at /etc/letsencrypt/live/antitux.net when
# present, and falls back to a generated self-signed certificate
# (/data/ssl/vreckan_dev.crt + .key) otherwise.
ENTRYPOINT ["python", "run_https.py"]
