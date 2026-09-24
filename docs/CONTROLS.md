# Control-point compatibility matrix

ricercar implements the UPnP AV MediaRenderer device with:
`urn:schemas-upnp-org:service:AVTransport:1` (incl. `SetNextAVTransportURI`),
`RenderingControl:1`, `ConnectionManager:1`, GENA events, SSDP advertisement.

| Control point | Status | Notes |
|---|---|---|
| Own SOAP/HTTP test-suite (`cargo test -p ricercar-upnp`) | ✅ automated | SetURI→Play→SetNext→gapless→STOPPED, volume, GENA, device.xml |
| HTTP-streamed sources | ✅ automated | `http://` URLs decoded on the fly, bit-exact |
| BubbleUPnP | ⬜ untested | Expected to work; please report issues (open-source Android) |
| Symfonium | ⬜ untested | a streaming service/Tidal/… bit-perfect via "push to renderer" |
| Hi-Fi Cast / Linn Kazoo | ⬜ untested | |
| mConnect / Twonky "PlayTo" | ⬜ untested | |
| OpenHome (BubbleUPnP "Play using… OpenHome") | ⬜ not implemented | Planned 0.2 (`Product/Playlist/Info/Time/Volume`) |

Protocol-level features known NOT yet implemented (0.2 candidates):
- Range-request serving of library files (we are a renderer, not a server —
  no DMS yet)
- Multi-room / grouped playback
- DIDL `res` protocolInfo matching beyond accepting `http-get:*:*:*`

To add your app to this table, open an issue with the app version +
what worked/broke; we keep this matrix honest with real reports.
