# Typstd

## Overview

### Telemetry

Tracing configuration can be adjusted either though CLI flags or with
environment variable `TYPSTD_LOG`.

In compile time one should enable feature `telemetry` then run OpenTelemetry
collector. Perhaps the easiest way to start collector is running it in docker
container.

```shell
docker run -p 4317:4317 otel/opentelemetry-collector-dev:latest
``.`
