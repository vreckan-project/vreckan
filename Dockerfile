FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    WORK=/data

WORKDIR /app

COPY requirements.txt ./

RUN apt-get update -y && \
    DEBIAN_FRONTEND=noninteractive apt-get dist-upgrade -y && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* && \
    pip install --no-cache-dir -r requirements.txt && \
    pip uninstall -y ecdsa && \
    SITE=$(python3 -c "import site; print(site.getsitepackages()[0])") && \
    sed -i 's/anyio\.abc\.BlockingPortal/anyio.from_thread.BlockingPortal/g' \
    "$SITE/starlette/testclient.py" && \
    mkdir -p /data

COPY . ./

EXPOSE 443

ENTRYPOINT ["python", "run_https.py"]
