# Remote Agents: Multi-Platform Job Execution

## Problem

Gauntlet Phase 1 runs everything locally — shell commands and Docker containers on the machine running `gauntlet run`. For a real CI system, we need:

- Jobs running on remote hosts (build farms, cloud VMs, self-hosted runners)
- Strong isolation between jobs (not just containers — VM-level)
- Multi-platform support (Linux, macOS, Windows)
- Many concurrent jobs per host
- A single agent process per host managing all of it

## Architecture

```
┌──────────────────────────────────────────────┐
│  Coordinator (tasked-server or gauntlet-server)│
│                                              │
│  Queues:                                     │
│    linux-x64      ─── jobs for Linux agents  │
│    macos-arm64    ─── jobs for macOS agents   │
│    windows-x64    ─── jobs for Windows agents │
│    default        ─── any platform            │
└──────────┬───────────┬───────────┬───────────┘
           │           │           │
     ┌─────▼─────┐ ┌──▼──────┐ ┌─▼──────────┐
     │ Linux     │ │ macOS   │ │ Windows    │
     │ Agent     │ │ Agent   │ │ Agent      │
     │           │ │         │ │            │
     │ Backend:  │ │ Backend:│ │ Backend:   │
     │ Firecracker│ │ Tart   │ │ Hyper-V    │
     │ (microVM) │ │ (VM)   │ │ (container)│
     │           │ │         │ │            │
     │ Fallback: │ │Fallback:│ │ Fallback:  │
     │ Docker    │ │ Docker  │ │ Docker/WSL │
     └───────────┘ └─────────┘ └────────────┘
```

### Components

**Coordinator** — tasked-server with platform-specific queues. Each queue has its own concurrency limits. The compiler targets jobs to the right queue based on `runs-on` labels. This already works — tasked has queues, concurrency, rate limits, retries.

**Agent** — a single long-running process per host. Pulls jobs from its queue, executes them in isolated environments, streams output back. The agent IS a tasked engine instance consuming from one or more queues.

**Isolation backend** — platform-specific. The agent delegates job execution to whichever backend is available on the host.

## Agent Design

```rust
// The agent is a thin wrapper around a tasked engine with
// a platform-specific executor registered.

struct Agent {
    engine: Arc<Engine>,
    backend: Arc<dyn IsolationBackend>,
}

#[async_trait]
trait IsolationBackend: Send + Sync {
    /// Boot an isolated environment, run the job, return output.
    async fn run_job(&self, spec: JobSpec) -> Result<JobResult, Error>;

    /// Kill a running job.
    async fn kill(&self, job_id: &str) -> Result<(), Error>;

    /// How many concurrent jobs this backend can handle.
    fn capacity(&self) -> usize;
}
```

### Platform Backends

| Backend | Platform | Isolation | Boot time | Concurrent jobs | Docker-in-job |
|---------|----------|-----------|-----------|-----------------|---------------|
| `FirecrackerBackend` | Linux | microVM (KVM) | ~125ms | Hundreds | Yes (via containerd inside VM) |
| `TartBackend` | macOS (Apple Silicon) | VM (Virtualization.framework) | ~3-5s | 4-8 (RAM bound) | No (Docker not available in macOS VMs) |
| `HyperVBackend` | Windows | Container/VM | ~1-2s | Depends on RAM | Yes (Windows containers) |
| `DockerBackend` | All | Container | ~1s | Dozens | Yes (DinD or bind mount) |

**DockerBackend** is the universal fallback — works everywhere Docker runs. Less isolation than VMs, but zero platform-specific code.

### Linux: Firecracker

The best story. Firecracker boots a microVM in ~125ms with:
- Full kernel isolation (separate kernel per job)
- Configurable CPU/memory per VM
- Network isolation via tap devices
- Disk I/O isolation via rate-limited block devices

For Docker-in-job support (needed for `docker build` in CI), use **firecracker-containerd**: an agent inside the VM runs containerd, so jobs can build and run containers normally.

A single Linux host with 64 cores / 256GB RAM could run 50+ concurrent CI jobs, each in its own microVM.

### macOS: Tart

macOS CI is constrained by Apple's Virtualization.framework:
- VMs are heavier (~3-5s boot, 2-4GB RAM each)
- Realistic concurrency: 4-8 VMs on a Mac Mini/Studio
- Pre-built macOS and Linux images available
- Used in production by Cirrus CI and GitLab

For macOS CI jobs (Xcode builds, Swift tests), this is the only real option. Docker isn't available inside macOS VMs — jobs that need Docker should target Linux agents.

### Windows: Hyper-V

Windows isolation options:
- **Hyper-V containers** — process isolation with kernel-level separation
- **WSL2** — Linux containers via Windows Subsystem for Linux
- **Full Hyper-V VMs** — heaviest, but strongest isolation

Most Windows CI jobs are .NET/MSBuild/PowerShell — they don't need VM-level isolation. Hyper-V containers or plain Docker containers are usually sufficient.

## Pipeline Format Changes

Add `runs-on` to target platforms:

```json
{
  "tasks": [
    {
      "id": "build-linux",
      "command": "cargo build --release",
      "runs-on": "linux-x64"
    },
    {
      "id": "build-macos",
      "command": "swift build",
      "runs-on": "macos-arm64"
    },
    {
      "id": "test-windows",
      "command": "dotnet test",
      "runs-on": "windows-x64"
    }
  ]
}
```

The compiler maps `runs-on` to a tasked queue name. If omitted, defaults to `default` (any available agent).

## Agent Registration & Health

Agents register with the coordinator on startup and maintain a heartbeat:

```
POST /agents/register
{
  "id": "agent-mac-mini-01",
  "platform": "macos-arm64",
  "backend": "tart",
  "capacity": 6,
  "labels": ["macos", "xcode-16", "apple-silicon"],
  "queues": ["macos-arm64", "default"]
}
```

The coordinator tracks agent health. If an agent misses heartbeats, its jobs are re-queued (tasked's timeout + retry handles this naturally).

## Implementation Phases

### Phase 2a: Docker Agent (all platforms)
- Agent binary that connects to tasked-server and consumes from a queue
- DockerBackend for job isolation (works everywhere)
- Output streaming back to coordinator
- `runs-on` support in pipeline format + compiler

### Phase 2b: Firecracker Agent (Linux)
- FirecrackerBackend using firecracker-containerd
- Pre-built rootfs images with common CI toolchains
- Network isolation via tap devices
- Disk snapshots for fast clone of base images

### Phase 2c: Tart Agent (macOS)
- TartBackend using Tart CLI or library
- Pre-built macOS + Linux VM images
- Xcode version management
- SSH-based command execution inside VMs

### Phase 2d: Agent management
- Registration, heartbeat, health checking
- Auto-scaling hints (agent reports capacity and load)
- Label-based routing (not just platform — also `gpu`, `large`, etc.)
- Agent groups / pools

## Open Questions

1. **Image management** — How do agents get job images? Pull from registry per-job? Pre-cache? Snapshot?
2. **Networking** — How do jobs access the internet? NAT per VM? Shared host network?
3. **Secrets delivery** — How do secrets get into isolated environments securely?
4. **Artifact transfer** — Local filesystem won't work for remote agents. S3/GCS? Built-in artifact server?
5. **Output streaming** — Real-time log streaming from agent → coordinator → user. WebSocket? SSE? Polling?
6. **Agent binary distribution** — Single binary per platform? Homebrew/apt/chocolatey?
