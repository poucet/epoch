# Mounting the epoch FUSE spike

Two ways to run the spike, for different purposes — keep both.

| | `./docker-run.sh` | `./mount-mac.sh` |
|---|---|---|
| FUSE driver | Linux in-kernel `/dev/fuse` | macFUSE |
| Where the mount lives | inside the container's Linux VM | your Mac, at `~/epoch` |
| Visible to Mac apps/IDEs | no (it's in the VM) | yes — a real `$HOME` path |
| Use it for | quick Linux-parity testing | actually using the mount on the laptop |

## Why a container mount can't be seen from macOS

Docker/OrbStack file sharing only flows **host → container** (the `-v $PWD:/work:ro`
bind). A FUSE mount created *inside* the container is a kernel mount in the Linux
VM's mount namespace; macOS has no path into it. So to use the mount on the Mac
we mount natively with macFUSE instead.

## Why not FUSE-T (the kext-less option)

FUSE-T mounts without a kernel extension, which would avoid the approval below.
It links and even mounts, but **`fuser` 0.17 is protocol-incompatible with it**:
fuser hard-compiles the macFUSE 4.x wire layout (`macfuse-4-compat`) and rejects
FUSE-T's requests right after INIT (`"Invalid request"`). Upstream issue
[cberner/fuser#273](https://github.com/cberner/fuser/issues/273) is open and
unresolved. Until that changes (or we fork fuser), macFUSE is the native path.

## One-time: enable the macFUSE kernel extension (Apple Silicon)

macFUSE is installed, but Apple requires you to authorize its kext once. Two parts:

**1. Lower the security policy in Recovery**
1. Shut down the Mac (Apple menu → Shut Down).
2. Press and **hold the power button** until "Loading startup options" appears.
3. Click **Options → Continue**; pick an admin user and enter the password.
4. Menu bar → **Utilities → Startup Security Utility**.
5. Select your system disk → **Security Policy…**
6. Choose **Reduced Security** and tick **"Allow user management of kernel
   extensions from identified developers."** Authenticate, then **restart**.

**2. Approve macFUSE and reboot**
1. Back in macOS, open **System Settings → Privacy & Security**, scroll to the
   bottom. A message says system software from **"Benjamin Fleischer"** (the
   macFUSE signer) was blocked → click **Allow**. (If it isn't shown, run
   `./mount-mac.sh` once to trigger the block, or open the macFUSE pane.)
2. **Reboot** when prompted. macFUSE now loads on every mount.

> "Reduced Security" is Apple's requirement for *any* third-party kext on Apple
> Silicon. It's reversible in Recovery (back to Full Security), but macFUSE will
> stop loading if you do.

## Native macOS mount

```sh
./mount-mac.sh                 # lower = this repo, mount = ~/epoch
./mount-mac.sh /some/dir       # custom lower dir
./mount-mac.sh /some/dir ~/mnt # custom lower dir + mountpoint
# or: EPOCH_LOWER=… EPOCH_MOUNT=… ./mount-mac.sh
```

Builds `epoch-fuse`, mounts, and unmounts on `Ctrl-C` or exit. Two modes:

- **Default** — the read-only overlay probe: reads through to the lower dir,
  `EROFS` on any write.
- **`EPOCH_STATE=<dir>`** — the durable, checkpointed copy-on-write overlay:
  writes/deletes are captured, checkpointed (debounced), persisted in
  `<dir>/epoch.redb`, resumed on remount, and time-travelable under
  `<mount>/.vfs/history/`. Also `EPOCH_HEAD`, `EPOCH_HARD_FSYNC=1`. See
  `docs/TIMELINE.md`.

If you see `Operation not permitted`, the kext approval above isn't complete.

## Linux-parity testing

```sh
./docker-run.sh          # Zellij split: shell + live `epoch /work /mnt/epoch`
./docker-run.sh zsh      # plain shell
```

Runs the same binary against Linux's in-kernel FUSE — no macFUSE involved. Best
for exercising the real `/dev/fuse` path the daemon will use in production.
