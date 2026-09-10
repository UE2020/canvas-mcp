This repository implements a Canvas MCP server that doesn't require you to generate an API token (e.g. to bypass organization restrictions).

# Setup

Build it using cargo:
```
cargo build --release
```

The binary will be placed inside target/release.

Then, configure your agent to use it. You'll need to specify the base url to use when logging in and accessing the API via an environment variable. For example, using Codex:

```toml
[mcp_servers.canvas]
command = "C:\\path\\to\\canvas-mcp.exe"
args = []

[mcp_servers.canvas.env]
CANVAS_URL = "https://canvas.university.edu"
```

The first time you use it, ask your agent to run the authentication tool. This will open a browser window where you'll have to log into Canvas. Once you're logged in, just close the window.

Currently, all calls are read-only: I haven't gotten around to allowing the agent to submit assignments or send messages/make comments.
