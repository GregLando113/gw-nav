# Fileserver download protocol: Gw.exe vs `src/fileconn`

Date: 2026-10-06
Binary: `38974.Gw.exe` (Ghidra). Source paths in asserts: `Gw\Download\DnApi.cpp`, `Net\FileCli\FcApi.cpp`, `Net\FileCli\FcSrv.cpp`.

Why: we saw many socket closures while downloading. This doc lays out what the real client does on the wire and where our client differed. The server is not visible to us, so its reaction to each deviation is inferred, not observed.

> **Status (2026-10-06):** all items are addressed in `crates/gw-fileconn` (re-exported as `gw_nav::fileconn`). #1, #2, #4, #6 and #9 are fixed as described. #5 is handled by replacing connections idle for more than 240 s instead of sending keepalives. #3 and #7: `FileClient::download_many` keeps up to 32 requests in flight on one connection, in batches of 16, with the deferred and piggybacked ACKs from §3; the pathing store and the CLI use it with one connection per download. #8: hosts are tried from a random start, on 6112 and then on port 80, which serves the same protocol. Live check: the golden map files are byte-identical; 806 models came down in 8.4 s on one connection (2 min 38 s before pipelining), with no errors or reconnects. Sections 0–7 describe the code **before** the fix.

## 0. Summary of significant deviations

Ranked by how likely each one is to cause connection resets.

| # | Area | Gw.exe | `src/fileconn` | Risk |
|---|---|---|---|---|
| 1 | ACK value (action 7) | Payload bytes received since the last ACK | `0x4000` after every chunk, then the **whole file size** at the end | **High** |
| 2 | ACK timing | Only after ≥ window (`0x4000`) unacked bytes, plus the remainder at end of file | After every chunk except the last, plus once at the end | High (same root cause as #1) |
| 3 | Parallel connections | **One** file connection; extra connects only to probe latency, then closed | Up to 4 (store) or `--connections` (CLI), all to `file1` | Medium-high |
| 4 | Unexpected server packets | Actions 1 and 9 ignored; 15 (cancel) handled | Anything other than FileManifest/FileData is a fatal error and drops the connection | Medium |
| 5 | Keepalive | Action 8 sent after 300 s idle | None; idle pooled connections go stale | Medium (pool only) |
| 6 | Sequence byte | Real counter, checked on receive | Fixed `0xF2` then `0xF3` forever, not checked on receive | Low, works today |
| 7 | Pipelining | Up to 32 files in flight, 16 per request packet | One file at a time (stop-and-wait) | None (speed only) |
| 8 | Server choice | DNS `File1..12` in random order, lowest RTT wins; port 6112, then 80 | First host from `file1` that answers; port 6112 only | Low (adds load on `file1`) |
| 9 | Input limits | Packet ≤ `0x8004`, file sizes 1..`0x3F00000`, no overflow | No limits | Low (hardening) |

**#1 is the most likely culprit.** For any file that arrives in more than one chunk, our final ACK re-acknowledges bytes the earlier ACKs already covered. The server's count of unacknowledged bytes then goes below zero. A u32 counter would wrap and stall. A sanity check would reset the connection. Single-chunk files are ACKed correctly, so closures should cluster on or right after larger files. That is a cheap way to confirm.

## 1. Layering

```
DnApi_*   (Gw\Download\DnApi.cpp)    game-facing queue: priorities, flags, loading screen
  └─ FcApi_*  (Net\FileCli\FcApi.cpp)    tasks, dependency tables, archive store, dispatch
       └─ FcSrv_*  (Net\FileCli\FcSrv.cpp)  the socket: connect, packets, flow control
```

`DnApi_*` never touches the network. `DnApi_QueueDownloadRequest @8359a0` turns `DOWNLOAD_FLAG_*` into an FcApi priority byte and calls `FcApi_QueueFileRequest`. Everything in this doc comes from `FcSrv_*` (`0x7d6320`–`0x7d7bf4`) and the dispatch code in `FcApi_*`.

## 2. Packet framing

Every packet starts with a 4-byte header: `u8 seq, u8 action, u16 size`. `size` includes the header. All values are little endian.

The first byte is **a sequence number, not a "stage"** (see §4). Our `stage::HELLO/REQUEST/MORE` constants (`F1/F2/F3`) are just its first three values.

Receive side (`FcSrv_ProcessRecvData @7d7220`):
- `size < 4` or `size > 0x8004`: disconnect.
- `size > 0x5B4` (one MSS): the packet is reassembled into a `0x8004` byte buffer.
- `action > 15` or no handler: disconnect.
- A handler that returns 0: disconnect, then pick another server.

### Server → client handlers (table `@a96550`, indexed by action)

| Action | Handler | Body | Notes |
|---|---|---|---|
| 1 | `FcSrv_ValidatePacketSize8` | ≥ 4 B | Ignored |
| 2 | `FcSrv_HandleConnectResponse @7d7430` | 7 × u32 (size ≥ `0x20`) | ServerHello. `[6]` = Gw.exe id (falls back to `[2]` if 0), `[1]`, `[0]`. Marks the server connected and records its RTT. |
| 5 | `FcSrv_HandleFileHeader @7d7650` | `file_id, size_decompressed, size_compressed, crc` (size ≥ `0x14`) | Both sizes must be in `1..=0x3F00000`. A header while another file is still in progress is a protocol error. |
| 6 | `FcSrv_HandleFileData @7d74e0` | raw bytes | Must not overflow `size_compressed`. Updates the ACK counter (§3). |
| 9 | `FcSrv_ValidatePacketSize4` | any | Ignored (current connection only) |
| 15 | `FcSrv_HandleCancelRequest @7d7480` | `u32 file_id` (size ≥ 8) | `FcApi_CancelPendingTasks`: drops pending tasks from that file on and rewinds dispatch so they are requested again |
| 0, 3, 4, 7, 8, 10–14 | none | | **Disconnect.** Action 4 (our `NOT_FOUND`) has no handler: the real client never asks for files it does not know about. |

### Client → server packets

| Action | Size | Body | Sent by |
|---|---|---|---|
| hello | 21 | `01 00000000` + `[F1 00 1000] u32 1, u32 0, u32 0` | `FcSrv_TryConnect @7d6490` (identical to ours) |
| 3 | `4 + 8n` | n × `(u32 file_id, u32 version)`, n ≤ 16 | `FcSrv_SendFileRequest @7d7070` |
| 7 | 8 | `u32 bytes_since_last_ack` | `FcSrv_HandleFileData`, `FcSrv_SendFileRequest` (piggyback) |
| 8 | 4 | none | `FcSrv_Update @7d6940`: keepalive after 300 s idle |
| 15 | 4 | none | `FcSrv_SendHeartbeat @7d7020` (Ghidra name; it sends action 15, probably "cancel everything") |
| 16 | 12 | `u32 4, u32 window` | `FcSrv_UpdateWindowSize @7d7ba0`, `FcSrv_SelectBestServer`: sets the server's send window |

## 3. Flow control and ACKs

State per connection (`conn+…`):
- `0x4c` `unacked`: FileData payload bytes since the last ACK. Headers and FileHeader packets do not count.
- `0x74` `window_sent`: the window last told to the server.
- `0x78/0x7c` `window[mode]`: `0x4000` / `0x200` at connect. `mode` is `DAT_0108d224`, set by `FcApi_SetOptions` bit 31. The game uses mode 0.

`FcSrv_HandleFileData`, after copying a chunk:

```
unacked += payload_len
if file complete: deliver it (FcApi_OnFileDataReceived → may send the next request)
if active || file not complete:          // active = any file still pending
    if unacked < window[mode]: return    // no ACK yet
    bump seq (if caught up, §4)
send [seq, 7, 8] u32 unacked; unacked = 0
```

So:
- **Mid-file**, the client ACKs once per window (16 KiB), with the real byte count, and bumps the sequence.
- **At the end of a file with nothing else pending**, it ACKs the remainder without bumping the sequence.
- **At the end of a file with more pending**, it waits for a full window. Any remainder rides in front of the next file request: `FcSrv_SendFileRequest` prepends `[seq, 7, 8] u32 unacked` when `unacked != 0`.

The sum of all ACK values always equals the FileData payload bytes received. That is the invariant we break.

### What `client.rs` does

`request_file` (`src/fileconn/client.rs:236`–`253`):
1. After every FileData chunk except the last: `[F3, 7, 8] u32 0x4000` (`DATA_RATE`). The value does not depend on how much arrived.
2. After the last chunk: `[F3, 7, 8] u32 total`.

Example: a 40 000 B file sent as 16 384 + 16 384 + 7 232.

| | ACKs | Sum |
|---|---|---|
| Gw.exe | 16 384, 16 384, 7 232 | 40 000 |
| ours | 16 384, 16 384, **40 000** | 72 768 |

If chunks are smaller than 16 KiB, every mid-file ACK over-credits too. The old code comment at `client.rs:246` said the server stops after about 120 KB unless we send the end-of-file ACK. That matches real behavior: the remainder must be ACKed. Only the value is wrong. It should be the bytes not yet ACKed, not `total`.

### Adaptive window (optional to copy)

On every sequence bump the client times the round trip (`conn+0x70`). In `FcSrv_ProcessRecvData`, when a reply with the new seq arrives:
- 4 replies in a row within `rtt_limit[mode]` (`@a96544`: 10 000 ms / 500 ms) double the window, up to `0x8000`.
- 4 replies in a row slower than 2 × the limit halve it, down to `0x200`.
- Each change sends an action 16 packet.

In mode 0 the 10 s limit means the window grows to `0x8000` almost at once. We never send action 16, so the server keeps its default. If our ACKs are correct, that should be fine.

## 4. Sequence byte

`conn+0x64` = `send_seq`, `conn+0x65` = `recv_seq`. Both are set to `0xF1` on connect (`FcSrv_ConnectionCallback` case 1 writes `0xF1F1`).

- **Bump** (`FcSrv_IncrementSequence @7d7200`): if `send_seq == recv_seq`, then `send_seq += 1` (u8 wrap) and start the RTT timer. If the server has not caught up yet, nothing changes.
- Bumps happen on: every file request packet, and every mid-file ACK.
- No bump on: the end-of-file ACK, keepalive (8), cancel (15), window update (16).
- Every client packet carries the current `send_seq`.
- **Receive:** a packet's seq must equal `recv_seq` or `recv_seq + 1`. If it is `+1`, `recv_seq` advances. Anything else disconnects.

Ours sends `F2` on the first request and `F3` on everything after, so the server sees one bump and then the same value forever. The server accepts that, since the real client also sends packets without bumping. So this is not a closure cause today, but:
- we do not check the seq of received packets, so a desync looks like some other error;
- the code comment's explanation ("later requests must use `MORE`") is a side effect of this counter, not the rule.

## 5. Pipelining (`FcApi_DispatchPendingRequests @7d5330`)

- Up to **32** normal-priority files in flight (`DAT_0108d144 < 0x20`). A separate pool of 256 exists for the other request type.
- Requests are batched up to **16** per action-3 packet. A new batch starts only when at least 16 slots are free.
- When a file finishes (`FcApi_OnFileDataReceived`), the client dispatches more requests at once, so the pipe stays full.
- `FcSrv_SetActiveState(1/0)` follows "anything pending" (`FcApi_LinkTaskToPending` / `UnlinkTaskFromPending`). It changes the ACK rule (§3) and the idle timeout (§6).

We send one request and wait for the whole file before sending the next. Our pool of N connections is a stand-in for this pipelining, and it creates #3.

## 6. Connections, timeouts, server choice

| | Gw.exe (`FcSrv_*`) | ours |
|---|---|---|
| Hosts | `File1..File12` via `Base__DnsName__Format`, **shuffled at random**, DNS refreshed every 30 s | `file1..file12` in order, first that answers |
| Ports | `6112`, then `80` (`@a96530`) | `6112` |
| Connections | ≤ 5 concurrent connect attempts to probe RTT. Picks the lowest RTT (< 200 ms, or after every group has answered). Switches only if 50 ms better. **`FcSrv_DisconnectOthers` keeps exactly one.** | Pool of 4 (`MODEL_CONNECTIONS`) or `--connections`, all to the same first host |
| Reconnect backoff | A host is retried ≥ 10 s after it disconnected | Immediate (pool retry) |
| Keepalive | `[seq, 8, 4]` when 300 s have passed since the last packet received or the last selection (`FcSrv_Update`, polled every 300 ms) | none |
| Receive timeout | 30 s while files are pending, 360 s idle; then disconnect | 30 s read timeout |
| After an error | Disconnect, select the next best server, re-dispatch pending tasks | Reconnect to the same host (`FileClient` keeps `addr`); pool `reconnect` goes back to `file1` |

## 7. Recommended changes

In order of expected impact:

1. **Fix ACK accounting** (`client.rs` `request_file`). Keep `unacked += body.len()` per FileData chunk. Mid-file, send `ack(unacked)` only once `unacked >= 0x4000`, then reset it. At the end of the file, send `ack(unacked)` if it is non-zero. Remove `DATA_RATE`. The tests `expect_more`/`expect_complete` currently check for the wrong values and need updating.
2. **Tolerate actions 1 and 9** (skip them in the read loop). **Handle 15** as a per-file cancel: retry the request instead of tearing down the connection. Keep 4 = NotFound, since we see it in practice.
3. **Fewer connections, spread out.** Default the pool to 1–2, and/or pick hosts in random order as the client does, instead of always starting at `file1`. Longer term, add pipelining (several file ids in one action-3 packet) so one connection is enough.
4. **Idle connections in the pool.** Either send `[seq, 8, 4]` keepalives, or record a last-used time and reconnect connections idle for more than about 4 minutes before use.
5. **Track the sequence for real** (`send_seq`/`recv_seq`, bump rules from §4) and check incoming packets against it, so a desync shows up as a clear error.
6. **Hardening:** reject `size > 0x8004`, file sizes of 0 or `> 0x3F00000`, and data that overflows `size_compressed`.

## 8. Address index (build 38974)

| Address | Name | Role |
|---|---|---|
| `7d6490` | `FcSrv_TryConnect` | Builds the 21-byte hello, starts connects (≤ 5) |
| `7d67e0` | `FcSrv_ConnectionCallback` | Socket events: connect fail/ok, close, recv. Initializes seq `F1F1`, windows `0x4000/0x4000/0x200` |
| `7d6940` | `FcSrv_Update` | 300 ms tick: DNS, connect, keepalive (action 8), idle timeout |
| `7d7070` | `FcSrv_SendFileRequest` | Action 3, piggybacked ACK, seq bump |
| `7d7200` | `FcSrv_IncrementSequence` | Seq bump when caught up |
| `7d7220` | `FcSrv_ProcessRecvData` | Framing, seq check, adaptive window, dispatch |
| `7d74e0` | `FcSrv_HandleFileData` | Data copy, ACK rule |
| `7d7650` | `FcSrv_HandleFileHeader` | FileHeader checks |
| `7d77d0` | `FcSrv_SelectBestServer` | RTT-based choice, initial window packet |
| `7d7ba0` | `FcSrv_UpdateWindowSize` | Action 16 |
| `7d5330` | `FcApi_DispatchPendingRequests` | 32 in flight, 16 per packet |
| `a96530` | data | Ports `[6112, 80]`; `@a96544` RTT limits `[10000, 500]`; `@a96550` recv handler table |
