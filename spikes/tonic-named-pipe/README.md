# Tonic over Windows named pipes

This executable proves the transport assumptions needed before SUSM adopts Tonic:

- Buf descriptor-set generation feeds `tonic-prost-build` without `protoc`;
- unary and server-streaming RPCs run over a byte-mode Windows named pipe;
- dropping a client stream cancels the HTTP/2 stream and drops the server response stream;
- the server creates the pipe with a protected DACL granting access only to LocalSystem and the owning user SID and rejects remote clients;
- the server authenticates the connected named-pipe client and exposes its SID and process ID through Tonic request extensions;
- the client reads the server process ID from the connected pipe before using the channel.

Run it from the repository root:

```powershell
mise exec -- cargo run -p tonic-named-pipe-spike
```
