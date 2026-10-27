# SABnzbd API fixtures

Captured from a real SABnzbd, downloading for real from a stand-in Usenet
server, so the adapter in `crates/integrations/src/sabnzbd.rs` is tested
against what the client says.

- `probe.nzb`: one file, one segment, naming an article on the stand-in
  server: 300000 random bytes as `probe-disc.iso`, yEnc-encoded.
- `addfile-4.5.3.json`: `mode=addfile` answering the job's `nzo_id`.
- `queue-4.5.3.json`: `mode=queue` with one job waiting (the queue paused).
- `history-delete-4.5.3.json`: `mode=history&name=delete&value=<nzo_id>`
  with `del_files=0`, answering `{"status": true}` (as it does for an id it
  does not have). Recorded 2026-10-27; the entry went to SABnzbd's history
  archive, and `del_files=1` was found, by test and in its source
  (`_api_history`), to remove only a failed job's incomplete folder, never a
  completed job's files.
- `history-4.5.3.json`: `mode=history` with one job `Completed` (its
  `storage` the downloaded file; SABnzbd's deobfuscation renamed it
  `probe.iso`) and one `Failed` (an article the server does not have; its
  `storage` the incomplete folder, which must never be imported).

## Provenance

SABnzbd 4.5.3, image `lscr.io/linuxserver/sabnzbd:4.5.3`, captured
2026-10-25 on a loopback-only port, with no route to any real news server:

```bash
docker run -d --name sab -p 127.0.0.1:18081:8080 -v "$PWD/config:/config" \
  lscr.io/linuxserver/sabnzbd:4.5.3
```

The stand-in news server is a small Python NNTP responder serving one yEnc
article, run in SABnzbd's network namespace
(`docker run --network container:sab python:3.13-alpine ...`) and added as a
server at 127.0.0.1:1119. A category `tangible` with folder `tangible` was
added. The downloaded file was byte-identical to the one served.

Observed along the way, and relied on by the adapter:

- `mode=version` needs no key; every other mode answers 403
  `API Key Required` or `API Key Incorrect` without the right one. Every
  mode takes `apikey` in a POST body, so the key never goes in a URL.
- A host name not in `host_whitelist` is refused with 403
  `Hostname verification failed`; IP addresses are let in.
- `addfile` answers `{"status": true, "nzo_ids": [...]}`, or
  `{"status": false, "nzo_ids": []}` for a file that is not an NZB. The same
  NZB added twice becomes two jobs: SABnzbd does not deduplicate.
- A category SABnzbd does not have is silently replaced by `*`, the
  default, so the download lands outside the category's folder.
- An unknown `nzo_ids` filter answers empty slots in both queue and history.
