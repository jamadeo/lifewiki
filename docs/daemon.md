# Design: a local inference daemon

## The problem, measured

Two identical `lw ask` runs against the local model:

```
real 58.06
real 59.05
```

No warm-up benefit, because the process exits and takes the loaded model with
it. The model is **17 GB** on disk and `goose-local-inference` already caches
loaded models in an `InferenceRuntime` keyed by model and settings — the cache is
just per-process. Its lifetime is the bug, not its absence.

Worth being precise about who pays this. An agent run makes **one `complete()`
call per turn**, and a build is typically 5–6 turns, so today the model is loaded
once per *command*, not once per turn. The 58s is one load amortised over the
whole run. A daemon does not make inference faster; it makes the load happen
once per *machine boot* instead of once per command.

## Shape

A background process holds the loaded model. The CLI sends it a request and
gets a reply. Same binary, hidden subcommand:

```
lw ask ──► ~/.lifewiki/daemon.sock ──► lw __daemon (holds the 17 GB model)
```

### Why the `Backend` trait is the right seam

```rust
pub trait Backend: Send + Sync {
    fn actor(&self) -> String;
    async fn complete(&self, system: &str, messages: &[Message], tools: &[Tool])
        -> Result<(Message, Usage)>;
}
```

Two methods, one of which is pure data in and out. Everything crossing the
boundary is **already `Serialize + Deserialize`**: `Message` and `Usage` from
`goose-provider-types`, `Tool` from `rmcp`. Nothing needs a wire format
inventing for it.

This matters more than it looks. The daemon runs *only* inference. The agent
loop, the tools, the wiki, the log, git — all stay in the CLI process. The
daemon never touches the wiki, so the S44 guarantee that a question cannot write
holds without re-proving. Adding a `DaemonBackend` is a sibling of
`LocalBackend`, and everything above it is unchanged.

### Transport: Unix domain socket

At `~/.lifewiki/daemon.sock`, with a length-prefixed JSON frame per message.

- Filesystem permissions do the access control. A TCP port on localhost is
  reachable by every process on the machine, including a browser; for something
  holding your entire life this is the wrong default.
- The socket file gives us liveness for free: `connect()` failing with
  `ECONNREFUSED` means a stale socket from a killed daemon.
- Requests are large (a full conversation, ~10 KB) and infrequent, so JSON's
  overhead is irrelevant next to a multi-second inference.

Not gRPC or an HTTP server: both are a dependency and a schema language for
what is one request type and one response type.

## Lifecycle

**Autostart on demand.** `lw ask --local` connects; on failure it forks
`lw __daemon`, waits for the socket, and retries. No `lw daemon start` to
remember, no launchd plist to install. The cost is that the first command after
a boot still pays the load, which is unavoidable.

**Singleton via the socket, not a pidfile.** The daemon binds the socket with an
exclusive `flock` on `daemon.lock` held for its lifetime. Two daemons racing to
start means one wins the lock and the other exits quietly. Pidfiles go stale
when a process is SIGKILLed and the pid is reused; a held flock cannot.

**No idle timeout.** The daemon runs until stopped. Stated plainly: **it holds
17 GB resident**, which on a 32 GB machine is half of memory held for a tool you
might use twice a day. `lw daemon stop` frees it. An idle timeout would bound
this automatically and remains the obvious next change.

**Version check on connect.** The daemon reports the binary version it was built
from; a mismatch makes the CLI kill and restart it. Otherwise `cargo install`
leaves an old daemon serving new requests, which would produce failures that
look like model problems.

## Protocol

```jsonc
// request
{"v": 1, "id": 7, "op": "complete",
 "model": "unsloth/gemma-4-26B-A4B-it-GGUF:Q4_K_M",
 "system": "...", "messages": [...], "tools": [...]}

// response
{"v": 1, "id": 7, "ok": {"message": {...}, "usage": {...}}}
{"v": 1, "id": 7, "err": "Context length exceeded: ..."}
```

Plus `{"op": "status"}` for `lw daemon status` and `{"op": "shutdown"}`.

`model` is per-request rather than fixed at daemon start, because `[build]` and
`[ask]` can name different local models. The daemon keeps a small cache and
loads on demand — which is exactly what `InferenceRuntime` already does, so this
is delegation rather than new logic.

**Concurrency: serialise requests.** llama.cpp's context is not shareable, and
`goose-local-inference` already takes a `cold_load_lock` during loading. One
in-flight request at a time, others queue. Two concurrent builds would each want
several GB of KV cache; queueing is both simpler and kinder to the machine.

## Failure modes

Each of these is a real thing that will happen, not a hypothetical:

| what happens | what the CLI does |
|---|---|
| No daemon running | start one, wait for the socket, retry |
| Stale socket file (daemon SIGKILLed) | `ECONNREFUSED` → unlink, start fresh |
| Daemon dies mid-request | connection drops → report it plainly, do not silently retry a half-done inference |
| Version mismatch after upgrade | shut it down, start the new one |
| Model not downloaded | daemon returns the error; CLI shows the `goose configure` message it shows today |
| Autostart fails (no `local` feature) | fall back to in-process, which is the current behaviour |
| Two CLIs at once | one holds the lock and serves both; requests queue |

**Deliberately not handled:** a daemon wedged but still accepting connections.
Detecting that needs heartbeats and a supervisor, which is more machinery than
the problem deserves. `lw daemon stop` is the escape hatch.

## What the user sees

```sh
lw ask "who is Ada?"          # autostarts, first call slow, rest instant
lw daemon status              # running, model, uptime, memory, requests served
lw daemon stop                # free the 17 GB now
lw ask --no-daemon "..."      # bypass, for debugging
```

Nothing changes for hosted providers: `--local` is what routes through the
daemon.

## Scope

Behind the existing `local` cargo feature. Roughly:

| piece | lines |
|---|---|
| `daemon.rs` — server, socket, idle timer, singleton | ~200 |
| `DaemonBackend` in `model.rs` — connect, autostart, retry | ~120 |
| `protocol.rs` — request/response types | ~60 |
| `lw daemon` subcommands | ~50 |
| tests | ~150 |

About **430 lines of code**, all `#[cfg(feature = "local")]`. Given we just cut
1,000 lines of bloat, that deserves justification: this removes a 58-second wait
from every single local command, which is the difference between the local
provider being usable and being a demo. That is a real capability, not a
convenience — and it is inert for anyone using a hosted provider.

## Testability

The parts likely to break are testable without a model:

- Protocol round-trips as pure serde tests.
- The server is generic over a `Backend`, so the whole socket path — autostart,
  singleton, version mismatch, idle exit, queueing — can be tested with
  `ScriptedBackend` and no GPU.
- Only "the real model stays loaded between calls" needs hardware, and that is
  one `#[ignore]`d test.

## Alternatives rejected

**A `goose` daemon instead of ours.** Right long-term home — goose owns model
management already — but `goose-local-inference` exposes no server today, and we
would be blocked on an upstream change. If one appears, `DaemonBackend` becomes
a client of theirs and everything above it is unchanged.

**mmap and rely on the page cache.** llama.cpp already mmaps; the 58s is
metadata parsing, tensor setup and Metal buffer allocation, none of which the
page cache helps with. Would not fix it.

**Preload at login (launchd).** Holds 17 GB from boot for a tool used
occasionally, and installing a plist is a much bigger intrusion than a process
that exits when idle.

**Keep the process alive after a command (a "warm" mode).** Just a daemon with a
worse interface.

## Open questions

1. **Is 15 minutes right, or should it be 0 (never exit) with an explicit
   `lw daemon stop`?** Depends whether you would rather reclaim memory or never
   wait twice. I lean 15 minutes, but you know your machine.
2. **Should `lw build --local` autostart too, or only `ask`?** A build is long
   enough that a one-off load matters less, and it is the command most likely to
   be run from a script where a lingering daemon is surprising.
3. **Should the daemon preload on start**, so the first `ask` is fast, or load
   on first request? Preloading makes autostart slower but the first real
   request fast.
4. **Is a 17 GB resident daemon acceptable at all**, or should this wait for a
   smaller default model? A 4 GB model would make the whole tradeoff easier.


---

## Outcome

Built, behind the `local` feature. Measured on an identical question:

```
warm (daemon)      8.00s
cold (--no-daemon) 21.62s
```

2.7x, and the daemon shows 17 GB resident across commands with the model loaded
once. `lw daemon status` reports what it is holding.

Verified by hand, since these are the parts that fail in practice:

| case | behaviour |
|---|---|
| No daemon | autostarts, first request loads the model |
| `SIGKILL`, stale socket left behind | `status` says not running; next command binds fresh |
| Second daemon started | loses the lock race and exits, original untouched |
| `lw daemon stop` | exits, socket removed |
| Hosted provider | no socket created; the daemon is only for `--local` |
| Daemon unreachable | falls back to in-process with a warning |

One measurement worth recording, because it nearly misled me: comparing whole
`lw build` runs showed 57s cold against 69s warm, which looks like the daemon
making things *slower*. It was variance in how much the model chose to generate,
not overhead. Comparing a fixed question is what showed the real effect.

The socket path (autostart, singleton, version mismatch, stale socket, error
propagation) is covered by seven tests using `ScriptedBackend`, so none of it
needs a GPU. Only "the real model stays loaded" needs hardware, and that was
verified by hand.
