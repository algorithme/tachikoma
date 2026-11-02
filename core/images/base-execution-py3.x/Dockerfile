FROM python:3.13.18-slim-trixie@sha256:ae14d4ce8771606477a81446e7ef429edd6762305fc50c84507c1041fa8285d1

LABEL org.opencontainers.image.source="https://github.com/algorithme/tachikoma"

ENV HOME=/workspace \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN mkdir /workspace && chown 10001:10001 /workspace
WORKDIR /workspace
USER 10001:10001

CMD ["python3"]
