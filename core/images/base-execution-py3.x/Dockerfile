FROM python:3.13.18-slim-trixie@sha256:e48f59cdba69a3cbd9ed2cf0f348d90d45a8d350d98134cf1070bc106ee9ed90

LABEL org.opencontainers.image.source="https://github.com/algorithme/tachikoma"

ENV HOME=/workspace \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN mkdir /workspace && chown 10001:10001 /workspace
WORKDIR /workspace
USER 10001:10001

CMD ["python3"]
