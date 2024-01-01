# Typstd

## Overview

**Typstd** is a pretty simple language server for [Typst][1] markup language.
Its distinctive feature is workspace management and completion for global
objects which are defined out of scope of text document in focus (e.g.
bibliography references).

[1]: https://github.com/typst/typst

### Workspace

In order to determine entrypoints for compilation one can define `typst.toml`
configuration file which enumerates targets for rendering.

```toml
[[document]]
name = "typstd"
version = "0.0.0"
entrypoint = "main.typ"
authors = ["Daniel Bershatsky <daniel.bershatsky@gmail.com>"]
license = "MIT"
description = "Plain and simple language server for Typst markup language."
repository = "https://github.com/daskol/typstd"
keywords = ["language-server", "languager-server-protocol", "lsp", "typst"]
```

### Neovim

```lua
-- Default capabilities with `nvim-cmp` package.
local capabilities = require('cmp_nvim_lsp').default_capabilities()
lspconfig_configs['typstd'] = {
    default_config = {
        name = 'typstd',
        filetypes = { 'typst' },
        cmd = { 'typstd' },
        cmd_env = {},
        single_file_support = true,
        capabilities = capabilities,
    }
}
```

### Telemetry

Tracing configuration can be adjusted either though CLI flags or with
environment variable `TYPSTD_LOG`.

In compile time one should enable feature `telemetry` then run OpenTelemetry
collector. Perhaps the easiest way to start collector is running it in docker
container.

```shell
docker run -p 4317:4317 otel/opentelemetry-collector-dev:latest
```
