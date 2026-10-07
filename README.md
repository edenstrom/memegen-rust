# memegen-rust

A high-performance Rust port of [memegen.link](https://github.com/jacebrowning/memegen), built mainly to run locally as an **MCP server**, with the HTTP API kept as well.

- All 210 upstream templates and fonts, with upstream's text layout: auto-wrapping, font fitting, stroke, rotated text and `:emoji:` aliases (drawn with Twemoji)
- PNG, JPG, GIF and WebP output, including animated GIF/WebP templates
- Recently rendered memes are kept in a 256 MB in-memory cache
- A new static PNG meme takes about 0.5 ms; see [Performance](#performance)
- Also runs on [Cloudflare Workers](#cloudflare-workers), with the API and a remote MCP endpoint

## Build

```sh
cargo build --release
```

The binary reads `templates/`, `fonts/`, `emoji/` and `static/` from the repository it was built in. To use a different location, set `--root` or `MEMEGEN_ROOT`.

## MCP

There is no authentication. Two transports are available:

| Transport | Command | Endpoint |
|---|---|---|
| stdio | `memegen mcp` | n/a |
| Streamable HTTP | `memegen serve` | `http://localhost:5000/mcp` |

The HTTP endpoint binds to `127.0.0.1` by default. It rejects requests whose `Host` isn't localhost (to block DNS rebinding) and browser requests with a non-local `Origin`. To allow other host names, pass `--mcp-allowed-host <host>`.

### Claude Code

```sh
# stdio
claude mcp add memegen -- /path/to/memegen-rust/target/release/memegen mcp

# or over HTTP (with `memegen serve` running)
claude mcp add --transport http memegen http://localhost:5000/mcp
```

### Claude Desktop / other clients (stdio)

```json
{
  "mcpServers": {
    "memegen": {
      "command": "/path/to/memegen-rust/target/release/memegen",
      "args": ["mcp"]
    }
  }
}
```

### Tools

| Tool | Description |
|---|---|
| `list_templates` | Search templates (`filter`, `animated`). Returns ID, name, line count and example text |
| `get_template` | Full details for one template |
| `list_fonts` | Fonts available for `font` |
| `generate_meme` | Render `template_id` + `text[]`. Optional: `extension`, `font`, `save_to` (absolute path), `include_image`. Returns the image inline plus a URL. Inline images over 1 MB (base64) are downscaled in the same format; the URL and `save_to` stay full size |

## HTTP API

Start the server with `memegen serve` (or just `memegen`). Swagger docs are at `/docs` and the OpenAPI spec at `/openapi.json`.

| Route | Description |
|---|---|
| `GET /images/{template}/{line1}/{line2}.{png,jpg,gif,webp}` | Render a meme. `?font=` sets the font; non-canonical text redirects with a 301 |
| `GET /images/{template}.{ext}` | Template background without text |
| `GET /images/` | Example memes (`?filter=`, `?animated=`) |
| `POST /images/` | Build a meme URL from `{template_id, text[], font, extension, redirect}` (JSON or form) |
| `GET /templates/`, `GET /templates/{id}` | Template catalog (`?filter=`, `?animated=`) |
| `POST /templates/{id}` | Build a meme URL for a template |
| `GET /fonts/`, `GET /fonts/{id}` | Fonts |

Text escapes in URLs: `_` → space, `__` → `_`, `--` → `-`, `~q` → `?`, `~a` → `&`, `~p` → `%`, `~h` → `#`, `~s` → `/`, `~b` → `\`, `~l` → `<`, `~g` → `>`, `~n` → newline, `''` → `"`.

## Configuration

| Variable / flag | Default | Purpose |
|---|---|---|
| `HOST` / `--host` | `127.0.0.1` | Bind address |
| `PORT` / `--port` | `5000` | Port |
| `DOMAIN` | unset | When set, API responses use `https://$DOMAIN` URLs |
| `DEBUG=true` | off | Skip the render cache and re-render every request |
| `DEFAULT_STATIC_EXTENSION` | `png` | Default format for static templates |
| `DEFAULT_ANIMATED_EXTENSION` | `gif` | Default format for animated templates |
| `MEMEGEN_ROOT` / `--root` | build directory | Location of the assets |
| `RUST_LOG` | `info` (`warn` for stdio) | Log level (logs go to stderr) |

## Cloudflare Workers

`worker/` builds the same code to WebAssembly with [workers-rs](https://github.com/cloudflare/workers-rs). It serves the full HTTP API plus MCP at `/mcp` (stateless Streamable HTTP, JSON responses, no sessions).

```sh
cd worker
npx wrangler dev      # http://localhost:8787
npx wrangler deploy   # https://memegen.<account>.workers.dev
```

The build needs the `wasm32-unknown-unknown` Rust target (`rustup target add wasm32-unknown-unknown`); wrangler installs `worker-build` on first use.

- **Assets**: `templates/`, `fonts/`, `emoji/` and `static/` (about 4,700 files, 105 MB) are uploaded as [static assets](https://developers.cloudflare.com/workers/static-assets/) through symlinks in `worker/assets/`; later deploys only upload changed files. The Worker runs first for every request (`run_worker_first`), so the raw files aren't public. The compiled Worker is about 1.6 MB gzipped.
- **Startup**: the template catalog is embedded at build time; fonts (5 MB) are fetched on the first request in each isolate.
- **Caching**: successful `/images/...` responses go into Cloudflare's cache, so a repeated meme isn't rendered again. The Cache API has no effect on `*.workers.dev`, so use a [custom domain](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/) to get this. Each isolate also keeps small in-memory caches (isolates have 128 MB of memory).
- **CPU time**: rendering needs the Workers Paid plan. In local `wrangler dev`, a new static meme took 8–17 ms and an animated GIF about 85 ms, so most renders exceed the Free plan's 10 ms CPU limit. `wrangler.toml` sets a 30 s limit.
- **Differences from the native server**: WebP is lossless, because libwebp (C) doesn't build for `wasm32-unknown-unknown`. Animated WebP keeps 20 frames instead of 80 and is large (about 5.6 MB for `oprah`), so use GIF for animations. `generate_meme` has no `save_to`. The `/mcp` endpoint has no Host/Origin checks; it's meant to be public. Set `DOMAIN` in `wrangler.toml` to use a custom host in returned URLs (by default, URLs use the host of the first request each isolate serves).

### Remote MCP

```sh
claude mcp add --transport http memegen https://memegen.<account>.workers.dev/mcp
```

## Not ported

Watermarks, previews, error images, custom backgrounds and overlays, size/color/layout parameters, animated text, legacy shortcut redirects, and the remote API-key/analytics integrations.

## Performance

Run `memegen bench` to reproduce (set `RAYON_NUM_THREADS=1` for single-core numbers). These are medians on an 18-core Apple Silicon Mac. "Warm" means the template background is already decoded but the meme itself is new; this is the normal case in a running server. Static PNG and JPG renders reuse each template's encoded background and only encode the rows the text touches. Animated WebP frames are encoded in parallel, each as just the area that changed since the previous frame.

| Case | Upstream Python wall / CPU | Rust warm wall / CPU (1 thread) | Rust warm wall (18 threads) |
|---|---|---|---|
| static png, 2 lines | 44.8 / 44.7 ms | 0.85 / 0.85 ms | 0.49 ms |
| static jpg, wrapped text | 79.5 / 79.3 ms | 2.0 / 2.0 ms | 1.0 ms |
| static png, 3 lines rotated | 431 / 431 ms | 2.6 / 2.6 ms | 0.69 ms |
| static png, emoji | 414 / 57 ms (Twemoji download) | 0.66 / 0.66 ms | 0.41 ms |
| animated gif, 17 frames | 653 / 652 ms | 52 / 52 ms | 11.3 ms |
| animated webp, 17 frames | 818 / 815 ms | 118 / 118 ms | 9.4 ms |

A repeated meme comes from the in-memory cache in about 2 µs. Over HTTP, `wrk` measured about 5,800 new memes/s and 17,000 cached responses/s. Starting `memegen mcp` takes about 15 ms to the `initialize` response.

## Licenses

The code and templates come from memegen (MIT, see `LICENSE-upstream.txt`); fonts carry their own licenses in `fonts/`. Emoji graphics are Twemoji (CC-BY 4.0, see `emoji/LICENSE.md`).
