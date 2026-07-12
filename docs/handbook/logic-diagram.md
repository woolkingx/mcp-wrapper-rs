# Class Logic Swimlanes

Each lane is one class. The arrows are legal `Event { target, method, payload }` crossings; they do not imply direct access to another class's retained data. `payload` carries `sessionId`, `backendId`, `requestId`, and method-specific `data` whenever that address is needed.

## Client MCP call

```mermaid
sequenceDiagram
    participant CP as ClientPort
    participant W as McpWrapper
    participant M as Mcp
    participant S as SessionManager
    participant B as BackendManager
    participant BP as BackendPort

    CP->>W: Event(mcp.receive, payload)
    W->>M: mcp.receive(payload)
    M->>B: backend.resolve(payload)
    B-->>M: backendId
    M->>S: session.register(payload + backendId)
    M->>B: backend.ensure(backendId)
    B->>BP: connect/send(payload)
    BP-->>B: Event(backend.response, payload)
    B->>S: session.resolve(payload)
    S-->>B: sessionId + ClientPort
    B-->>W: Event(mcp.response, payload)
    W->>CP: send(sessionId, response)
```

`ClientPort` does not select a backend. `Mcp` asks `BackendManager` for the configured target, and `SessionManager` records the request relation before the backend can reply. The same path works when the client is stdio or SSE and when the backend is a child process or SSE endpoint.

## Wrapper tool call and status readback

```mermaid
sequenceDiagram
    participant CP as ClientPort
    participant W as McpWrapper
    participant M as Mcp
    participant B as BackendManager
    participant S as SessionManager

    CP->>W: Event(mcp.tools.call, payload)
    W->>M: mcp.onToolCall(payload)
    alt backend lifecycle action
        M->>B: backend.restart|stop|ensure(payload)
        B-->>M: BackendSnapshot
    else status action
        M->>B: backend.snapshot(backendId)
        M->>S: session.snapshot(sessionId|backendId)
        B-->>M: BackendSnapshot
        S-->>M: SessionSnapshot
    end
    M-->>W: Event(mcp.response, payload)
    W->>CP: send(sessionId, response)
```

`Mcp` validates the advertised tool schema but never edits backend or session state. `BackendManager` and `SessionManager` return snapshots; `Mcp` only projects those snapshots into the MCP response.

## Backend observation and cache refresh

```mermaid
sequenceDiagram
    participant BP as BackendPort
    participant B as BackendManager
    participant W as McpWrapper
    participant M as Mcp
    participant S as SessionManager
    participant CP as ClientPort

    BP-->>B: Event(backend.notification, payload)
    B-->>W: Event(mcp.backendChanged, payload)
    W->>M: mcp.backendChanged(payload)
    M->>B: backend.send(discovery refresh request)
    B->>BP: send(refresh request)
    BP-->>B: Event(backend.discovery, payload)
    B-->>W: Event(mcp.refresh, payload)
    W->>M: mcp.refresh(payload)
    M->>M: update catalog[backendId]
    M->>S: session.sessionsFor(backendId)
    S-->>M: affected sessionIds
    M-->>W: Event(mcp.listChanged, payload)
    W->>CP: send(sessionId, notification)
```

The backend notification is an observation, not cache truth. `Mcp` changes its catalog only after the backend refresh result arrives for the addressed `backendId`.

## Registry reload and topology reconciliation

```mermaid
sequenceDiagram
    participant M as Mcp
    participant W as McpWrapper
    participant B as BackendManager
    participant S as SessionManager
    participant BP as BackendPort

    M->>W: Event(backend.reloadRegistry, payload)
    W->>B: backend.reloadRegistry(payload)
    B->>B: validate registry.json against registry.schema.json
    B->>B: diff definitions, routes, hooks, and projections
    B->>S: session.activeCalls(affected backendIds)
    S-->>B: active-call readback
    alt safe to reconcile
        B->>BP: create, reconnect, or stop affected ports
        B->>B: replace registry, slots, and route data
        B-->>W: Event(mcp.registryChanged, payload)
        W->>M: mcp.registryChanged(payload)
        M->>M: rebuild affected catalogs and tool projection
    else active calls block destructive change
        B-->>W: Event(mcp.error, payload)
    end
```

`BackendManager` owns both the external registry and its live reconciliation. A registry reload cannot silently destroy an active backend call: it asks `SessionManager` for the runtime fact before changing the affected slot or port.
