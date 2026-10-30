FROM python:3.13.16-slim-trixie@sha256:cf50a4f6ac4f026458b08d5222023d700ac806410dff5e9ca8728fa44f2b850f

LABEL org.opencontainers.image.source="https://github.com/algorithme/tachikoma"

ENV HOME=/workspace \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN mkdir /workspace && chown 10001:10001 /workspace
WORKDIR /workspace
USER 10001:10001

CMD ["python3"]
