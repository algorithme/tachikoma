FROM python:3.13.16-slim-trixie@sha256:bf44cdfcb76cd3b41e879bc058fc37ec5872002ccfde7fcb765e218cde0cd79c

LABEL org.opencontainers.image.source="https://github.com/algorithme/tachikoma"

ENV HOME=/workspace \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN mkdir /workspace && chown 10001:10001 /workspace
WORKDIR /workspace
USER 10001:10001

CMD ["python3"]
