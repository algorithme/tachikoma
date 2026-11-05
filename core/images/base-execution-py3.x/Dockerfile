FROM python:3.13.18-slim-trixie@sha256:d0a9e379c0c588befb0e427db17ca9731751d281f32c6cb1a90a1277b680bdad

LABEL org.opencontainers.image.source="https://github.com/algorithme/tachikoma"

ENV HOME=/workspace \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN mkdir /workspace && chown 10001:10001 /workspace
WORKDIR /workspace
USER 10001:10001

CMD ["python3"]
