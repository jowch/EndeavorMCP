# Remote MCP Connection Architecture

## Decision

EndeavorMCP will ship as one binary with a shared core and three explicitly bounded operating modes:

```text
endeavor-mcp serve --transport stdio
endeavor-mcp serve --transport streamable-http
endeavor-mcp connect --descriptor <endeavor-connection-url>
```

The binary is packaged by agent-harness plugins together with skills and harness-specific MCP configuration. The same runtime design is intended to work with Claude Code, Google Antigravity, Codex, and any client that supports local `stdio` MCP servers and/or Streamable HTTP MCP endpoints.

## Goals

- Run notebook-aware Endeavor capabilities on the machine that owns the relevant Pluto/Julia process, project, filesystem, data, and compute allocation.
- Support both local and remote projects through one installed Endeavor binary.
- Support remote jobs whose assigned host and port vary between launches.
- Use SSH port forwarding, including existing user SSH configuration such as `ProxyJump`, bastions, SSH agents, hardware keys, institutional authentication, and multiplexing.
- Keep agent-harness MCP configuration stable even when the remote Endeavor instance uses a different port on every launch.
- Require the user to explicitly identify the remote Endeavor instance; no scheduler discovery, port scanning, or implicit target selection is required.
- Avoid implementing a second notebook-management server locally or reimplementing MCP transport semantics unnecessarily.

## Architecture

```text
Remote host or compute allocation

  endeavor-mcp serve --transport streamable-http --bind 127.0.0.1:<remote-port>
      |
      | SSH forwarding, supervised locally
      v

Local workstation

  endeavor-mcp connect --descriptor <connection-url> --bind 127.0.0.1:8765
      |
      | stable local Streamable HTTP endpoint
      v

  http://127.0.0.1:8765/mcp
      |
      v

  Claude Code / Antigravity / Codex
```

The local `connect` mode is a connection supervisor, not another notebook-management instance. It owns SSH tunnel creation, validation, monitoring, status, and cleanup. The remote `serve` instance remains the sole MCP server with notebook, Julia, Pluto, project, filesystem, and compute authority.

## Operating Modes

| Command | Role | Capability authority | Transport |
|---|---|---|---|
| `endeavor-mcp serve --transport stdio` | Local MCP server | Local workstation project, filesystem, Julia, Pluto, and processes | stdio |
| `endeavor-mcp serve --transport streamable-http` | Remote MCP server | Remote project, filesystem, Julia, Pluto, scheduler allocation, and processes | Streamable HTTP |
| `endeavor-mcp connect --descriptor ...` | Local SSH connection supervisor | SSH forwarding only; it must not execute notebook tools or inspect a project merely because it shares the binary | Local loopback TCP forward to remote HTTP |

All modes share common code for the Endeavor tool implementation, configuration schema, connection descriptors, diagnostics, version compatibility, and authorization policy where applicable. The connector mode deliberately has a narrower authority boundary.

## Remote Connection Workflow

1. The user starts EndeavorMCP on the remote host or allocated compute node.

   ```bash
   endeavor-mcp serve \
     --transport streamable-http \
     --bind 127.0.0.1:0 \
     --path /mcp
   ```

2. The remote server chooses an available port and prints a human-readable connection instruction plus a machine-readable connection descriptor.

3. The user supplies that descriptor to a local agent or invokes `endeavor-mcp connect` directly. The user is the authority for selecting the remote target.

4. The local connector resolves the SSH profile through `~/.ssh/config`, opens a managed SSH local forward, and exposes a fixed loopback endpoint, for example `http://127.0.0.1:8765/mcp`.

5. The agent harness connects to that stable local endpoint as an ordinary remote HTTP MCP server.

6. If the remote Endeavor process is replaced, the tunnel is replaced and the harness reconnects/reinitializes the remote MCP server. A TCP tunnel cannot safely preserve an MCP session across a remote-server replacement.

## Connection Descriptor

A descriptor carries connection coordinates only. It should be easy for a user to copy or provide to an agent, but it must not carry bearer tokens, passwords, private-key paths, or shell fragments.

Illustrative URI form:

```text
endeavor://connect/v1?ssh=cluster&host=n2cn0216&port=41873&path=%2Fmcp
```

Illustrative JSON form:

```json
{
  "version": 1,
  "ssh_profile": "cluster",
  "target_host": "n2cn0216",
  "target_port": 41873,
  "mcp_path": "/mcp",
  "local_bind": "127.0.0.1:8765"
}
```

The `ssh_profile` is an SSH `Host` alias, not an attempt to model institutional topology in Endeavor. Existing SSH configuration owns details such as usernames, bastions, `ProxyJump`, `IdentityFile`, Kerberos/GSSAPI, SSH-agent use, FIDO keys, and keepalive defaults.

Example user-owned SSH configuration:

```sshconfig
Host cluster
    HostName login2.institute.edu
    User ada
    ProxyJump bastion.institute.edu
    ServerAliveInterval 30
    ServerAliveCountMax 3
```

The connector can then spawn an argument-array equivalent of:

```bash
ssh -N \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=3 \
  -L 127.0.0.1:8765:n2cn0216:41873 \
  cluster
```

Do not build this command with `sh -c` or interpolate unvalidated descriptor fields into a shell command.

## Stable Local Endpoint

The remote port may change every time EndeavorMCP is launched. The local port should be stable for a named connection, for example:

```text
http://127.0.0.1:8765/mcp
```

Agent harnesses are configured against the local endpoint once. `endeavor-mcp connect` changes the SSH forward destination when the user supplies a new descriptor; it does not require changing the harness configuration.

The connector must bind to `127.0.0.1` by default. It must fail clearly if the selected local port is in use rather than silently choosing a random alternative that disagrees with harness configuration.

## Harness Integration

The runtime contract is portable; plugin/configuration packaging is harness-specific. Each harness should expose two explicit MCP server names rather than silently switching a single `endeavor` identity between local and remote authority.

```text
endeavor-local   -> local `stdio` EndeavorMCP process
endeavor-remote  -> `http://127.0.0.1:8765/mcp`, forwarded to remote EndeavorMCP
```

The explicit split makes the execution context visible: the two endpoints can have different filesystems, Julia installations, package depots, resource limits, credentials, data mounts, and notebook/process ownership.

### Claude Code

A Claude Code plugin can bundle the binary, skills, hooks as needed, and MCP declarations. Conceptually:

```json
{
  "mcpServers": {
    "endeavor-local": {
      "command": "${CLAUDE_PLUGIN_ROOT}/bin/endeavor-mcp",
      "args": ["serve", "--transport", "stdio"]
    },
    "endeavor-remote": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

A connection skill accepts an explicit descriptor and invokes the bundled binary's `connect` mode. Depending on plugin lifecycle behavior, the user or skill may then enable/reconnect the remote MCP entry.

### Antigravity

Antigravity can use its `mcp_config.json` with one local command entry and one remote `serverUrl` entry:

```json
{
  "mcpServers": {
    "endeavor-local": {
      "command": "/absolute/path/to/endeavor-mcp",
      "args": ["serve", "--transport", "stdio"]
    },
    "endeavor-remote": {
      "serverUrl": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

The same local `connect` command establishes the tunnel before Antigravity connects or reconnects to `endeavor-remote`.

### Codex

Codex can use equivalent local command and HTTP MCP entries:

```toml
[mcp_servers.endeavor_local]
command = "/absolute/path/to/endeavor-mcp"
args = ["serve", "--transport", "stdio"]

[mcp_servers.endeavor_remote]
url = "http://127.0.0.1:8765/mcp"
```

Codex plugin packaging may bundle skills and MCP connections, but the portable invariant is still the static local URL plus the same `endeavor-mcp connect` workflow.

## Plugin Design

A plugin should package one Endeavor binary once, plus skills and thin harness-specific configuration adapters.

```text
plugin/
  bin/endeavor-mcp
  skills/
    endeavor-local/
    endeavor-remote/
  claude/
    .mcp.json
  antigravity/
    mcp_config.json.fragment
  codex/
    config.toml.fragment
```

The exact packaging manifest differs by harness. Do not make the Endeavor core or descriptor protocol depend on one plugin system.

## Lifecycle and Failure Semantics

The connector has a small explicit state machine:

```text
disconnected -> connecting -> forwarding -> unhealthy -> disconnected
```

It must:

- Validate descriptor scheme, version, host, port, path, and local bind address.
- Spawn OpenSSH directly with an argument vector.
- Use `ExitOnForwardFailure=yes`.
- Bind locally only to loopback unless the user explicitly overrides this.
- Report distinct failures for local-port conflicts, SSH authentication, jump-host routing, remote TCP connectivity, and MCP health.
- Track and clean up the precise child SSH process or process group it created.
- Probe the remote MCP endpoint through the tunnel before reporting a healthy connection.
- Treat replacement of the remote Endeavor instance as a new backend.

A broken tunnel to the same remote instance may be restarted. A new remote server process, host, port, or instance identity requires a client-side MCP reconnect/reinitialization. Do not silently route an existing MCP session to a different backend.

## Security Boundaries

- Bind the remote Streamable HTTP service to loopback by default and reach it through SSH.
- Bind the local tunnel endpoint to `127.0.0.1` by default.
- Do not place secrets in descriptors, terminal command lines, shell history, logs, repository configuration, or chat transcripts.
- Keep SSH authentication and MCP application authorization conceptually separate. An SSH tunnel provides network reachability; it does not automatically define all application-level authorization.
- Do not permit descriptor fields to become arbitrary SSH options or shell fragments.
- Keep connector mode narrowly scoped to connection management. It should not obtain notebook or local project authority merely because it is part of the Endeavor binary.

## Initial Scope

Implement the smallest reliable version first:

1. One Endeavor binary with `serve --transport stdio`, `serve --transport streamable-http`, and `connect --descriptor` modes.
2. User-supplied connection descriptor; no scheduler integration, discovery, or port scanning.
3. One fixed local loopback port per configured remote connection, initially `127.0.0.1:8765`.
4. OpenSSH forwarding through an SSH config profile.
5. Explicit remote MCP reconnect after a remote instance replacement.
6. Separate `endeavor-local` and `endeavor-remote` names in each harness.
7. Claude Code packaging first, followed by thin Antigravity and Codex configuration adapters.

## Deferred Work

Do not make these requirements for the first version:

- Scheduler-aware job discovery or automatic node/port resolution.
- Port scanning.
- Transparent migration of a live MCP session between remote server instances.
- A general local HTTP reverse proxy that multiplexes several remote instances behind one endpoint.
- Dynamic, arbitrary remote tool mirroring through a second MCP server.
- A hidden automatic switch between local and remote project authority.

These may become useful later, but they introduce substantial lifecycle, transport, security, and user-context complexity without being necessary for the explicit user-driven workflow.
