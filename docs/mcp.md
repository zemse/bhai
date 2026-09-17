# MCP

MCP is off until the global config turns it on:

```toml
mcp = true
```

Servers then start at launch and their names and one-line descriptions go in the system
prompt. `/mcp` prints what connected, how many tools each has, what the listing costs and
why anything failed.

## Deferred loading

Tool schemas stay out of the tool list, however many servers are connected. The model
finds a tool with `mcp_search` (keywords, a tool name or a server name), which returns
the matching schemas, and runs it with `mcp_call` under its exact `mcp__server__tool`
name. So the tool list and the cacheable prompt prefix stay the same size whether one
server is connected or ten.

Every `mcp_call` goes through the permission policy under that same name, so
`mcp__tavily__tavily_search` or `mcp__tavily` in a rule list decides it.

A server that only a child identity allows is not started at launch; it starts on that
child's first MCP call and is then shared with anyone else who is allowed it.

## Where servers are configured

Read lowest precedence first, a later definition replacing an earlier one of the same
name:

- `~/.claude.json`, the top-level `mcpServers` and then the current project's entry
- `<project>/.mcp.json`
- `[mcp.servers]` in `~/.config/bhai/config.toml`

Servers from a repo's `.mcp.json` are listed but not started until they are approved in
`~/.claude.json` (`enabledMcpjsonServers`, or `enableAllProjectMcpServers`), as Claude
Code asks for, and an unapproved repo entry never shadows a server of yours with the same
name. The project config file is not read for servers at all.

## Transports

A `command` is a stdio server, a `url` is a streamable http one:

```toml
[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { LOG_LEVEL = "info" }

[mcp.servers.tavily]
url = "https://mcp.tavily.com/mcp/"
headers = { Authorization = "Bearer ${TAVILY_API_KEY}" }
```

`${VAR}` and `${VAR:-default}` in a header value are expanded from the environment; a
variable that is not set leaves the server unstarted, with the header and the variable
named in `/mcp`. Header values are never printed.
